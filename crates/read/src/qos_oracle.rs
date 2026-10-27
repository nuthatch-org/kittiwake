//! Edge & Node's QoS oracle as they publish it, for `/api/qos/oracle`.
//!
//! The figures are theirs and are labelled as theirs. They are also checked: qos-reo-nest decodes
//! the same Gnosis postings independently, so the two readings of one day should agree, and the
//! answer says when they do not rather than leaving that for a reader to discover.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

use kittiwake_qos::nest::GRAPH_EPOCH_DAYS;

/// Above this relative difference the two readings are reported as disagreeing. The nest keeps
/// doubles and the subgraph keeps decimals, so exact equality is not the test.
pub const TOLERANCE: f64 = 0.01;

/// The oracle's day number for a unix day.
pub fn oracle_day(unix_day: i64) -> i64 {
    unix_day - GRAPH_EPOCH_DAYS
}

/// The unix day of an oracle day number.
pub fn unix_day(oracle_day: i64) -> i64 {
    oracle_day + GRAPH_EPOCH_DAYS
}

/// The subgraph's `BigDecimal`s arrive as strings.
fn decimal<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    Ok(s.and_then(|s| s.parse().ok()).unwrap_or(0.0))
}

/// `IndexerDailyDataPoint`: one indexer through one gateway on one day.
#[derive(Debug, Clone, Deserialize)]
pub struct IndexerDay {
    pub indexer_wallet: String,
    #[serde(default)]
    pub indexer_url: Option<String>,
    #[serde(deserialize_with = "decimal")]
    pub query_count: f64,
    #[serde(deserialize_with = "decimal")]
    pub total_query_fees: f64,
    #[serde(deserialize_with = "decimal")]
    pub avg_indexer_latency_ms: f64,
    #[serde(deserialize_with = "decimal")]
    pub proportion_indexer_200_responses: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentRef {
    pub id: String,
}

/// `QueryDailyDataPoint`: what the gateway was asked of one deployment on one day.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryDay {
    /// Null on the one row a day for queries the gateway could not attach to any deployment.
    #[serde(rename = "subgraphDeployment")]
    pub deployment: Option<DeploymentRef>,
    #[serde(default)]
    pub chain_id: Option<String>,
    #[serde(deserialize_with = "decimal")]
    pub query_count: f64,
    #[serde(deserialize_with = "decimal")]
    pub total_query_fees: f64,
    #[serde(deserialize_with = "decimal")]
    pub gateway_query_success_rate: f64,
    #[serde(deserialize_with = "decimal")]
    pub avg_gateway_latency_ms: f64,
    #[serde(deserialize_with = "decimal")]
    pub user_attributed_error_rate: f64,
}

