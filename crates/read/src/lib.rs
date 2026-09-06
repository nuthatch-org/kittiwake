//! The read layer: cached, coalesced, provenance-bearing reads of the nests.
//!
//! One function does the thinking ([`ReadCtx::cached`]) and the routes are thin descriptions of a
//! query and a shape. That is deliberate: freshness, readiness, coalescing and staleness labelling
//! are properties of *reading a nest*, not of any particular panel, and the previous service put
//! them at each call site where they drifted.

pub mod sql;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kittiwake_cache::{CachePolicy, Entry, Freshness, Store, classify};
use kittiwake_nest::{Lane, NestGate, QuerySpec};
use kittiwake_types::{ApiError, Meta, NestId, Provenance, ServedFrom};

/// What a route hands the read layer: a way to go and get the answer, once.
pub type Fetcher = Arc<
    dyn Fn(NestGate, Lane) -> BoxFuture<'static, Result<(serde_json::Value, Provenance), ApiError>>
        + Send
        + Sync,
>;

/// Everything a read needs. Cheap to clone.
#[derive(Clone)]
pub struct ReadCtx {
    pub gate: NestGate,
    pub store: Store,
    pub schema_version: u32,
}

/// A cached answer and the record of how it was delivered.
pub struct Read {
    pub entry: Arc<Entry>,
    pub meta: Meta,
}

impl ReadCtx {
    pub fn new(gate: NestGate, store: Store, schema_version: u32) -> Self {
        Self {
            gate,
            store,
            schema_version,
        }
    }

    /// Read one route's answer, honouring its freshness policy.
    ///
    /// The three outcomes, in the order they are checked:
    ///
    /// - **fresh in cache** — served, and the nest is not troubled at all;
    /// - **stale in cache** — served immediately *labelled stale*, with a refresh running behind
    ///   the response, because a reader who is told the age of a number can decide for themselves,
    ///   whereas one who is not has been misled;
    /// - **absent or expired** — the nest is asked, coalesced with everyone else waiting for the
    ///   same thing, after checking the nest says it is ready.
    pub async fn cached(
        &self,
        route: &'static str,
        nest: NestId,
        params: serde_json::Value,
        policy: CachePolicy,
        fetch: Fetcher,
    ) -> Result<Read, ApiError> {
        let key = kittiwake_cache::key(self.schema_version, nest.as_str(), route, &params);

        if let Some(entry) = self.store.get(&key).await {
            match classify(entry.age(), &policy) {
                Freshness::Fresh => {
                    metrics::counter!("read_cache_total", "route" => route, "outcome" => "fresh")
                        .increment(1);
                    let meta = entry.meta(ServedFrom::Cache, false);
                    return Ok(Read { entry, meta });
                }
                Freshness::Stale => {
                    metrics::counter!("read_cache_total", "route" => route, "outcome" => "stale")
                        .increment(1);
                    // Refresh behind the response, on the background lane so a revalidation can
                    // never take the permit a waiting reader needs.
                    self.spawn_refresh(key.clone(), route, nest, fetch.clone());
                    let meta = entry.meta(ServedFrom::Cache, true);
                    return Ok(Read { entry, meta });
                }
                Freshness::Expired => {
                    metrics::counter!("read_cache_total", "route" => route, "outcome" => "expired")
                        .increment(1);
                }
            }
        } else {
            metrics::counter!("read_cache_total", "route" => route, "outcome" => "miss")
                .increment(1);
        }

        let entry = self.fetch_and_store(&key, nest, fetch, Lane::Read).await?;
        let meta = entry.meta(ServedFrom::Nest, false);
        Ok(Read { entry, meta })
    }

    fn spawn_refresh(&self, key: String, route: &'static str, nest: NestId, fetch: Fetcher) {
        let ctx = self.clone();
        tokio::spawn(async move {
            if let Err(e) = ctx.fetch_and_store(&key, nest, fetch, Lane::Cron).await {
                // A failed revalidation is not a failed request: the reader already has the stale
                // answer and was told it was stale. Worth a line, not an alarm.
                tracing::debug!(route, error = %e, "background revalidation failed");
                metrics::counter!("read_revalidate_failed_total", "route" => route).increment(1);
            }
        });
    }

    async fn fetch_and_store(
        &self,
        key: &str,
        nest: NestId,
        fetch: Fetcher,
        lane: Lane,
    ) -> Result<Arc<Entry>, ApiError> {
        // A serving route consults the nest's own judgement before answering. An archive is
        // exempt: it is stalled by construction and its provenance says where its data ends.
        if !nest.is_archival() {
            self.gate.ready(nest).await.map_err(ApiError::from)?;
        }
        let (value, provenance) = fetch(self.gate.clone(), lane).await?;
        Ok(self
            .store
            .insert(key.to_string(), Entry::new(value, provenance))
            .await)
    }
}

