//! Edge & Node's gateway QoS per indexer, and the QoS Quality score computed over it.
//!
//! Closed days come from Postgres, where `ingest-qos-days` keeps them; only today is asked of
//! qos-reo-nest, one day per statement, because the nest cannot answer more inside its memory budget.
//! Of the nest, only the contract views are read: `qos_indexer_daily`, `qos_allocation_daily`,
//! `qos_indexer_seconds_behind` and `qos_freshness`. `qos_deployment_daily` has no reader: served
//! share is attempts over every indexer's attempts, not over the gateway's query count.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kittiwake_cache::{CachePolicy, Entry};
use kittiwake_db::{QosAllocationDay, QosIndexerDay};
use kittiwake_qos::aggregate::{
    AggregateOpts, AllocationRow, Phase2Metric, QosDailyRow, aggregate_indexer_metrics,
    compute_phase2_metrics,
};
use kittiwake_qos::nest::{AllocationDaily, GRAPH_EPOCH_DAYS, day_number};
use kittiwake_qos::score::{
    DeploymentContribution, QualityDetail, QualityOpts, compute_quality_detail, qos_grade,
};
use kittiwake_types::{ApiError, Meta, NestId, ServedFrom};
use serde_json::{Value, json};

use crate::routes::{Part, many};
use crate::{Fetcher, Read, ReadCtx, shape, sql};

/// The score's window, as the incumbent's cron and its deployment breakdown both used it.
pub const SCORE_WINDOW_DAYS: i64 = 30;

/// Days of score history the panel draws.
pub const SCORE_HISTORY_DAYS: i64 = 30;

/// The oracle posts every five minutes and its documents resolve about half an hour later, so a
/// fresher cache would only re-ask for the same buckets.
const POLL: Duration = Duration::from_secs(300);

const TIMEOUT: Duration = Duration::from_secs(45);

/// Where closed QoS days are kept. Postgres in production; a map in the tests.
pub trait QosStore: Send + Sync {
    fn indexer_days<'a>(
        &'a self,
        addr: &'a str,
        from: i64,
        to: i64,
    ) -> BoxFuture<'a, Result<Vec<QosIndexerDay>, ApiError>>;
    fn cohort_days<'a>(
        &'a self,
        addr: &'a str,
        from: i64,
        to: i64,
        also: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>>;
    fn indexer_allocation_days<'a>(
        &'a self,
        addr: &'a str,
        from: i64,
        to: i64,
    ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>>;
    /// The days from `from` to `to` that have been computed, whether or not anyone was measured on them.
    fn stored_days(&self, from: i64, to: i64) -> BoxFuture<'_, Result<Vec<i64>, ApiError>>;
}

fn database(e: kittiwake_db::DbError) -> ApiError {
    ApiError::Unavailable(format!("database: {e}"))
}

impl QosStore for kittiwake_db::Pool {
    fn indexer_days<'a>(
        &'a self,
        addr: &'a str,
        from: i64,
        to: i64,
    ) -> BoxFuture<'a, Result<Vec<QosIndexerDay>, ApiError>> {
        Box::pin(async move {
            kittiwake_db::qos_indexer_days(self, addr, from, to)
                .await
                .map_err(database)
        })
    }
    fn cohort_days<'a>(
        &'a self,
        addr: &'a str,
        from: i64,
        to: i64,
        also: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>> {
        Box::pin(async move {
            kittiwake_db::qos_cohort_days(self, addr, from, to, also)
                .await
                .map_err(database)
        })
    }
    fn indexer_allocation_days<'a>(
        &'a self,
        addr: &'a str,
        from: i64,
        to: i64,
    ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>> {
        Box::pin(async move {
            kittiwake_db::qos_indexer_allocation_days(self, addr, from, to)
                .await
                .map_err(database)
        })
    }
    fn stored_days(&self, from: i64, to: i64) -> BoxFuture<'_, Result<Vec<i64>, ApiError>> {
        Box::pin(async move {
            Ok(kittiwake_db::qos_day_states(self, from, to)
                .await
                .map_err(database)?
                .into_iter()
                .map(|s| s.day)
                .collect())
        })
    }
}

/// Without the database there are no closed days, and asking the nest for them instead is exactly
/// the multi-day statement it refuses.
fn store_or_unavailable(store: Option<&dyn QosStore>) -> Result<&dyn QosStore, ApiError> {
    store.ok_or_else(|| {
        ApiError::Unavailable(
            "no database is configured: closed QoS days are kept there, and the nest answers one day \
             at a time"
                .into(),
        )
    })
}