/// The same day as qos-reo-nest decoded it, from `qos_indexer_day`.
#[derive(Debug, Clone, Copy)]
pub struct NestDay {
    pub indexers: i64,
    pub query_count: f64,
    pub total_query_fees: f64,
    pub settled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Totals {
    pub query_count: f64,
    pub total_query_fees: f64,
    /// Query-weighted. `None` when nothing was queried, which is not a success rate of zero.
    pub success_rate: Option<f64>,
    pub avg_latency_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IndexerSummary {
    pub indexer: String,
    pub url: Option<String>,
    pub gateways: usize,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeploymentSummary {
    pub deployment: String,
    pub chain: Option<String>,
    pub user_error_rate: Option<f64>,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Agree,
    Disagree,
    NotCompared,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reconciliation {
    pub verdict: Verdict,
    pub reason: Option<String>,
    pub oracle_query_count: f64,
    pub nest_query_count: Option<f64>,
    pub query_count_delta: Option<f64>,
    pub oracle_total_query_fees: f64,
    pub nest_total_query_fees: Option<f64>,
    pub fees_delta: Option<f64>,
    pub oracle_indexers: usize,
    pub nest_indexers: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub day: i64,
    pub date: String,
    /// The gateway's view, from the query topic.
    pub network: Totals,
    pub deployments: usize,
    /// Counted in `network`, but belonging to no deployment.
    pub unattributed_query_count: f64,
    /// The indexers' view, from the indexer topic. It counts attempts, so it can exceed `network`.
    pub indexer_attempts: Totals,
    pub top_indexers: Vec<IndexerSummary>,
    pub top_deployments: Vec<DeploymentSummary>,
    pub reconciliation: Reconciliation,
}

/// Sums and query-weighted means. A weight of zero leaves the means unknown, not zero.
#[derive(Default)]
struct Acc {
    queries: f64,
    fees: f64,
    success: f64,
    latency: f64,
}

impl Acc {
    fn add(&mut self, queries: f64, fees: f64, success: f64, latency: f64) {
        self.queries += queries;
        self.fees += fees;
        self.success += success * queries;
        self.latency += latency * queries;
    }

    fn totals(&self) -> Totals {
        let mean = |x: f64| (self.queries > 0.0).then(|| x / self.queries);
        Totals {
            query_count: self.queries,
            total_query_fees: self.fees,
            success_rate: mean(self.success),
            avg_latency_ms: mean(self.latency),
        }
    }
}

fn delta(ours: f64, theirs: f64) -> f64 {
    let base = ours.abs().max(theirs.abs());
    if base == 0.0 {
        0.0
    } else {
        (theirs - ours) / base
    }
}

pub fn reconcile(indexers: &[IndexerDay], nest: Option<NestDay>) -> Reconciliation {
    let mut acc = Acc::default();
    let mut wallets = std::collections::BTreeSet::new();
    for r in indexers {
        acc.add(r.query_count, r.total_query_fees, 0.0, 0.0);
        wallets.insert(r.indexer_wallet.to_lowercase());
    }
    let mut out = Reconciliation {
        verdict: Verdict::NotCompared,
        reason: None,
        oracle_query_count: acc.queries,
        nest_query_count: None,
        query_count_delta: None,
        oracle_total_query_fees: acc.fees,
        nest_total_query_fees: None,
        fees_delta: None,
        oracle_indexers: wallets.len(),
        nest_indexers: None,
    };
    let Some(nest) = nest else {
        out.reason = Some("qos-reo-nest has not stored this day yet".into());
        return out;
    };
    out.nest_query_count = Some(nest.query_count);
    out.nest_total_query_fees = Some(nest.total_query_fees);
    out.nest_indexers = Some(nest.indexers);
    let dq = delta(nest.query_count, acc.queries);
    let df = delta(nest.total_query_fees, acc.fees);
    out.query_count_delta = Some(dq);
    out.fees_delta = Some(df);
    if !nest.settled {
        out.reason = Some("the nest has not settled this day, so it may still be filling".into());
        return out;
    }
    out.verdict = if dq.abs() <= TOLERANCE && df.abs() <= TOLERANCE {
        Verdict::Agree
    } else {
        Verdict::Disagree
    };
    out
}

pub fn summarise(
    unix_day: i64,
    indexers: &[IndexerDay],
    queries: &[QueryDay],
    nest: Option<NestDay>,
    top: usize,
) -> Summary {
    let mut network = Acc::default();
    let mut unattributed = 0.0;
    let mut deployments: Vec<DeploymentSummary> = queries
        .iter()
        .filter_map(|q| {
            network.add(
                q.query_count,
                q.total_query_fees,
                q.gateway_query_success_rate,
                q.avg_gateway_latency_ms,
            );
            let Some(deployment) = &q.deployment else {
                unattributed += q.query_count;
                return None;
            };
            let mut one = Acc::default();
            one.add(
                q.query_count,
                q.total_query_fees,
                q.gateway_query_success_rate,
                q.avg_gateway_latency_ms,
            );
            Some(DeploymentSummary {
                deployment: deployment.id.clone(),
                chain: q.chain_id.clone(),
                user_error_rate: (q.query_count > 0.0).then_some(q.user_attributed_error_rate),
                totals: one.totals(),
            })
        })
        .collect();
    let deployment_count = deployments.len();

    // One row per indexer per gateway; a reader wants one per indexer.
    let mut attempts = Acc::default();
    let mut by_wallet: BTreeMap<String, (Acc, Option<String>, usize)> = BTreeMap::new();
    for r in indexers {
        attempts.add(
            r.query_count,
            r.total_query_fees,
            r.proportion_indexer_200_responses,
            r.avg_indexer_latency_ms,
        );
        let e = by_wallet
            .entry(r.indexer_wallet.to_lowercase())
            .or_insert_with(|| (Acc::default(), None, 0));
        e.0.add(
            r.query_count,
            r.total_query_fees,
            r.proportion_indexer_200_responses,
            r.avg_indexer_latency_ms,
        );
        if e.1.is_none() {
            e.1 = r.indexer_url.clone().filter(|u| !u.is_empty());
        }
        e.2 += 1;
    }
    let mut top_indexers: Vec<IndexerSummary> = by_wallet
        .into_iter()
        .map(|(indexer, (acc, url, gateways))| IndexerSummary {
            indexer,
            url,
            gateways,
            totals: acc.totals(),
        })
        .collect();
    top_indexers.sort_by(|a, b| b.totals.query_count.total_cmp(&a.totals.query_count));
    top_indexers.truncate(top);
    deployments.sort_by(|a, b| b.totals.query_count.total_cmp(&a.totals.query_count));
    deployments.truncate(top);

    Summary {
        day: oracle_day(unix_day),
        date: chrono::DateTime::from_timestamp(unix_day * 86_400, 0)
            .map(|d| d.date_naive().to_string())
            .unwrap_or_default(),
        network: network.totals(),
        deployments: deployment_count,
        unattributed_query_count: unattributed,
        indexer_attempts: attempts.totals(),
        top_indexers,
        top_deployments: deployments,
        reconciliation: reconcile(indexers, nest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(wallet: &str, q: f64, fees: f64, ok: f64, ms: f64) -> IndexerDay {
        IndexerDay {
            indexer_wallet: wallet.into(),
            indexer_url: Some(format!("https://{wallet}/")),
            query_count: q,
            total_query_fees: fees,
            avg_indexer_latency_ms: ms,
            proportion_indexer_200_responses: ok,
        }
    }

    fn qd(id: &str, q: f64, fees: f64, ok: f64, ms: f64) -> QueryDay {
        QueryDay {
            deployment: Some(DeploymentRef { id: id.into() }),
            chain_id: Some("arbitrum-one".into()),
            query_count: q,
            total_query_fees: fees,
            gateway_query_success_rate: ok,
            avg_gateway_latency_ms: ms,
            user_attributed_error_rate: 0.0,
        }
    }

    fn nest(q: f64, fees: f64, settled: bool) -> Option<NestDay> {
        Some(NestDay {
            indexers: 1,
            query_count: q,
            total_query_fees: fees,
            settled,
        })
    }

    #[test]
    fn decimals_arrive_as_strings() {
        let r: IndexerDay = serde_json::from_value(serde_json::json!({
            "indexer_wallet": "0xabc", "indexer_url": "https://x/", "query_count": "40125",
            "total_query_fees": "12.5", "avg_indexer_latency_ms": "1734.19",
            "proportion_indexer_200_responses": "0.83"
        }))
        .unwrap();
        assert_eq!(r.query_count, 40125.0);
        assert_eq!(r.total_query_fees, 12.5);
    }

    #[test]
    fn gateways_merge_into_one_indexer_with_weighted_means() {
        let s = summarise(
            20752,
            &[
                ix("0xA", 300.0, 3.0, 1.0, 100.0),
                ix("0xa", 100.0, 1.0, 0.0, 500.0),
            ],
            &[],
            None,
            10,
        );
        assert_eq!(s.top_indexers.len(), 1);
        let a = &s.top_indexers[0];
        assert_eq!(a.indexer, "0xa");
        assert_eq!(a.gateways, 2);
        assert_eq!(a.totals.query_count, 400.0);
        assert_eq!(a.totals.success_rate, Some(0.75));
        assert_eq!(a.totals.avg_latency_ms, Some(200.0));
    }

    #[test]
    fn nothing_queried_is_unknown_not_zero() {
        let s = summarise(20752, &[], &[qd("Qm1", 0.0, 0.0, 0.0, 0.0)], None, 10);
        assert_eq!(s.network.success_rate, None);
        assert_eq!(s.top_deployments[0].user_error_rate, None);
    }

    #[test]
    fn day_numbers_match_the_oracle() {
        // 2026-10-26 is the oracle's day 2139, read off its own dayStart.
        let s = summarise(20752, &[], &[], None, 10);
        assert_eq!(s.day, 2139);
        assert_eq!(s.date, "2026-10-26");
    }

    #[test]
    fn top_is_by_queries_and_truncated() {
        let s = summarise(
            20752,
            &[],
            &[
                qd("Qm1", 5.0, 0.0, 1.0, 1.0),
                qd("Qm2", 50.0, 0.0, 1.0, 1.0),
                qd("Qm3", 9.0, 0.0, 1.0, 1.0),
            ],
            None,
            2,
        );
        assert_eq!(s.deployments, 3);
        let ids: Vec<_> = s
            .top_deployments
            .iter()
            .map(|d| d.deployment.as_str())
            .collect();
        assert_eq!(ids, ["Qm2", "Qm3"]);
        assert_eq!(s.network.query_count, 64.0);
    }

    #[test]
    fn unattributed_queries_count_toward_the_network_but_name_no_deployment() {
        let none: QueryDay = serde_json::from_value(serde_json::json!({
            "subgraphDeployment": null, "chain_id": null, "query_count": "17399",
            "total_query_fees": "0", "gateway_query_success_rate": "0",
            "avg_gateway_latency_ms": "0", "user_attributed_error_rate": "0"
        }))
        .unwrap();
        let s = summarise(20752, &[], &[qd("Qm1", 1.0, 1.0, 1.0, 1.0), none], None, 10);
        assert_eq!(s.network.query_count, 17400.0);
        assert_eq!(s.unattributed_query_count, 17399.0);
        assert_eq!(s.deployments, 1);
        assert_eq!(s.top_deployments.len(), 1);
    }

    #[test]
    fn readings_within_tolerance_agree() {
        let r = reconcile(
            &[ix("0xa", 1000.0, 10.0, 1.0, 1.0)],
            nest(1005.0, 10.02, true),
        );
        assert_eq!(r.verdict, Verdict::Agree);
    }

    #[test]
    fn readings_apart_disagree() {
        let r = reconcile(
            &[ix("0xa", 1000.0, 10.0, 1.0, 1.0)],
            nest(900.0, 10.0, true),
        );
        assert_eq!(r.verdict, Verdict::Disagree);
        assert!((r.query_count_delta.unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn a_missing_or_unsettled_nest_day_is_not_compared() {
        let rows = [ix("0xa", 1000.0, 10.0, 1.0, 1.0)];
        assert_eq!(reconcile(&rows, None).verdict, Verdict::NotCompared);
        let r = reconcile(&rows, nest(1.0, 1.0, false));
        assert_eq!(r.verdict, Verdict::NotCompared);
        assert!(r.query_count_delta.is_some());
    }
}
