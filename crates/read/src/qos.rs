//! Edge & Node's gateway QoS per indexer, and the QoS Quality score computed over it.
//!
//! Read from qos-reo-nest, which indexes the oracle's own postings on Gnosis. Only its daily views
//! are read - `qos_indexer_daily`, `qos_allocation_daily` and the seconds-behind view - because
//! those names and columns are the contract; the documents and five-minute rows beneath them are the
//! nest's to reshape. `qos_deployment_daily` has no reader: served share is attempts over every
//! indexer's attempts, not over the gateway's query count.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kittiwake_cache::CachePolicy;
use kittiwake_qos::aggregate::{
    AggregateOpts, AllocationRow, Phase2Metric, QosDailyRow, aggregate_indexer_metrics,
    compute_phase2_metrics,
};
use kittiwake_qos::nest::{AllocationDaily, GRAPH_EPOCH_DAYS};
use kittiwake_qos::score::{
    DeploymentContribution, QualityDetail, QualityOpts, compute_quality_detail, qos_grade,
};
use kittiwake_types::{ApiError, NestId};
use serde_json::{Value, json};

use crate::routes::{Part, many};
use crate::{Fetcher, Read, ReadCtx, shape, sql};

/// The score's window, as the incumbent's cron and its deployment breakdown both used it.
pub const SCORE_WINDOW_DAYS: i64 = 30;

/// Days of score history the panel draws.
pub const SCORE_HISTORY_DAYS: i64 = 30;

/// Days per cohort statement. The nest caps an answer at 50,000 rows; on 2026-09-07 the peers on
/// Ellipfra's 1,377 deployments were 4,389 rows, so five days fits.
pub const DAYS_PER_SLICE: i64 = 5;

/// The oracle posts every five minutes and its documents resolve about half an hour later, so a
/// fresher cache would only re-ask for the same buckets.
const POLL: Duration = Duration::from_secs(300);

const TIMEOUT: Duration = Duration::from_secs(45);

/// Part names for the slices: enough for the score window plus its history.
const ROW_SLICES: [&str; 13] = [
    "rows_0", "rows_1", "rows_2", "rows_3", "rows_4", "rows_5", "rows_6", "rows_7", "rows_8",
    "rows_9", "rows_10", "rows_11", "rows_12",
];

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

fn graph_day_date(day: i64) -> String {
    chrono::DateTime::from_timestamp((day + GRAPH_EPOCH_DAYS) * 86_400, 0)
        .map(|d| d.date_naive().to_string())
        .unwrap_or_default()
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

// ---------------------------------------------------------------------------------------------
// Query Performance
// ---------------------------------------------------------------------------------------------

/// `/api/indexer/[address]/qos`: the Query Performance card, from `since_day` (a unix day number).
pub async fn indexer_qos(ctx: &ReadCtx, addr: &str, since_day: i64) -> Result<Read, ApiError> {
    let parts = vec![
        Part::rows(
            "days",
            NestId::Qos,
            sql::qos_indexer_daily_sql(addr, since_day).ok_or_else(bad_address)?,
        ),
        Part::rows(
            "gateways",
            NestId::Qos,
            sql::qos_indexer_gateways_sql(addr, since_day).ok_or_else(bad_address)?,
        ),
        Part::one("freshness", NestId::Qos, sql::QOS_FRESHNESS_SQL.to_string()),
    ];
    ctx.cached(
        "indexer-qos",
        NestId::Qos,
        json!({"address": addr, "since": since_day}),
        CachePolicy::from_poll_interval(POLL),
        folded(parts, qos_answer),
    )
    .await
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
    let freshness = v.get("freshness").cloned().unwrap_or(Value::Null);
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
        // Two questions: when the publisher last posted to Gnosis, and the newest day this indexer has
        // figures for. A publisher gone quiet shows as the first falling behind; a quiet indexer does not.
        "freshness": {
            "publisherLastPostAt": shape::i(&freshness, "last_post_at"),
            "newestDate": latest_date,
            "newestDateBuckets": latest.and_then(|r| f(r, "buckets")).map(|b| b as i64),
        },
    })
}

// ---------------------------------------------------------------------------------------------
// QoS Quality
// ---------------------------------------------------------------------------------------------

/// For one indexer from `from_day` to `to_day` (unix day numbers): every indexer's rows on the
/// deployments it served, sliced, and the open allocations on its deployments.
fn cohort_parts(addr: &str, from_day: i64, to_day: i64) -> Result<Vec<Part>, ApiError> {
    let mut parts = Vec::new();
    let mut start = from_day;
    for rows in ROW_SLICES {
        if start > to_day {
            break;
        }
        let end = (start + DAYS_PER_SLICE - 1).min(to_day);
        parts.push(Part::rows(
            rows,
            NestId::Qos,
            sql::qos_cohort_rows_sql(addr, from_day, start, end).ok_or_else(bad_address)?,
        ));
        start = end + 1;
    }
    if start <= to_day {
        return Err(ApiError::Internal(
            "the score window needs more slices than are named".into(),
        ));
    }
    parts.push(Part::rows(
        "allocations",
        NestId::Alloc,
        sql::indexer_allocation_shares_sql(addr).ok_or_else(bad_address)?,
    ));
    Ok(parts)
}