fn f(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A figure, or null. Never zero for something that was not measured.
fn num(x: Option<f64>) -> Value {
    x.and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn unix_day_date(day: i64) -> String {
    chrono::DateTime::from_timestamp(day * 86_400, 0)
        .map(|d| d.date_naive().to_string())
        .unwrap_or_default()
}

fn graph_day_date(day: i64) -> String {
    unix_day_date(day + GRAPH_EPOCH_DAYS)
}

/// `many`, then a fold. A truncated answer is refused rather than folded: every statement here is
/// sized to fit under the row cap, so truncation means rows are missing, and a score over missing
/// peers is a different score that looks like the right one.
fn folded(parts: Vec<Part>, fold: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Fetcher {
    let inner = many(parts, TIMEOUT);
    let fold = Arc::new(fold);
    Arc::new(move |gate, lane| {
        let inner = inner.clone();
        let fold = fold.clone();
        Box::pin(async move {
            let (v, p) = inner(gate, lane).await?;
            if p.truncated {
                return Err(ApiError::Internal(
                    "the QoS nest truncated a statement sized to fit its row cap".into(),
                ));
            }
            Ok((fold(&v), p))
        })
    })
}

fn bad_address() -> ApiError {
    ApiError::BadRequest("not a valid address".into())
}

/// How long a live read may be served stale. `kittiwake-warmer` refreshes each one before this runs
/// out, and the oracle's documents resolve half an hour behind the chain in any case.
pub const LIVE_STALE_FOR: Duration = Duration::from_secs(2 * 60 * 60);

/// One live read: its cache key and the statements behind it. The routes and the warmer build them
/// in the same place, so the warmer fills exactly the entries the routes read.
struct LiveRead {
    route: &'static str,
    nest: NestId,
    params: Value,
    parts: Vec<Part>,
}

impl LiveRead {
    async fn read(self, ctx: &ReadCtx) -> Result<Read, ApiError> {
        ctx.cached(
            self.route,
            self.nest,
            self.params,
            CachePolicy::from_poll_interval(POLL).kept_warm(LIVE_STALE_FOR),
            folded(self.parts, Value::clone),
        )
        .await
    }
}

/// A live read, or `None` when the nest could not give it. Closed days are in Postgres, so a refused
/// statement costs the answer the figures it carried, never the route.
async fn live(ctx: &ReadCtx, read: LiveRead) -> Option<Read> {
    let route = read.route;
    read.read(ctx)
        .await
        .inspect_err(
            |e| tracing::warn!(route, error = %e, "live QoS read failed; its figures are left out"),
        )
        .ok()
}

/// Today's reads behind the Query Performance card: its figures, seconds behind, and freshness.
fn today_reads(addr: &str, today: i64) -> Result<[LiveRead; 3], ApiError> {
    let params = json!({"address": addr, "day": today});
    Ok([
        LiveRead {
            route: "indexer-qos-today",
            nest: NestId::Qos,
            params: params.clone(),
            parts: vec![
                Part::rows(
                    "days",
                    NestId::Qos,
                    sql::qos_indexer_daily_sql(addr, today).ok_or_else(bad_address)?,
                ),
                Part::rows(
                    "gateways",
                    NestId::Qos,
                    sql::qos_indexer_gateways_sql(addr, today).ok_or_else(bad_address)?,
                ),
            ],
        },
        // Apart from the figures, so the heaviest of today's statements refused costs only its own.
        LiveRead {
            route: "indexer-qos-behind",
            nest: NestId::Qos,
            params,
            parts: vec![Part::rows(
                "behind",
                NestId::Qos,
                sql::qos_indexer_seconds_behind_sql(addr, today).ok_or_else(bad_address)?,
            )],
        },
        LiveRead {
            route: "qos-freshness",
            nest: NestId::Qos,
            params: json!({}),
            parts: vec![Part::rows(
                "freshness",
                NestId::Qos,
                sql::QOS_FRESHNESS_SQL.to_string(),
            )],
        },
    ])
}

/// Today's network-wide rows: one statement shared by every indexer's request.
fn network_today(today: i64) -> LiveRead {
    LiveRead {
        route: "qos-network-today",
        nest: NestId::Qos,
        params: json!({"day": today}),
        parts: vec![Part::rows(
            "rows",
            NestId::Qos,
            sql::qos_network_rows_sql(today),
        )],
    }
}

/// The open allocations on `addr`'s deployments.
fn allocation_shares(addr: &str) -> Result<LiveRead, ApiError> {
    Ok(LiveRead {
        route: "qos-allocation-shares",
        nest: NestId::Alloc,
        params: json!({"address": addr}),
        parts: vec![Part::rows(
            "allocations",
            NestId::Alloc,
            sql::indexer_allocation_shares_sql(addr).ok_or_else(bad_address)?,
        )],
    })
}

/// Warm the QoS nest's reads behind `addr`'s QoS card and score, one at a time, stopping at the
/// first the nest refuses. Meant for a [`ReadCtx::warming`] copy.
pub async fn warm_qos_nest(ctx: &ReadCtx, addr: &str, today: i64) -> Result<Vec<Read>, ApiError> {
    let [figures, behind, freshness] = today_reads(addr, today)?;
    let mut out = Vec::new();
    for read in [figures, behind, freshness, network_today(today)] {
        out.push(read.read(ctx).await?);
    }
    Ok(out)
}

/// Warm the allocation nest's read behind `addr`'s QoS score.
pub async fn warm_allocation_shares(ctx: &ReadCtx, addr: &str) -> Result<Read, ApiError> {
    allocation_shares(addr)?.read(ctx).await
}

/// A merged answer, dated by the first live read (the stored days are older than it by construction)
/// and degraded when any live read is missing. Only the first dates it: the others may be another
/// nest's blocks.
fn merged(value: Value, live: &[Option<&Read>]) -> Read {
    let first = live.first().copied().flatten();
    let mut provenance = first
        .map(|r| r.entry.provenance.clone())
        .unwrap_or_default();
    provenance.degraded |= live.iter().any(Option::is_none);
    let entry = Arc::new(Entry::new(value, provenance));
    let meta = match first {
        Some(r) => Meta {
            provenance: entry.provenance.clone(),
            ..r.meta.clone()
        },
        None => entry.meta(ServedFrom::Postgres, false),
    };
    Read { entry, meta }
}

// ---------------------------------------------------------------------------------------------
// Query Performance
// ---------------------------------------------------------------------------------------------

/// `/api/indexer/[address]/qos`: the Query Performance card from `since_day` through `today` (unix day
/// numbers), closed days from the store and today from the nest.
pub async fn indexer_qos(
    ctx: &ReadCtx,
    store: Option<&dyn QosStore>,
    addr: &str,
    since_day: i64,
    today: i64,
) -> Result<Read, ApiError> {
    let store = store_or_unavailable(store)?;
    let [figures, behind, freshness] = today_reads(addr, today)?;
    let closed = if since_day < today {
        store.indexer_days(addr, since_day, today - 1).await?
    } else {
        Vec::new()
    };
    let (figures, behind, freshness) =
        futures::join!(live(ctx, figures), live(ctx, behind), live(ctx, freshness),);
    let lives = [figures.as_ref(), behind.as_ref(), freshness.as_ref()];
    let mut today_live = serde_json::Map::new();
    for r in lives.iter().flatten() {
        if let Some(obj) = r.entry.value.as_object() {
            today_live.extend(obj.clone());
        }
    }
    let input = qos_input(&closed, &Value::Object(today_live), today);
    Ok(merged(qos_answer(&input), &lives))
}

/// A stored day in the shape the nest answers a day in.
fn stored_day(d: &QosIndexerDay) -> Value {
    json!({
        "date": unix_day_date(d.day),
        "query_count": d.query_count,
        "num_200": d.num_200,
        "success_rate": d.success_rate,
        "latency_ms": d.latency_ms,
        "blocks_behind": d.blocks_behind,
        "total_query_fees": d.total_query_fees,
        "avg_query_fee": d.avg_query_fee,
        "allocations": d.allocations,
        "buckets": d.buckets,
        "bad_buckets": d.bad_buckets,
        "worst_bucket_start": d.worst_bucket_start,
        "worst_bucket_success_rate": d.worst_bucket_success_rate,
        "worst_bucket_queries": d.worst_bucket_queries,
        "seconds_behind_peers": d.seconds_behind_peers,
        "share_queries_over_5min_behind": d.share_queries_over_5min_behind,
        "share_with_block_time": d.share_with_block_time,
    })
}

/// Closed days from the store, then today from the nest. A stored row for today is ignored: today is
/// still arriving, and the store's copy is only as new as the job's last run.
pub fn qos_input(closed: &[QosIndexerDay], live: &Value, today: i64) -> Value {
    let arr = |k: &str| {
        live.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let today_date = unix_day_date(today);
    let mut days: Vec<Value> = closed
        .iter()
        .filter(|d| d.day < today)
        .map(stored_day)
        .collect();
    let mut gateways: Vec<Value> = closed
        .iter()
        .filter(|d| d.day < today)
        .map(|d| json!({"date": unix_day_date(d.day), "gateways": d.gateways.join(",")}))
        .collect();
    let behind = arr("behind");
    for mut d in arr("days") {
        if d.get("date").and_then(Value::as_str) != Some(today_date.as_str()) {
            continue;
        }
        if let (Some(obj), Some(b)) = (d.as_object_mut(), behind.first()) {
            for k in [
                "seconds_behind_peers",
                "share_queries_over_5min_behind",
                "share_with_block_time",
            ] {
                obj.insert(k.into(), b.get(k).cloned().unwrap_or(Value::Null));
            }
        }
        days.push(d);
    }
    gateways.extend(arr("gateways"));
    json!({"days": days, "gateways": gateways, "freshness": arr("freshness")})
}

/// The card's shape: the retired route's `IndexerQoSPoint` per day, plus what it could not say.
///
/// A day with no row is absent, never a zero. Window figures are ratios of sums, which is where the
/// old card went wrong: it averaged daily figures, so a quiet day counted as much as a busy one.
pub fn qos_answer(v: &Value) -> Value {
    let days = v
        .get("days")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let gateways: HashMap<String, Vec<String>> = v
        .get("gateways")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let ids = r
                        .get("gateways")?
                        .as_str()?
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    Some((r.get("date")?.as_str()?.to_string(), ids))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut qos = Vec::with_capacity(days.len());
    let (mut queries, mut ok, mut fees) = (0.0, 0.0, 0.0);
    let (mut lat_x, mut lat_q, mut sec_x, mut sec_q) = (0.0, 0.0, 0.0, 0.0);
    let (mut buckets, mut bad) = (0_i64, 0_i64);
    let mut worst: Option<(f64, i64, f64)> = None;
    let bucket = |rate: f64, start: i64, volume: f64| json!({"start": start, "successRate": num(Some(rate * 100.0)), "queries": num(Some(volume))});

    for r in &days {
        let q = f(r, "query_count").unwrap_or(0.0);
        let latency = f(r, "latency_ms");
        let seconds = f(r, "seconds_behind_peers");
        let known = f(r, "share_with_block_time");
        let day_buckets = f(r, "buckets").unwrap_or(0.0) as i64;
        let day_bad = f(r, "bad_buckets").unwrap_or(0.0) as i64;
        let day_fees = f(r, "total_query_fees").unwrap_or(0.0);

        queries += q;
        ok += f(r, "num_200").unwrap_or(0.0);
        fees += day_fees;
        buckets += day_buckets;
        bad += day_bad;
        if let Some(l) = latency {
            lat_x += l * q;
            lat_q += q;
        }
        if let (Some(s), Some(k)) = (seconds, known) {
            sec_x += s * q * k;
            sec_q += q * k;
        }

        let worst_bucket = match (
            shape::i(r, "worst_bucket_start").as_i64(),
            f(r, "worst_bucket_success_rate"),
        ) {
            (Some(start), Some(rate)) => {
                let volume = f(r, "worst_bucket_queries").unwrap_or(0.0);
                if worst.is_none_or(|(w, ..)| rate < w) {
                    worst = Some((rate, start, volume));
                }
                bucket(rate, start, volume)
            }
            _ => Value::Null,
        };
        let date = shape::t(r, "date");
        let gateway_ids = date
            .as_str()
            .and_then(|d| gateways.get(d))
            .cloned()
            .unwrap_or_default();

        qos.push(json!({
            "date": date,
            "queryCount": num(Some(q)),
            "successRate": num(f(r, "success_rate").map(|x| x * 100.0)),
            "latencyMs": num(latency),
            "blocksBehind": num(f(r, "blocks_behind")),
            "avgQueryFee": num(f(r, "avg_query_fee")),
            "totalQueryFees": num(Some(day_fees)),
            "buckets": day_buckets,
            "partial": day_buckets < 288,
            "badBuckets": day_bad,
            "worstBucket": worst_bucket,
            "secondsBehind": num(seconds),
            "shareQueriesOver5MinBehind": num(f(r, "share_queries_over_5min_behind")),
            "shareWithBlockTime": num(known),
            "gatewayIds": gateway_ids,
        }));
    }

    let ratio = |x: f64, d: f64| (d > 0.0).then(|| x / d);
    let latest = days.last();
    let latest_date = latest.map(|r| shape::t(r, "date")).unwrap_or(Value::Null);
    let freshness = v
        .get("freshness")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("topic").and_then(Value::as_str) == Some("indexer"))
        })
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "qos": qos,
        "summary": {
            "days": days.len(),
            "queryCount": num(Some(queries)),
            "successRate": num(ratio(ok, queries).map(|x| x * 100.0)),
            "latencyMs": num(ratio(lat_x, lat_q)),
            "secondsBehind": num(ratio(sec_x, sec_q)),
            "totalQueryFees": num(Some(fees)),
            "avgQueryFee": num(ratio(fees, queries)),
            "latestAvgQueryFee": num(latest.and_then(|r| f(r, "avg_query_fee"))),
            "latestDate": latest_date.clone(),
            "buckets": buckets,
            "badBuckets": bad,
            "worstBucket": worst.map(|(rate, start, volume)| bucket(rate, start, volume)),
        },
        // Two questions: when the publisher last posted to Gnosis, and the newest bucket the nest
        // holds. A publisher gone quiet shows as the first falling behind; a stalled resolver shows as
        // the second falling behind the first.
        "freshness": {
            "publisherLastPostAt": shape::i(&freshness, "last_post_time"),
            "newestBucketStart": shape::i(&freshness, "newest_bucket_start"),
            "newestDate": latest_date,
            "newestDateBuckets": latest.and_then(|r| f(r, "buckets")).map(|b| b as i64),
        },
    })
}