/// Build a fetcher that runs one query and shapes its rows.
pub fn one_query(
    nest: NestId,
    sql: String,
    timeout: Duration,
    shape: fn(&[serde_json::Value]) -> serde_json::Value,
) -> Fetcher {
    Arc::new(move |gate: NestGate, lane: Lane| {
        let sql = sql.clone();
        Box::pin(async move {
            let mut spec = QuerySpec::read(nest, sql).with_timeout(timeout);
            spec.lane = lane;
            // Coalesce on the statement itself: two routes that happen to ask the same question
            // should cost the nest one fold, not two.
            let key = format!(
                "{}:{}",
                nest.as_str(),
                blake3::hash(spec.sql.as_bytes()).to_hex()
            );
            let answer = gate
                .query_coalesced(&key, spec)
                .await
                .map_err(ApiError::from)?;
            Ok((shape(&answer.rows), answer.provenance.clone()))
        })
    })
}

pub mod routes {
    use super::*;

    /// Rows through unchanged, as an array. The nest already aliases its columns to the names the
    /// frontend reads.
    fn rows_as_array(rows: &[serde_json::Value]) -> serde_json::Value {
        serde_json::Value::Array(rows.to_vec())
    }

    /// The first row, or null. For the single-row folds.
    fn first_row(rows: &[serde_json::Value]) -> serde_json::Value {
        rows.first().cloned().unwrap_or(serde_json::Value::Null)
    }

    pub struct IndexersParams {
        pub first: i64,
        pub skip: i64,
        pub order_by: String,
        pub descending: bool,
    }

    /// `/api/indexers`. Cold on the nest this is a whole-history fold measured at 23.5 seconds,
    /// which is exactly why it is the first thing the warmer keeps hot.
    pub async fn indexers(ctx: &ReadCtx, p: IndexersParams) -> Result<Read, ApiError> {
        let params = serde_json::json!({
            "first": p.first, "skip": p.skip,
            "orderBy": p.order_by, "desc": p.descending
        });
        let sql = sql::indexers_sql(p.first, p.skip, &p.order_by, p.descending);
        ctx.cached(
            "indexers",
            NestId::Alloc,
            params,
            CachePolicy::from_poll_interval(Duration::from_secs(60)),
            one_query(NestId::Alloc, sql, Duration::from_secs(45), rows_as_array),
        )
        .await
    }

    /// `/api/epochs`.
    pub async fn epochs(ctx: &ReadCtx, first: i64) -> Result<Read, ApiError> {
        ctx.cached(
            "epochs",
            NestId::Alloc,
            serde_json::json!({ "first": first }),
            CachePolicy::from_poll_interval(Duration::from_secs(300)),
            one_query(
                NestId::Alloc,
                sql::epochs_sql(first),
                Duration::from_secs(30),
                rows_as_array,
            ),
        )
        .await
    }

    /// `/api/network-stats`. Two folds, one answer; both are coalesced independently, so a
    /// concurrent `/api/grt-flow` asking the same params fold pays nothing for it.
    pub async fn network_stats(ctx: &ReadCtx) -> Result<Read, ApiError> {
        let fetch: Fetcher = Arc::new(|gate: NestGate, lane: Lane| {
            Box::pin(async move {
                let mut net = QuerySpec::read(NestId::Alloc, sql::network_sql());
                let mut par = QuerySpec::read(NestId::Alloc, sql::network_params_sql());
                net.lane = lane;
                par.lane = lane;

                let (net, par) = tokio::join!(
                    gate.query_coalesced("alloc:network", net),
                    gate.query_coalesced("alloc:network-params", par),
                );
                let net = net.map_err(ApiError::from)?;
                let par = par.map_err(ApiError::from)?;

                let value = serde_json::json!({
                    "network": first_row(&net.rows),
                    "params": first_row(&par.rows),
                });
                // Both folds come off the same nest at the same moment; report the more
                // conservative of the two so the pair is never dated later than its oldest half.
                let provenance = older_of(&net.provenance, &par.provenance);
                Ok((value, provenance))
            })
        });

        ctx.cached(
            "network-stats",
            NestId::Alloc,
            serde_json::json!({}),
            CachePolicy::from_poll_interval(Duration::from_secs(60)),
            fetch,
        )
        .await
    }

    /// The older of two provenances, so a composed answer is dated by its stalest part rather than
    /// its freshest. Claiming the newer would be flattering and wrong.
    fn older_of(a: &Provenance, b: &Provenance) -> Provenance {
        let pick_older = |x: Option<i64>, y: Option<i64>| match (x, y) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (v, None) | (None, v) => v,
        };
        Provenance {
            as_of: pick_older(a.as_of, b.as_of),
            sealed_through: pick_older(a.sealed_through, b.sealed_through),
            registry_hash: a.registry_hash.clone().or_else(|| b.registry_hash.clone()),
            nid: a.nid.clone().or_else(|| b.nid.clone()),
            source: a.source.clone().or_else(|| b.source.clone()),
            truncated: a.truncated || b.truncated,
            degraded: a.degraded || b.degraded,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_composed_answer_is_dated_by_its_stalest_half() {
            let fresh = Provenance {
                as_of: Some(500),
                sealed_through: Some(490),
                ..Default::default()
            };
            let old = Provenance {
                as_of: Some(400),
                sealed_through: Some(395),
                degraded: true,
                ..Default::default()
            };
            let merged = older_of(&fresh, &old);
            assert_eq!(merged.as_of, Some(400));
            assert_eq!(merged.sealed_through, Some(395));
            assert!(
                merged.degraded,
                "a degraded half makes the whole answer degraded"
            );
        }
    }
}