/// A `qos_allocation_daily` row as the nest answered it.
pub fn allocation_daily_from(r: &Value) -> Option<AllocationDaily> {
    Some(AllocationDaily {
        indexer: r.get("indexer")?.as_str()?.to_lowercase(),
        deployment: r.get("deployment")?.as_str()?.to_string(),
        chain: r.get("chain").and_then(Value::as_str).map(str::to_string),
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
    pub fn from_answer(v: &Value) -> Self {
        let rows_of = |names: &[&str]| -> Vec<Value> {
            names
                .iter()
                .filter_map(|k| v.get(*k).and_then(Value::as_array))
                .flatten()
                .cloned()
                .collect()
        };
        Self {
            rows: rows_of(&ROW_SLICES)
                .iter()
                .filter_map(allocation_daily_from)
                .map(|r| QosDailyRow::from(&r))
                .collect(),
            allocations: rows_of(&["allocations"])
                .iter()
                .filter_map(allocation_row_from)
                .collect(),
        }
    }

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

/// `/api/indexer/[address]/qos-score`: the latest score with its breakdown, and one score per day.
pub async fn indexer_qos_score(
    ctx: &ReadCtx,
    addr: &str,
    today_unix_day: i64,
) -> Result<Read, ApiError> {
    let from = today_unix_day - SCORE_WINDOW_DAYS - (SCORE_HISTORY_DAYS - 1);
    let parts = cohort_parts(addr, from, today_unix_day)?;
    let who = addr.to_string();
    let today = today_unix_day - GRAPH_EPOCH_DAYS;
    ctx.cached(
        "indexer-qos-score",
        NestId::Qos,
        json!({"address": addr, "today": today_unix_day}),
        CachePolicy::from_poll_interval(POLL),
        folded(parts, move |v| {
            score_answer(&Cohort::from_answer(v), &who, today)
        }),
    )
    .await
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
    addr: &str,
    today_unix_day: i64,
) -> Result<Read, ApiError> {
    let parts = cohort_parts(addr, today_unix_day - SCORE_WINDOW_DAYS, today_unix_day)?;
    let who = addr.to_string();
    let today = today_unix_day - GRAPH_EPOCH_DAYS;
    ctx.cached(
        "indexer-qos-deployments",
        NestId::Qos,
        json!({"address": addr, "today": today_unix_day}),
        CachePolicy::from_poll_interval(POLL),
        folded(parts, move |v| {
            deployments_answer(&Cohort::from_answer(v), &who, today)
        }),
    )
    .await
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

    const A: &str = "0x00000000000000000000000000000000000000aa";
    const B: &str = "0x00000000000000000000000000000000000000bb";
    const DEP: &str = "QmdT8aMbj72gJt6RtSQFhhWbyG4C7vwGkghnvfT8Y8zLgJ";
    /// 2026-09-12, as a Graph day number.
    const TODAY: i64 = 20_708 - GRAPH_EPOCH_DAYS;

    fn day(date: &str, queries: f64, ok: f64, latency: f64, buckets: i64) -> Value {
        json!({
            "date": date, "query_count": queries, "num_200": ok, "success_rate": ok / queries,
            "latency_ms": latency, "blocks_behind": 3.0, "total_query_fees": queries * 0.001,
            "avg_query_fee": 0.001, "allocations": 1, "buckets": buckets, "bad_buckets": 0,
            "worst_bucket_start": null, "worst_bucket_success_rate": null, "worst_bucket_queries": null,
            "seconds_behind_peers": 12.0, "share_queries_over_5min_behind": 0.0, "share_with_block_time": 1.0,
        })
    }

    #[test]
    fn a_day_the_nest_has_no_row_for_is_absent_rather_than_zero() {
        let out = qos_answer(&json!({
            "days": [day("2026-09-01", 10.0, 10.0, 100.0, 288), day("2026-09-03", 10.0, 10.0, 100.0, 288)],
            "gateways": [{"date": "2026-09-03", "gateways": "0xgw1,0xgw2"}],
            "freshness": {"last_post_at": 1_788_829_815_u64},
        }));
        let dates: Vec<Value> = out["qos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["date"].clone())
            .collect();
        assert_eq!(dates, vec![json!("2026-09-01"), json!("2026-09-03")]);
        assert_eq!(out["summary"]["days"], json!(2));
        assert_eq!(out["qos"][1]["gatewayIds"], json!(["0xgw1", "0xgw2"]));
        assert_eq!(
            out["freshness"]["publisherLastPostAt"],
            json!(1_788_829_815_u64)
        );
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
        let out = qos_answer(&json!({"days": [], "gateways": [], "freshness": null}));
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

    fn unix(day: i64) -> i64 {
        day + GRAPH_EPOCH_DAYS
    }

    fn row(ix: &str, unix_day: i64, queries: f64, ok: f64) -> Value {
        json!({
            "indexer": ix, "deployment": DEP, "chain": "arbitrum-one", "gateway": "0xgw",
            "unix_day": unix_day, "query_count": queries, "num_200": ok, "latency_ms": 120.0,
            "blocks_behind": 3.0, "total_query_fees": 0.5, "buckets": 288,
        })
    }

    /// Two indexers attempted 1,100 queries on a deployment where the gateway answered 1,000, because
    /// it hedged 100 across both. Against the gateway's count their shares summed to 1.1.
    fn hedged() -> Cohort {
        let hex = shape::ipfs_hash_to_bytes32(DEP).unwrap();
        Cohort::from_answer(&json!({
            "rows_0": [row(A, unix(TODAY), 800.0, 700.0), row(B, unix(TODAY), 300.0, 300.0)],
            "allocations": [
                {"indexer": A, "subgraph_deployment": hex, "allocated_tokens": "1000000000000000000000"},
                {"indexer": B, "subgraph_deployment": hex, "allocated_tokens": "3000000000000000000000"},
            ],
        }))
    }

    #[test]
    fn served_share_is_against_every_indexers_attempts_and_the_gap_follows() {
        let c = hedged();
        let share = |ix: &str| {
            deployments_answer(&c, ix, TODAY)["deployments"][0]["served_share"]
                .as_f64()
                .unwrap()
        };
        assert_eq!(share(A), 800.0 / 1_100.0);
        assert_eq!(share(B), 300.0 / 1_100.0);
        assert!((share(A) + share(B) - 1.0).abs() < 1e-12);
        // An allocation share of 1,000 in 4,000 against a served share of 8 in 11.
        let gap = c.phase2(A, TODAY).unwrap().served_gap;
        assert!((gap - (0.25 - 800.0 / 1_100.0)).abs() < 1e-12, "{gap}");
    }

    #[test]
    fn the_route_scores_exactly_as_the_ported_scorer_does() {
        let c = hedged();
        let direct =
            kittiwake_qos::aggregate::score_indexers(&c.rows, &AggregateOpts::today(TODAY));
        let expected = direct
            .iter()
            .find(|q| q.indexer == A)
            .unwrap()
            .quality
            .q_score;
        let out = score_answer(&c, A, TODAY);
        assert_eq!(out["latest"]["q_score"], json!(expected));
        assert_eq!(out["latest"]["day"], json!("2026-09-12"));
        assert_eq!(out["latest"]["grade"], json!(qos_grade(expected).grade));
    }

    #[test]
    fn history_holds_a_score_for_each_day_the_window_still_saw_the_indexer() {
        let c = Cohort::from_answer(&json!({"rows_0": [
            row(A, unix(TODAY - 2), 500.0, 500.0),
            row(B, unix(TODAY - 2), 500.0, 400.0),
        ]}));
        let out = score_answer(&c, A, TODAY);
        assert_eq!(out["daily"].as_array().unwrap().len(), 3, "{out}");
        assert!(out["latest"].is_object());

        let none = score_answer(&Cohort::default(), A, TODAY);
        assert_eq!(none["latest"], Value::Null);
        assert_eq!(none["daily"], json!([]));
        assert_eq!(
            deployments_answer(&Cohort::default(), A, TODAY)["total"],
            Value::Null
        );
    }

    #[test]
    fn the_slices_cover_the_score_window_and_its_history_once_each() {
        let today = 20_708;
        let from = today - SCORE_WINDOW_DAYS - (SCORE_HISTORY_DAYS - 1);
        let parts = cohort_parts(A, from, today).unwrap();
        let mut next = from;
        for p in parts.iter().filter(|p| p.name.starts_with("rows_")) {
            let days: Vec<i64> = p
                .sql
                .split("DATE '1970-01-01' + ")
                .skip(1)
                .map(|s| {
                    s.split(|c: char| !c.is_ascii_digit())
                        .next()
                        .unwrap()
                        .parse()
                        .unwrap()
                })
                .collect();
            assert_eq!(days[0], next, "a slice starts where the last ended");
            next = days[1] + 1;
        }
        assert_eq!(next, today + 1, "the slices reach today");
        assert!(
            parts
                .iter()
                .all(|p| !p.sql.contains("qos_deployment_daily")),
            "served share needs no gateway totals"
        );
        assert!(
            cohort_parts(A, 0, 1_000).is_err(),
            "a range with more slices than names is refused"
        );
    }

    struct Truncating;

    impl wiremock::Respond for Truncating {
        fn respond(&self, req: &wiremock::Request) -> wiremock::ResponseTemplate {
            if req.url.path().ends_with("/ready") {
                return wiremock::ResponseTemplate::new(200).set_body_json(json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "count": 1, "rows": [{"date": "2026-09-07", "query_count": 1.0}], "truncated": true
            }))
        }
    }

    #[tokio::test]
    async fn a_truncated_answer_is_refused_rather_than_charted() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(Truncating)
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
        let Err(e) = indexer_qos(&ctx, A, 20_700).await else {
            panic!("a truncated answer was charted");
        };
        assert!(format!("{e:?}").contains("truncated"), "{e:?}");
    }
}