// ---------------------------------------------------------------------------------------------
// QoS Quality
// ---------------------------------------------------------------------------------------------

/// A `qos_allocation_daily` row as the nest answered it.
pub fn allocation_daily_from(r: &Value) -> Option<AllocationDaily> {
    Some(AllocationDaily {
        indexer: r.get("indexer")?.as_str()?.to_lowercase(),
        deployment: r.get("deployment")?.as_str()?.to_string(),
        chain: r
            .get("chain")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
        gateway_id: r
            .get("gateway")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        day_start: shape::i(r, "unix_day").as_i64()? * 86_400,
        query_count: f(r, "query_count").unwrap_or(0.0),
        num_200: f(r, "num_200"),
        latency_ms: f(r, "latency_ms"),
        blocks_behind: f(r, "blocks_behind"),
        total_query_fees: f(r, "total_query_fees").unwrap_or(0.0),
        buckets: f(r, "buckets").unwrap_or(0.0) as u32,
    })
}

/// A stored allocation day as the scorer reads it.
pub fn daily_row(a: &QosAllocationDay) -> QosDailyRow {
    QosDailyRow {
        indexer_address: a.indexer.clone(),
        deployment_id: a.deployment.clone(),
        day_number: day_number(a.day * 86_400),
        query_count: a.query_count,
        success_count: a.num_200,
        avg_latency_ms: a.latency_ms,
        blocks_behind: a.blocks_behind,
        chain_id: a.chain.clone(),
    }
}

/// An open stake total, with the deployment re-spelt as the CID the oracle uses: the allocations
/// nest carries the bytes32 id, the oracle `Qm…`, and a mismatch would zero every allocation share.
pub fn allocation_row_from(r: &Value) -> Option<AllocationRow> {
    Some(AllocationRow {
        indexer_address: r.get("indexer")?.as_str()?.to_lowercase(),
        deployment_id: shape::deployment_ipfs_hash(r.get("subgraph_deployment")?.as_str()?)?,
        allocated_grt: f(r, "allocated_tokens")? / 1e18,
    })
}

/// Everything a score is computed from.
#[derive(Debug, Default, Clone)]
pub struct Cohort {
    pub rows: Vec<QosDailyRow>,
    pub allocations: Vec<AllocationRow>,
}

impl Cohort {
    /// The window that ends on `day` (a Graph day number), as the incumbent's cron scored that day:
    /// from thirty days before through the day itself.
    fn window(&self, day: i64) -> Vec<QosDailyRow> {
        let since = day - SCORE_WINDOW_DAYS;
        self.rows
            .iter()
            .filter(|r| r.day_number >= since && r.day_number <= day)
            .cloned()
            .collect()
    }

    /// One indexer's score on one day, or `None` when the oracle measured it on nothing in the
    /// window.
    pub fn score(&self, addr: &str, day: i64) -> Option<QualityDetail> {
        let mine = aggregate_indexer_metrics(&self.window(day), &AggregateOpts::today(day))
            .into_iter()
            .find(|(ix, _)| ix == addr)?
            .1;
        Some(compute_quality_detail(&mine, &QualityOpts::default()))
    }

    /// Served gap and efficiency on `day` against the allocations open now, which is also what the
    /// incumbent's cron had: the allocations open when it ran.
    pub fn phase2(&self, addr: &str, day: i64) -> Option<Phase2Metric> {
        compute_phase2_metrics(&self.allocations, &self.window(day))
            .into_iter()
            .find(|(ix, _)| ix == addr)
            .map(|(_, m)| m)
    }
}

