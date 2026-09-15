//! The read layer: cached, coalesced, provenance-bearing reads of the nests.
//!
//! One function does the thinking ([`ReadCtx::cached`]) and the routes are thin descriptions of a
//! query and a shape. That is deliberate: freshness, readiness, coalescing and staleness labelling
//! are properties of *reading a nest*, not of any particular panel, and the previous service put
//! them at each call site where they drifted.

pub mod census;
pub mod concentration;
pub mod dips_agreements;
pub mod indexing_status;
pub mod manifest;
pub mod naming;
pub mod preflight;
pub mod qos;
pub mod recommend;
pub mod sql;
pub mod status_probe;

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
    /// Absent when no database is configured, and the indexer page is then served without names.
    pub naming: Option<naming::Naming>,
    /// Absent when no Arbitrum RPC is configured. The routes that need a contract read say so
    /// rather than quietly omitting the part they could not compute.
    pub chain: Option<kittiwake_chain::Chain>,
}

/// A source that is not a nest: it has no provenance of its own, so the age of the cache entry is
/// the whole of what we can say about how current it is.
pub type ExternalFetcher =
    Arc<dyn Fn() -> BoxFuture<'static, Result<serde_json::Value, ApiError>> + Send + Sync>;

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
            naming: None,
            chain: None,
        }
    }

    pub fn with_naming(mut self, naming: Option<naming::Naming>) -> Self {
        self.naming = naming;
        self
    }

    pub fn with_chain(mut self, chain: Option<kittiwake_chain::Chain>) -> Self {
        self.chain = chain;
        self
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

    /// Read something that is not a nest: a price, a TVL figure.
    ///
    /// No readiness check, because there is nothing to ask; no nest provenance, because there is
    /// none to carry. The freshness rules are otherwise identical, including that a failure is
    /// reported rather than papered over with the last known value.
    pub async fn cached_external(
        &self,
        route: &'static str,
        poll: Duration,
        fresh_from: ServedFrom,
        fetch: ExternalFetcher,
    ) -> Result<Read, ApiError> {
        self.cached_external_keyed(route, route, poll, fresh_from, fetch)
            .await
    }

    /// As [`Self::cached_external`], but with a per-subject key: one entry per indexer rather than
    /// one for the whole route.
    /// `fresh_from` is where a *miss* gets its answer. A hit reports `Cache`, because it came from
    /// the cache: this path used to answer `Chain` in all three branches, so `/api/price` claimed
    /// to have just read the chain while serving a value minutes old.
    pub async fn cached_external_keyed(
        &self,
        route: &'static str,
        subject: &str,
        poll: Duration,
        fresh_from: ServedFrom,
        fetch: ExternalFetcher,
    ) -> Result<Read, ApiError> {
        let policy = CachePolicy::from_poll_interval(poll);
        let key = kittiwake_cache::key(
            self.schema_version,
            "ext",
            route,
            &serde_json::json!({ "subject": subject }),
        );

        if let Some(entry) = self.store.get(&key).await {
            match classify(entry.age(), &policy) {
                Freshness::Fresh => {
                    let meta = entry.meta(ServedFrom::Cache, false);
                    return Ok(Read { entry, meta });
                }
                Freshness::Stale => {
                    let ctx = self.clone();
                    let key2 = key.clone();
                    let f = fetch.clone();
                    tokio::spawn(async move {
                        if let Ok(v) = f().await {
                            ctx.store
                                .insert(key2, Entry::new(v, Provenance::default()))
                                .await;
                        }
                    });
                    let meta = entry.meta(ServedFrom::Cache, true);
                    return Ok(Read { entry, meta });
                }
                Freshness::Expired => {}
            }
        }

        let value = fetch().await?;
        let entry = self
            .store
            .insert(key, Entry::new(value, Provenance::default()))
            .await;
        let meta = entry.meta(fresh_from, false);
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

pub mod disassembly;
pub mod routes;
pub mod search;
pub mod shape;