/// Stored rows before `today` on the deployments `addr` served, today's rows from the nest on those
/// same deployments, and the open allocations on them. Today's network-wide rows are one statement
/// shared by every indexer's request. Without today's rows the score is the stored window's; without
/// the allocations the served gap is unknown.
async fn cohort(
    ctx: &ReadCtx,
    store: &dyn QosStore,
    addr: &str,
    from: i64,
    today: i64,
) -> Result<(Cohort, [Option<Read>; 2]), ApiError> {
    let shares = allocation_shares(addr)?;
    let (live_rows, shares) = futures::join!(live(ctx, network_today(today)), live(ctx, shares));
    let today_rows: Vec<QosDailyRow> = live_rows
        .as_ref()
        .and_then(|r| r.entry.value.get("rows")?.as_array().cloned())
        .map(|rows| {
            rows.iter()
                .filter_map(allocation_daily_from)
                .map(|r| QosDailyRow::from(&r))
                .collect()
        })
        .unwrap_or_default();
    let served_today: Vec<String> = today_rows
        .iter()
        .filter(|r| r.indexer_address == addr)
        .map(|r| r.deployment_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let closed = if from < today {
        store
            .cohort_days(addr, from, today - 1, &served_today)
            .await?
    } else {
        Vec::new()
    };
    let deployments: BTreeSet<&str> = closed
        .iter()
        .map(|r| r.deployment.as_str())
        .chain(served_today.iter().map(String::as_str))
        .collect();
    let mut rows: Vec<QosDailyRow> = closed
        .iter()
        .filter(|r| r.day < today)
        .map(daily_row)
        .collect();
    rows.extend(
        today_rows
            .iter()
            .filter(|r| deployments.contains(r.deployment_id.as_str()))
            .cloned(),
    );
    let allocations = shares
        .as_ref()
        .and_then(|r| r.entry.value.get("allocations")?.as_array().cloned())
        .map(|a| a.iter().filter_map(allocation_row_from).collect())
        .unwrap_or_default();
    Ok((Cohort { rows, allocations }, [live_rows, shares]))
}

/// `/api/indexer/[address]/qos-score`: the latest score with its breakdown, and one score per day.
pub async fn indexer_qos_score(
    ctx: &ReadCtx,
    store: Option<&dyn QosStore>,
    addr: &str,
    today_unix_day: i64,
) -> Result<Read, ApiError> {
    let store = store_or_unavailable(store)?;
    let from = today_unix_day - SCORE_WINDOW_DAYS - (SCORE_HISTORY_DAYS - 1);
    let (c, [rows, shares]) = cohort(ctx, store, addr, from, today_unix_day).await?;
    Ok(merged(
        score_answer(&c, addr, today_unix_day - GRAPH_EPOCH_DAYS),
        &[rows.as_ref(), shares.as_ref()],
    ))
}

/// The retired panel's shape: `latest` as its score row, `daily` as `{day, q_score}`. The history is
/// computed per day over the window that day had, where the incumbent's built up one cron run at a
/// time.
pub fn score_answer(cohort: &Cohort, addr: &str, today: i64) -> Value {
    let daily: Vec<Value> = (today - (SCORE_HISTORY_DAYS - 1)..=today)
        .filter_map(|day| {
            let d = cohort.score(addr, day)?;
            Some(json!({"day": graph_day_date(day), "q_score": num(Some(d.result.q_score))}))
        })
        .collect();
    let latest = cohort.score(addr, today).map(|d| {
        let p2 = cohort.phase2(addr, today);
        json!({
            "day": graph_day_date(today),
            "day_number": today,
            "reliability": num(Some(d.result.reliability)),
            "lat_util": num(Some(d.result.lat_util)),
            "fresh_util": num(d.result.fresh_util),
            "coverage": num(Some(d.result.coverage)),
            "served_gap": num(p2.map(|m| m.served_gap)),
            "efficiency": num(p2.map(|m| m.efficiency)),
            "q_score": num(Some(d.result.q_score)),
            "grade": qos_grade(d.result.q_score).grade,
        })
    });
    json!({"window_days": SCORE_WINDOW_DAYS, "latest": latest, "daily": daily})
}

/// `/api/indexer/[address]/qos-deployments`: the deployments behind today's score.
pub async fn indexer_qos_deployments(
    ctx: &ReadCtx,
    store: Option<&dyn QosStore>,
    addr: &str,
    today_unix_day: i64,
) -> Result<Read, ApiError> {
    let store = store_or_unavailable(store)?;
    let (c, [rows, shares]) = cohort(
        ctx,
        store,
        addr,
        today_unix_day - SCORE_WINDOW_DAYS,
        today_unix_day,
    )
    .await?;
    Ok(merged(
        deployments_answer(&c, addr, today_unix_day - GRAPH_EPOCH_DAYS),
        &[rows.as_ref(), shares.as_ref()],
    ))
}

fn drag(d: &DeploymentContribution) -> f64 {
    d.weight * (1.0 - d.q.unwrap_or(1.0))
}

/// Heaviest drag first: weight times how far short of a perfect deployment it fell, the order an
/// operator fixes things in.
///
/// The incumbent's route passed no deployment totals, so every served share it published was zero.
/// Weight has been queries served since 2026-08-15, so the score does not depend on the share.
pub fn deployments_answer(cohort: &Cohort, addr: &str, today: i64) -> Value {
    let Some(detail) = cohort.score(addr, today) else {
        return json!({"window_days": SCORE_WINDOW_DAYS, "deployments": [], "total": null});
    };
    let mut deployments = detail.deployments.clone();
    deployments.sort_by(|a, b| drag(b).total_cmp(&drag(a)));
    let r = &detail.result;
    json!({
        "window_days": SCORE_WINDOW_DAYS,
        "total": {
            "q_score": num(Some(r.q_score)),
            "reliability": num(Some(r.reliability)),
            "lat_util": num(Some(r.lat_util)),
            "fresh_util": num(r.fresh_util),
            "coverage": num(Some(r.coverage)),
            "credible_deployments": r.credible_deployments,
            "unmeasured_deployments": r.unmeasured_deployments,
        },
        "deployments": deployments.iter().map(|d| json!({
            "deployment_id": d.deployment,
            "queries": num(Some(d.n)),
            "weight": num(Some(d.weight)),
            "reliability": num(d.reliability),
            "reliability_used": num(d.reliability_used),
            "cohort_best_reliability": num(d.cohort_best_reliability),
            "lat_util": num(Some(d.lat_util)),
            "fresh_util": num(d.fresh_util),
            "time_behind_sec": num(d.time_behind_sec),
            "time_behind_own_sec": num(d.time_behind_own_sec),
            "served_share": num(Some(d.served_share)),
            "q": num(d.q),
            "measured": d.measured,
            "drag": num(Some(drag(d))),
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------------------------
// REO coverage
// ---------------------------------------------------------------------------------------------

/// The Rewards Eligibility Oracle's window, today included.
pub const REO_WINDOW_DAYS: i64 = 28;

/// The curation floor a qualifying subgraph needs from 2026-10-06, per the Foundation's announcement
/// of 22 September. The oracle's criteria document does not state it.
pub const REO_SIGNAL_FLOOR_GRT: i128 = 500;

/// One deployment an indexer answered on one day, summed over chains and gateways later.
#[derive(Debug, Clone, PartialEq)]
pub struct Served {
    /// Unix day number.
    pub day: i64,
    pub deployment: String,
    pub num_200: f64,
}

/// Per `Qm…` deployment asked about, the change in signal (wei) per unix day; the earliest key holds
/// every change before the window. A deployment with no curation events has an empty map.
pub type SignalDeltas = HashMap<String, BTreeMap<i64, i128>>;

/// `/api/indexer/[address]/qos-qualifying-days`: per UTC day of the oracle's window, the
/// deployments that answered at least one query with HTTP 200, with signal on each as of that day.
pub async fn indexer_qos_qualifying_days(
    ctx: &ReadCtx,
    store: Option<&dyn QosStore>,
    addr: &str,
    today: i64,
) -> Result<Read, ApiError> {
    let store = store_or_unavailable(store)?;
    if !sql::is_address(addr) {
        return Err(bad_address());
    }
    let from = today - (REO_WINDOW_DAYS - 1);
    let (closed, stored) = futures::try_join!(
        store.indexer_allocation_days(addr, from, today - 1),
        store.stored_days(from, today - 1),
    )?;
    let today_rows = live(ctx, network_today(today)).await;

    let mut served: Vec<Served> = closed
        .iter()
        .filter(|r| r.day < today)
        .map(|r| Served {
            day: r.day,
            deployment: r.deployment.clone(),
            num_200: r.num_200.unwrap_or(0.0),
        })
        .collect();
    let mut measured: BTreeSet<i64> = stored.into_iter().filter(|d| *d < today).collect();
    if let Some(r) = &today_rows {
        measured.insert(today);
        let rows = r
            .entry
            .value
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        served.extend(
            rows.iter()
                .filter_map(allocation_daily_from)
                .filter(|a| a.indexer == addr)
                .map(|a| Served {
                    day: a.day_start.div_euclid(86_400),
                    deployment: a.deployment,
                    num_200: a.num_200.unwrap_or(0.0),
                }),
        );
    }

    let deployments: BTreeSet<&str> = served
        .iter()
        .filter(|s| s.num_200 > 0.0)
        .map(|s| s.deployment.as_str())
        .collect();
    let (signal, read) = match signal_days(&deployments, from) {
        Err(why) => (Err(why), None),
        Ok(None) => (Ok(SignalDeltas::new()), None),
        Ok(Some((asked, read))) => match live(ctx, read).await {
            Some(r) => {
                let mut deltas = signal_deltas_from(
                    &r.entry
                        .value
                        .as_object()
                        .map(|parts| {
                            parts
                                .values()
                                .filter_map(Value::as_array)
                                .flatten()
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                );
                for cid in asked {
                    deltas.entry(cid).or_default();
                }
                (Ok(deltas), Some(Some(r)))
            }
            None => (
                Err("the allocations nest did not answer, so signal on each day is unknown"),
                Some(None),
            ),
        },
    };
    let value = qualifying_days_answer(&served, &measured, today, signal.as_ref().map_err(|e| *e));
    let mut lives = vec![today_rows.as_ref()];
    if let Some(r) = &read {
        lives.push(r.as_ref());
    }
    Ok(merged(value, &lives))
}

/// The allocations nest read behind the floor, and the `Qm…` ids it asks about, or `None` when there
/// is nothing to ask. Two hundred ids per statement keeps each under the nest's 16 KiB limit on a query.
fn signal_days(
    deployments: &BTreeSet<&str>,
    from: i64,
) -> Result<Option<(Vec<String>, LiveRead)>, &'static str> {
    const PARTS: [&str; 16] = [
        "signal0", "signal1", "signal2", "signal3", "signal4", "signal5", "signal6", "signal7",
        "signal8", "signal9", "signal10", "signal11", "signal12", "signal13", "signal14",
        "signal15",
    ];
    let ids: Vec<(String, String)> = deployments
        .iter()
        .filter_map(|cid| Some((cid.to_string(), shape::ipfs_hash_to_bytes32(cid)?)))
        .collect();
    if ids.is_empty() {
        return Ok(None);
    }
    let hexes: Vec<String> = ids.iter().map(|(_, h)| h.clone()).collect();
    if hexes.len() > 200 * PARTS.len() {
        return Err("more deployments than the signal read asks about in one answer");
    }
    let parts = hexes
        .chunks(200)
        .zip(PARTS)
        .filter_map(|(chunk, name)| {
            Some(Part::rows(
                name,
                NestId::Alloc,
                sql::deployment_signal_days_sql(chunk, from)?,
            ))
        })
        .collect();
    Ok(Some((
        ids.into_iter().map(|(cid, _)| cid).collect(),
        LiveRead {
            route: "qos-qualifying-signal",
            nest: NestId::Alloc,
            params: json!({"from": from, "deployments": hexes}),
            parts,
        },
    )))
}

/// The rows the nest answers [`sql::deployment_signal_days_sql`] with, keyed by `Qm…`.
pub fn signal_deltas_from(rows: &[Value]) -> SignalDeltas {
    let mut out = SignalDeltas::new();
    for r in rows {
        let (Some(cid), Some(day), Some(delta)) = (
            r.get("deployment")
                .and_then(Value::as_str)
                .and_then(shape::deployment_ipfs_hash),
            shape::i(r, "unix_day").as_i64(),
            r.get("delta")
                .and_then(Value::as_str)
                .and_then(|d| d.parse::<i128>().ok()),
        ) else {
            continue;
        };
        *out.entry(cid).or_default().entry(day).or_default() += delta;
    }
    out
}

const QUALIFYING_TEST_REASON: &str = "the QoS rows give each deployment and day a count of HTTP 200s \
     and the mean latency and blocks behind of all its queries, not each query's, so whether a 200 \
     was also under 5,000 ms and under 50,000 blocks behind cannot be told. Each count is deployments \
     with at least one HTTP 200 that day, which bounds the oracle's count from above: under 5 here is \
     under 5 for the oracle, and 5 or more is not proof of 5";

/// The route's shape. `measured` is every day the oracle's feed is known to cover, so a day in it with
/// no rows for this indexer is a zero rather than a gap.
pub fn qualifying_days_answer(
    served: &[Served],
    measured: &BTreeSet<i64>,
    today: i64,
    signal: Result<&SignalDeltas, &str>,
) -> Value {
    let floor = REO_SIGNAL_FLOOR_GRT * 1_000_000_000_000_000_000;
    let from = today - (REO_WINDOW_DAYS - 1);
    let mut by_day: BTreeMap<i64, BTreeMap<&str, f64>> = measured
        .iter()
        .filter(|d| (from..=today).contains(*d))
        .map(|d| (*d, BTreeMap::new()))
        .collect();
    for s in served.iter().filter(|s| (from..=today).contains(&s.day)) {
        *by_day
            .entry(s.day)
            .or_default()
            .entry(s.deployment.as_str())
            .or_default() += s.num_200;
    }
    let days: Vec<Value> = by_day
        .into_iter()
        .map(|(day, deps)| {
            let mut deps: Vec<(&str, f64)> = deps.into_iter().filter(|(_, n)| *n > 0.0).collect();
            deps.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
            let rows: Vec<(&str, f64, Option<i128>)> = deps
                .into_iter()
                .map(|(d, n)| {
                    let at = signal
                        .ok()
                        .and_then(|s| s.get(d))
                        .map(|deltas| deltas.range(..=day).map(|(_, v)| v).sum::<i128>());
                    (d, n, at)
                })
                .collect();
            let over = rows
                .iter()
                .filter(|(_, _, s)| s.is_some_and(|s| s >= floor))
                .count();
            json!({
                "date": unix_day_date(day),
                "partial": day == today,
                "count": if signal.is_ok() { over } else { rows.len() },
                "count_without_floor": rows.len(),
                "deployments": rows.iter().map(|(d, n, s)| json!({
                    "deployment_id": d,
                    "num_200": num(Some(*n)),
                    "signal_grt": num(s.map(|s| s as f64 / 1e18)),
                    "over_floor": s.map(|s| s >= floor),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "window_days": REO_WINDOW_DAYS,
        "qualifying_test_applied": false,
        "bound": "upper",
        "qualifying_test_reason": QUALIFYING_TEST_REASON,
        "signal_floor_grt": REO_SIGNAL_FLOOR_GRT,
        "signal_floor_applied": signal.is_ok(),
        "signal_floor_reason": signal.err(),
        "days": days,
    })
}

/// Every indexer's score on `today` (a Graph day number), for the directory.
pub fn score_network(
    cohort: &Cohort,
    today: i64,
) -> Vec<(String, QualityDetail, Option<Phase2Metric>)> {
    let rows = cohort.window(today);
    let phase2: HashMap<String, Phase2Metric> = compute_phase2_metrics(&cohort.allocations, &rows)
        .into_iter()
        .collect();
    aggregate_indexer_metrics(&rows, &AggregateOpts::today(today))
        .into_iter()
        .map(|(ix, metrics)| {
            let detail = compute_quality_detail(&metrics, &QualityOpts::default());
            let p2 = phase2.get(&ix).copied();
            (ix, detail, p2)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

    const A: &str = "0x00000000000000000000000000000000000000aa";
    const B: &str = "0x00000000000000000000000000000000000000bb";
    const DEP: &str = "QmdT8aMbj72gJt6RtSQFhhWbyG4C7vwGkghnvfT8Y8zLgJ";
    /// 2026-09-12, as a unix day number and a Graph day number.
    const TODAY_UNIX: i64 = 20_708;
    const TODAY: i64 = TODAY_UNIX - GRAPH_EPOCH_DAYS;

    fn day(date: &str, queries: f64, ok: f64, latency: f64, buckets: i64) -> Value {
        json!({
            "date": date, "query_count": queries, "num_200": ok, "success_rate": ok / queries,
            "latency_ms": latency, "blocks_behind": 3.0, "total_query_fees": queries * 0.001,
            "avg_query_fee": 0.001, "allocations": 1, "buckets": buckets, "bad_buckets": 0,
            "worst_bucket_start": null, "worst_bucket_success_rate": null, "worst_bucket_queries": null,
            "seconds_behind_peers": 12.0, "share_queries_over_5min_behind": 0.0, "share_with_block_time": 1.0,
        })
    }

    /// The old card averaged the days, which reads 75% and 150 ms here. The window served 950 of
    /// 1,000 queries.
    #[test]
    fn the_window_is_weighted_by_queries_and_a_partial_day_says_so() {
        let out = qos_answer(&json!({"days": [
            day("2026-09-06", 100.0, 50.0, 100.0, 288),
            day("2026-09-07", 900.0, 900.0, 200.0, 12),
        ]}));
        assert_eq!(out["summary"]["successRate"], json!(95.0));
        assert_eq!(out["summary"]["latencyMs"], json!(190.0));
        assert_eq!(out["qos"][0]["partial"], json!(false));
        assert_eq!(out["qos"][1]["partial"], json!(true));
        assert_eq!(out["qos"][1]["successRate"], json!(100.0));
    }

    #[test]
    fn an_indexer_the_oracle_never_measured_has_no_points_and_no_figures() {
        let out = qos_answer(&json!({"days": [], "gateways": [], "freshness": []}));
        assert_eq!(out["qos"], json!([]));
        for k in [
            "successRate",
            "latencyMs",
            "avgQueryFee",
            "worstBucket",
            "latestDate",
        ] {
            assert_eq!(out["summary"][k], Value::Null, "{k}");
        }
        assert_eq!(out["freshness"]["newestDate"], Value::Null);
    }

    #[test]
    fn the_window_worst_bucket_is_the_lowest_of_the_days() {
        let mut d1 = day("2026-09-06", 100.0, 90.0, 1.0, 288);
        d1["worst_bucket_start"] = json!(100);
        d1["worst_bucket_success_rate"] = json!(0.5);
        d1["worst_bucket_queries"] = json!(60);
        let mut d2 = day("2026-09-07", 100.0, 90.0, 1.0, 288);
        d2["worst_bucket_start"] = json!(200);
        d2["worst_bucket_success_rate"] = json!(0.0);
        d2["worst_bucket_queries"] = json!(52_286);
        let out = qos_answer(&json!({ "days": [d1, d2] }));
        assert_eq!(
            out["summary"]["worstBucket"],
            json!({"start": 200, "successRate": 0.0, "queries": 52_286.0})
        );
    }

    fn stored(addr: &str, unix_day: i64, queries: f64) -> QosIndexerDay {
        QosIndexerDay {
            indexer: addr.into(),
            day: unix_day,
            query_count: queries,
            num_200: Some(queries),
            success_rate: Some(1.0),
            latency_ms: Some(100.0),
            total_query_fees: queries * 0.001,
            buckets: 288,
            gateways: vec!["0xgw".into()],
            ..QosIndexerDay::default()
        }
    }

    fn alloc_day(addr: &str, unix_day: i64, queries: f64) -> QosAllocationDay {
        QosAllocationDay {
            indexer: addr.into(),
            deployment: DEP.into(),
            chain: Some("arbitrum-one".into()),
            gateway: "0xgw".into(),
            day: unix_day,
            query_count: queries,
            num_200: Some(queries),
            latency_ms: Some(120.0),
            blocks_behind: Some(3.0),
            total_query_fees: 0.5,
            buckets: 288,
        }
    }

    #[derive(Default)]
    struct MemStore {
        indexer: Vec<QosIndexerDay>,
        allocation: Vec<QosAllocationDay>,
        days: Vec<i64>,
    }

    impl QosStore for MemStore {
        fn indexer_days<'a>(
            &'a self,
            addr: &'a str,
            from: i64,
            to: i64,
        ) -> BoxFuture<'a, Result<Vec<QosIndexerDay>, ApiError>> {
            let out = self
                .indexer
                .iter()
                .filter(|d| d.indexer == addr && (from..=to).contains(&d.day))
                .cloned()
                .collect();
            Box::pin(async move { Ok(out) })
        }
        fn cohort_days<'a>(
            &'a self,
            _addr: &'a str,
            from: i64,
            to: i64,
            _also: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>> {
            let out = self
                .allocation
                .iter()
                .filter(|d| (from..=to).contains(&d.day))
                .cloned()
                .collect();
            Box::pin(async move { Ok(out) })
        }
        fn indexer_allocation_days<'a>(
            &'a self,
            addr: &'a str,
            from: i64,
            to: i64,
        ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>> {
            let out = self
                .allocation
                .iter()
                .filter(|d| d.indexer == addr && (from..=to).contains(&d.day))
                .cloned()
                .collect();
            Box::pin(async move { Ok(out) })
        }
        fn stored_days(&self, from: i64, to: i64) -> BoxFuture<'_, Result<Vec<i64>, ApiError>> {
            let out = self
                .days
                .iter()
                .copied()
                .filter(|d| (from..=to).contains(d))
                .collect();
            Box::pin(async move { Ok(out) })
        }
    }

    /// A stand-in nest that answers today for indexer A, and records every statement.
    struct Nest(Arc<Mutex<Vec<String>>>);

    impl Respond for Nest {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if req.url.path().ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            self.0.lock().unwrap().push(q.clone());
            let today = unix_day_date(TODAY_UNIX);
            let rows = if q.contains("curation__signalled") {
                let hex = shape::ipfs_hash_to_bytes32(DEP).unwrap();
                // 400 GRT before the window, 150 more two days ago: under the floor until then.
                json!([
                    {"deployment": hex, "unix_day": TODAY_UNIX - REO_WINDOW_DAYS, "delta": "400000000000000000000"},
                    {"deployment": hex, "unix_day": TODAY_UNIX - 2, "delta": "150000000000000000000"},
                ])
            } else if q.contains("qos_freshness") {
                json!([{"topic": "indexer", "newest_bucket_start": 1_789_171_000_i64, "last_post_time": 1_789_172_000_i64}])
            } else if q.contains("qos_indexer_seconds_behind") && q.contains(A) {
                json!([{"indexer": A, "seconds_behind_peers": 7.0, "share_with_block_time": 1.0}])
            } else if q.contains("string_agg") && q.contains(A) {
                json!([{"date": today, "gateways": "0xgw"}])
            } else if q.contains("qos_indexer_daily") && q.contains(A) {
                json!([{"date": today, "query_count": 500.0, "num_200": 250.0, "success_rate": 0.5, "buckets": 12, "bad_buckets": 3}])
            } else if q.contains("qos_allocation_daily") {
                json!([
                    {"indexer": A, "deployment": DEP, "chain": "arbitrum-one", "gateway": "0xgw", "unix_day": TODAY_UNIX,
                     "query_count": 100.0, "num_200": 100.0, "latency_ms": 120.0, "blocks_behind": 3.0, "total_query_fees": 0.1, "buckets": 12},
                    {"indexer": B, "deployment": DEP, "chain": "arbitrum-one", "gateway": "0xgw", "unix_day": TODAY_UNIX,
                     "query_count": 100.0, "num_200": 50.0, "latency_ms": 120.0, "blocks_behind": 3.0, "total_query_fees": 0.1, "buckets": 12},
                ])
            } else {
                json!([])
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "count": rows.as_array().map(Vec::len).unwrap_or(0), "rows": rows,
                "provenance": {"registry_hash": "r1", "sealed_through": 5000, "as_of": 5100}
            }))
        }
    }

    async fn ctx() -> (ReadCtx, Arc<Mutex<Vec<String>>>, MockServer) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (ctx, server) = ctx_with(Nest(seen.clone())).await;
        (ctx, seen, server)
    }

    async fn ctx_with(nest: impl Respond + 'static) -> (ReadCtx, MockServer) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(nest)
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let ctx = ReadCtx::new(
            kittiwake_nest::NestGate::new(cfg).unwrap(),
            kittiwake_cache::Store::new(64),
            1,
        );
        (ctx, server)
    }

    /// The stand-in nest, except that it refuses every statement containing `refuse` the way DuckDB
    /// refuses one over its memory limit, or says it is not ready at all.
    struct Refusing {
        refuse: &'static str,
        unready: bool,
    }

    impl Respond for Refusing {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if req.url.path().ends_with("/ready") && self.unready {
                return ResponseTemplate::new(503).set_body_json(json!({
                    "ready": false, "lag_blocks": 90_000, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            if !self.refuse.is_empty() && q.contains(self.refuse) {
                return ResponseTemplate::new(400).set_body_json(json!({
                    "error": "query failed: Out of Memory Error: could not allocate block of size 256.0 KiB (243.9 MiB/244.1 MiB used)"
                }));
            }
            Nest(Arc::new(Mutex::new(Vec::new()))).respond(req)
        }
    }

    fn dates(read: &Read) -> Vec<String> {
        read.entry.value["qos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["date"].as_str().unwrap().to_string())
            .collect()
    }

    /// Closed days are in Postgres, so a nest that cannot answer today costs the card today and
    /// nothing more: today is a gap, and the answer says it was degraded.
    #[tokio::test]
    async fn a_nest_that_cannot_answer_today_leaves_a_gap_not_a_failed_card() {
        for nest in [
            Refusing {
                refuse: "FROM qos_",
                unready: false,
            },
            Refusing {
                refuse: "",
                unready: true,
            },
        ] {
            let (ctx, _server) = ctx_with(nest).await;
            let store = MemStore {
                indexer: vec![
                    stored(A, TODAY_UNIX - 3, 1_000.0),
                    stored(A, TODAY_UNIX - 1, 2_000.0),
                ],
                ..MemStore::default()
            };
            let read = indexer_qos(&ctx, Some(&store), A, TODAY_UNIX - 89, TODAY_UNIX)
                .await
                .expect("the closed days are served");
            assert_eq!(dates(&read), vec!["2026-09-09", "2026-09-11"]);
            assert_eq!(read.entry.value["summary"]["queryCount"], json!(3_000.0));
            assert_eq!(
                read.entry.value["freshness"]["publisherLastPostAt"],
                Value::Null
            );
            assert!(read.entry.provenance.degraded);
        }
    }

    /// Seconds behind is the heaviest of today's statements. Refused alone, today keeps its other
    /// figures and seconds behind reads as not measured.
    #[tokio::test]
    async fn a_refused_seconds_behind_leaves_today_without_that_figure_only() {
        let (ctx, _server) = ctx_with(Refusing {
            refuse: "qos_indexer_seconds_behind",
            unready: false,
        })
        .await;
        let store = MemStore {
            indexer: vec![stored(A, TODAY_UNIX - 1, 2_000.0)],
            ..MemStore::default()
        };
        let read = indexer_qos(&ctx, Some(&store), A, TODAY_UNIX - 1, TODAY_UNIX)
            .await
            .unwrap();
        let out = &*read.entry.value;
        assert_eq!(dates(&read), vec!["2026-09-11", "2026-09-12"]);
        assert_eq!(out["qos"][1]["queryCount"], json!(500.0));
        assert_eq!(out["qos"][1]["secondsBehind"], Value::Null);
        assert_eq!(out["qos"][1]["gatewayIds"], json!(["0xgw"]));
        assert_eq!(
            out["freshness"]["publisherLastPostAt"],
            json!(1_789_172_000_i64)
        );
        assert!(read.entry.provenance.degraded);
    }

    /// Without today's rows the score is the one the stored window gives; without the open
    /// allocations the served gap is unknown rather than computed against nothing.
    #[tokio::test]
    async fn the_score_stands_on_stored_days_when_today_or_the_allocations_are_refused() {
        let store = MemStore {
            allocation: vec![
                alloc_day(A, TODAY_UNIX - 1, 800.0),
                alloc_day(B, TODAY_UNIX - 1, 300.0),
            ],
            ..MemStore::default()
        };
        let stored_only: Vec<QosDailyRow> = store.allocation.iter().map(daily_row).collect();
        let expected =
            kittiwake_qos::aggregate::score_indexers(&stored_only, &AggregateOpts::today(TODAY))
                .into_iter()
                .find(|q| q.indexer == A)
                .unwrap()
                .quality
                .q_score;

        let (ctx, _server) = ctx_with(Refusing {
            refuse: "qos_allocation_daily",
            unready: false,
        })
        .await;
        let score = indexer_qos_score(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .expect("the stored window scores");
        assert_eq!(score.entry.value["latest"]["q_score"], json!(expected));
        assert!(score.entry.provenance.degraded);
        let deployments = indexer_qos_deployments(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .expect("the stored window's deployments");
        assert_eq!(
            deployments.entry.value["deployments"][0]["served_share"]
                .as_f64()
                .unwrap(),
            800.0 / 1_100.0
        );
        assert!(deployments.entry.provenance.degraded);

        let (ctx, _server) = ctx_with(Refusing {
            refuse: "lodestar_allocations",
            unready: false,
        })
        .await;
        let score = indexer_qos_score(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .expect("no allocations is not no score");
        assert_eq!(score.entry.value["latest"]["served_gap"], Value::Null);
        assert!(score.entry.value["latest"]["q_score"].is_number());
        assert!(score.entry.provenance.degraded);
    }

    fn every_statement_names_one_day_at_most(seen: &[String]) {
        assert!(!seen.is_empty());
        for q in seen {
            let days = sql::days_named(q);
            assert!(days.len() <= 1, "spans days: {q}");
            assert!(
                days.iter().all(|d| *d == TODAY_UNIX),
                "asked the nest for a closed day: {q}"
            );
            assert!(
                !q.contains("BETWEEN") && !q.contains("day >") && !q.contains("day <"),
                "a range: {q}"
            );
        }
    }

    #[tokio::test]
    async fn closed_days_come_from_the_store_and_only_today_from_the_nest() {
        let (ctx, seen, _server) = ctx().await;
        let store = MemStore {
            indexer: vec![
                stored(A, TODAY_UNIX - 3, 1_000.0),
                stored(A, TODAY_UNIX - 1, 2_000.0),
                // A stale copy of today, which must lose to the nest's.
                stored(A, TODAY_UNIX, 9_999.0),
            ],
            ..MemStore::default()
        };
        let read = indexer_qos(&ctx, Some(&store), A, TODAY_UNIX - 3, TODAY_UNIX)
            .await
            .unwrap();
        let out = &*read.entry.value;
        let dates: Vec<&str> = out["qos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["date"].as_str().unwrap())
            .collect();
        assert_eq!(dates, vec!["2026-09-09", "2026-09-11", "2026-09-12"]);
        assert_eq!(out["qos"][0]["queryCount"], json!(1_000.0));
        assert_eq!(
            out["qos"][2]["queryCount"],
            json!(500.0),
            "today is the nest's"
        );
        assert_eq!(out["qos"][2]["secondsBehind"], json!(7.0));
        assert_eq!(out["qos"][2]["gatewayIds"], json!(["0xgw"]));
        assert_eq!(
            out["freshness"]["publisherLastPostAt"],
            json!(1_789_172_000_i64)
        );
        assert_eq!(
            out["summary"]["days"],
            json!(3),
            "the day with no row is absent"
        );
        every_statement_names_one_day_at_most(&seen.lock().unwrap());
    }

    /// The job writes today on every run, so the store may hold a copy of it; the route must still
    /// chart the nest's, which is newer than any copy.
    #[test]
    fn a_stored_copy_of_today_loses_to_the_nests_answer() {
        let live = json!({"days": [{"date": unix_day_date(TODAY_UNIX), "query_count": 500.0, "buckets": 12}]});
        let input = qos_input(
            &[
                stored(A, TODAY_UNIX - 1, 1.0),
                stored(A, TODAY_UNIX, 9_999.0),
            ],
            &live,
            TODAY_UNIX,
        );
        let counts: Vec<f64> = input["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["query_count"].as_f64().unwrap())
            .collect();
        assert_eq!(counts, vec![1.0, 500.0]);
    }

    #[tokio::test]
    async fn a_day_the_store_and_the_nest_both_lack_is_absent_rather_than_zero() {
        let (ctx, _seen, _server) = ctx().await;
        let store = MemStore {
            indexer: vec![stored(B, TODAY_UNIX - 2, 10.0)],
            ..MemStore::default()
        };
        let out = indexer_qos(&ctx, Some(&store), B, TODAY_UNIX - 5, TODAY_UNIX)
            .await
            .unwrap();
        let points = out.entry.value["qos"].as_array().unwrap().clone();
        assert_eq!(points.len(), 1, "{points:?}");
        assert_eq!(points[0]["date"], json!("2026-09-10"));
    }

    #[tokio::test]
    async fn the_score_reads_stored_peers_and_todays_rows_one_day_at_a_time() {
        let (ctx, seen, _server) = ctx().await;
        let store = MemStore {
            allocation: vec![
                alloc_day(A, TODAY_UNIX - 1, 800.0),
                alloc_day(B, TODAY_UNIX - 1, 300.0),
            ],
            ..MemStore::default()
        };
        let read = indexer_qos_score(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .unwrap();
        let rows: Vec<QosDailyRow> = store.allocation.iter().map(daily_row).chain([
            QosDailyRow::from(&allocation_daily_from(&json!({"indexer": A, "deployment": DEP, "chain": "arbitrum-one", "gateway": "0xgw", "unix_day": TODAY_UNIX, "query_count": 100.0, "num_200": 100.0, "latency_ms": 120.0, "blocks_behind": 3.0, "total_query_fees": 0.1, "buckets": 12})).unwrap()),
            QosDailyRow::from(&allocation_daily_from(&json!({"indexer": B, "deployment": DEP, "chain": "arbitrum-one", "gateway": "0xgw", "unix_day": TODAY_UNIX, "query_count": 100.0, "num_200": 50.0, "latency_ms": 120.0, "blocks_behind": 3.0, "total_query_fees": 0.1, "buckets": 12})).unwrap()),
        ]).collect();
        let expected =
            kittiwake_qos::aggregate::score_indexers(&rows, &AggregateOpts::today(TODAY))
                .into_iter()
                .find(|q| q.indexer == A)
                .unwrap()
                .quality
                .q_score;
        assert_eq!(read.entry.value["latest"]["q_score"], json!(expected));
        let deployments = indexer_qos_deployments(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .unwrap();
        let share = deployments.entry.value["deployments"][0]["served_share"]
            .as_f64()
            .unwrap();
        assert_eq!(share, 900.0 / 1_300.0);
        every_statement_names_one_day_at_most(&seen.lock().unwrap());
    }

    #[tokio::test]
    async fn without_a_database_the_routes_are_unavailable_and_ask_the_nest_nothing() {
        let (ctx, seen, _server) = ctx().await;
        for result in [
            indexer_qos(&ctx, None, A, TODAY_UNIX - 89, TODAY_UNIX).await,
            indexer_qos_score(&ctx, None, A, TODAY_UNIX).await,
            indexer_qos_deployments(&ctx, None, A, TODAY_UNIX).await,
            indexer_qos_qualifying_days(&ctx, None, A, TODAY_UNIX).await,
        ] {
            assert!(matches!(result, Err(ApiError::Unavailable(_))));
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    struct Truncating;

    impl Respond for Truncating {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if req.url.path().ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "count": 1, "rows": [{"date": unix_day_date(TODAY_UNIX), "query_count": 1.0}], "truncated": true
            }))
        }
    }

    #[tokio::test]
    async fn a_truncated_today_is_left_out_rather_than_charted() {
        let (ctx, _server) = ctx_with(Truncating).await;
        let store = MemStore {
            indexer: vec![stored(A, TODAY_UNIX - 1, 2_000.0)],
            ..MemStore::default()
        };
        let read = indexer_qos(&ctx, Some(&store), A, TODAY_UNIX - 1, TODAY_UNIX)
            .await
            .unwrap();
        assert_eq!(dates(&read), vec!["2026-09-11"]);
        assert!(read.entry.provenance.degraded);
    }

    const DEP2: &str = "QmTXzATwNfgGVukV1fX2T6xw9f6LAYRVWpsdXyRWzUR2H9";

    fn served(day: i64, deployment: &str, num_200: f64) -> Served {
        Served {
            day,
            deployment: deployment.into(),
            num_200,
        }
    }

    fn grt(n: i128) -> i128 {
        n * 1_000_000_000_000_000_000
    }

    fn signal(entries: &[(&str, &[(i64, i128)])]) -> SignalDeltas {
        entries
            .iter()
            .map(|(d, deltas)| (d.to_string(), deltas.iter().copied().collect()))
            .collect()
    }

    fn day_of<'a>(out: &'a Value, date: &str) -> &'a Value {
        out["days"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["date"] == json!(date))
            .unwrap_or_else(|| panic!("no {date} in {out}"))
    }

    /// The count is distinct deployments with a 200 that day: two gateways on one deployment are one,
    /// and a deployment that answered only errors is none. The rows cannot apply the oracle's latency
    /// and freshness bars, so the answer says the count bounds the oracle's from above.
    #[test]
    fn a_day_counts_distinct_deployments_that_answered_a_200() {
        let rows = [
            served(TODAY_UNIX - 1, DEP, 10.0),
            served(TODAY_UNIX - 1, DEP, 5.0),
            served(TODAY_UNIX - 1, DEP2, 0.0),
        ];
        let measured = BTreeSet::from([TODAY_UNIX - 1]);
        let sig = signal(&[(DEP, &[(TODAY_UNIX - 30, grt(600))])]);
        let out = qualifying_days_answer(&rows, &measured, TODAY_UNIX, Ok(&sig));
        let day = day_of(&out, "2026-09-11");
        assert_eq!(day["count_without_floor"], json!(1));
        assert_eq!(day["count"], json!(1));
        assert_eq!(day["deployments"][0]["deployment_id"], json!(DEP));
        assert_eq!(day["deployments"][0]["num_200"], json!(15.0));
        assert_eq!(out["qualifying_test_applied"], json!(false));
        assert_eq!(out["bound"], json!("upper"));
        assert!(
            out["qualifying_test_reason"]
                .as_str()
                .unwrap()
                .contains("5,000 ms")
        );
        assert_eq!(out["window_days"], json!(28));
    }

    /// Signal is read as of each day's end, so a deployment that crossed the floor mid-window counts
    /// from that day on and not before, and one that fell under it stops counting.
    #[test]
    fn the_floor_is_applied_with_the_signal_each_day_had() {
        let rows = [
            served(TODAY_UNIX - 3, DEP, 1.0),
            served(TODAY_UNIX - 3, DEP2, 1.0),
            served(TODAY_UNIX - 1, DEP, 1.0),
            served(TODAY_UNIX - 1, DEP2, 1.0),
        ];
        let measured = BTreeSet::from([TODAY_UNIX - 3, TODAY_UNIX - 1]);
        let sig = signal(&[
            (
                DEP,
                &[(TODAY_UNIX - 28, grt(400)), (TODAY_UNIX - 2, grt(100))],
            ),
            (
                DEP2,
                &[(TODAY_UNIX - 28, grt(900)), (TODAY_UNIX - 2, -grt(500))],
            ),
        ]);
        let out = qualifying_days_answer(&rows, &measured, TODAY_UNIX, Ok(&sig));
        assert_eq!(out["signal_floor_applied"], json!(true));
        assert_eq!(out["signal_floor_reason"], Value::Null);
        assert_eq!(out["signal_floor_grt"], json!(500));
        let before = day_of(&out, "2026-09-09");
        assert_eq!(before["count_without_floor"], json!(2));
        assert_eq!(before["count"], json!(1));
        let over: Vec<&Value> = before["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["over_floor"] == json!(true))
            .map(|d| &d["deployment_id"])
            .collect();
        assert_eq!(over, vec![&json!(DEP2)]);
        let after = day_of(&out, "2026-09-11");
        assert_eq!(after["count"], json!(1));
        let dep = after["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["deployment_id"] == json!(DEP))
            .unwrap();
        assert_eq!(dep["over_floor"], json!(true));
        assert_eq!(dep["signal_grt"], json!(500.0));
    }

    /// Without signal the count is the unfiltered one, and the answer says the floor was not applied
    /// and why, rather than counting every deployment as over it.
    #[test]
    fn without_signal_the_floor_is_not_applied_and_the_answer_says_so() {
        let rows = [served(TODAY_UNIX - 1, DEP, 3.0)];
        let measured = BTreeSet::from([TODAY_UNIX - 1]);
        let out = qualifying_days_answer(
            &rows,
            &measured,
            TODAY_UNIX,
            Err("the allocations nest did not answer"),
        );
        assert_eq!(out["signal_floor_applied"], json!(false));
        assert_eq!(
            out["signal_floor_reason"],
            json!("the allocations nest did not answer")
        );
        let day = day_of(&out, "2026-09-11");
        assert_eq!(day["count"], json!(1));
        assert_eq!(day["deployments"][0]["over_floor"], Value::Null);
        assert_eq!(day["deployments"][0]["signal_grt"], Value::Null);
    }

    /// A computed day with nothing for this indexer is a measured zero; a day nobody computed is a
    /// gap, and today is partial because its buckets are still arriving.
    #[test]
    fn a_measured_day_without_rows_is_zero_and_an_unmeasured_one_is_absent() {
        let rows = [served(TODAY_UNIX, DEP, 1.0)];
        let measured = BTreeSet::from([TODAY_UNIX - 2, TODAY_UNIX]);
        let sig = signal(&[(DEP, &[])]);
        let out = qualifying_days_answer(&rows, &measured, TODAY_UNIX, Ok(&sig));
        let dates: Vec<&str> = out["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["date"].as_str().unwrap())
            .collect();
        assert_eq!(dates, vec!["2026-09-10", "2026-09-12"]);
        assert_eq!(day_of(&out, "2026-09-10")["count"], json!(0));
        assert_eq!(day_of(&out, "2026-09-10")["partial"], json!(false));
        let today = day_of(&out, "2026-09-12");
        assert_eq!(today["partial"], json!(true));
        assert_eq!(today["count_without_floor"], json!(1));
        assert_eq!(today["count"], json!(0), "no curation events is no signal");
    }

    #[test]
    fn signal_rows_are_keyed_by_cid_and_kept_in_wei() {
        let hex = shape::ipfs_hash_to_bytes32(DEP).unwrap();
        let sig = signal_deltas_from(&[
            json!({"deployment": hex, "unix_day": 20_000, "delta": "170141183460469231731687303715884105727"}),
            json!({"deployment": hex, "unix_day": "20001", "delta": "-1"}),
        ]);
        assert_eq!(sig[DEP][&20_000], i128::MAX);
        assert_eq!(sig[DEP][&20_001], -1);
    }

    /// Closed days from the store, today from the nest's shared statement, and signal from the
    /// allocations nest. The QoS nest is still asked for today alone.
    #[tokio::test]
    async fn the_route_joins_stored_days_today_and_signal_as_of_each_day() {
        let (ctx, seen, _server) = ctx().await;
        let store = MemStore {
            allocation: vec![
                alloc_day(A, TODAY_UNIX - 3, 800.0),
                alloc_day(B, TODAY_UNIX - 3, 300.0),
            ],
            days: vec![TODAY_UNIX - 3, TODAY_UNIX - 2, TODAY_UNIX - 1],
            ..MemStore::default()
        };
        let read = indexer_qos_qualifying_days(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .unwrap();
        let out = &*read.entry.value;
        assert_eq!(out["signal_floor_applied"], json!(true));
        let dates: Vec<&str> = out["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["date"].as_str().unwrap())
            .collect();
        assert_eq!(
            dates,
            vec!["2026-09-09", "2026-09-10", "2026-09-11", "2026-09-12"]
        );
        let three_ago = day_of(out, "2026-09-09");
        assert_eq!(three_ago["count_without_floor"], json!(1));
        assert_eq!(three_ago["count"], json!(0), "400 GRT then");
        assert_eq!(day_of(out, "2026-09-10")["count"], json!(0));
        let today = day_of(out, "2026-09-12");
        assert_eq!(today["count"], json!(1), "550 GRT by today");
        assert_eq!(today["deployments"][0]["num_200"], json!(100.0));
        assert!(!read.entry.provenance.degraded);
        every_statement_names_one_day_at_most(&seen.lock().unwrap());
    }

    #[tokio::test]
    async fn a_refused_signal_read_serves_the_counts_without_the_floor() {
        let (ctx, _server) = ctx_with(Refusing {
            refuse: "curation__signalled",
            unready: false,
        })
        .await;
        let store = MemStore {
            allocation: vec![alloc_day(A, TODAY_UNIX - 1, 800.0)],
            days: vec![TODAY_UNIX - 1],
            ..MemStore::default()
        };
        let read = indexer_qos_qualifying_days(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .unwrap();
        let out = &*read.entry.value;
        assert_eq!(out["signal_floor_applied"], json!(false));
        assert!(out["signal_floor_reason"].is_string());
        assert_eq!(day_of(out, "2026-09-11")["count"], json!(1));
        assert!(read.entry.provenance.degraded);
    }

    /// The nest refuses a statement over 16 KiB, so a busy indexer's deployments are asked about two
    /// hundred at a time.
    #[tokio::test]
    async fn signal_for_many_deployments_is_asked_in_statements_the_nest_accepts() {
        let (ctx, seen, _server) = ctx().await;
        let allocation = (1..=450)
            .map(|n| QosAllocationDay {
                deployment: shape::deployment_ipfs_hash(&format!("{n:064x}")).unwrap(),
                ..alloc_day(A, TODAY_UNIX - 1, 1.0)
            })
            .collect();
        let store = MemStore {
            allocation,
            days: vec![TODAY_UNIX - 1],
            ..MemStore::default()
        };
        let read = indexer_qos_qualifying_days(&ctx, Some(&store), A, TODAY_UNIX)
            .await
            .unwrap();
        assert_eq!(read.entry.value["signal_floor_applied"], json!(true));
        assert_eq!(
            day_of(&read.entry.value, "2026-09-11")["count_without_floor"],
            json!(450)
        );
        let signal: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("curation__signalled"))
            .cloned()
            .collect();
        assert_eq!(signal.len(), 3);
        assert!(
            signal.iter().all(|q| q.len() < 16_384),
            "a statement over the nest's limit"
        );
    }
}
