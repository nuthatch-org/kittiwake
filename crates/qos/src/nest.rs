//! The QoS nest's rollups, shaped for the scorer.
//!
//! Both are summed from Edge & Node's five-minute oracle buckets into UTC days. The scorer was
//! written against Lodestar's old `qos_daily` and `deployment_daily`, so this is the only place the
//! two shapes meet.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::aggregate::{DeploymentTotalRow, QosDailyRow};

/// Days are counted from The Graph's mainnet launch, 2020-12-17, as the oracle numbers them.
pub const GRAPH_EPOCH_DAYS: i64 = 18_613;

pub fn day_number(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(86_400) - GRAPH_EPOCH_DAYS
}

/// `qos_allocation_daily`: an indexer on a deployment, chain and gateway for one day, from the
/// `gateway_indexer_attempt_qos_5_minutes_prod_v3` topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationDaily {
    pub indexer: String,
    pub deployment: String,
    /// The oracle's chain name, `None` when a bucket carried none.
    pub chain: Option<String>,
    pub gateway_id: String,
    /// Start of the UTC day, in unix seconds.
    pub day_start: i64,
    pub query_count: f64,
    /// Sum of the buckets' `num_indexer_200_responses`; `None` when no bucket that day published it.
    pub num_200: Option<f64>,
    /// Mean latency weighted by query count.
    pub latency_ms: Option<f64>,
    /// Unweighted mean of bucket means.
    pub blocks_behind: Option<f64>,
    pub total_query_fees: f64,
    /// Buckets seen that day, out of 288.
    pub buckets: u32,
}

impl From<&AllocationDaily> for QosDailyRow {
    /// The old ingest derived `success_count` as proportion times count when the oracle's count was
    /// missing. The nest sums the published count itself, so it is used as it stands.
    fn from(r: &AllocationDaily) -> Self {
        QosDailyRow {
            indexer_address: r.indexer.clone(),
            deployment_id: r.deployment.clone(),
            day_number: day_number(r.day_start),
            query_count: r.query_count,
            success_count: r.num_200,
            avg_latency_ms: r.latency_ms,
            blocks_behind: r.blocks_behind,
            chain_id: r.chain.clone(),
        }
    }
}

/// `qos_deployment_daily`: a deployment on a chain and gateway for one day, from the
/// `gateway_query_result_qos_5_minutes_prod_v3` topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentDaily {
    pub deployment: String,
    pub chain: Option<String>,
    pub gateway_id: String,
    /// Start of the UTC day, in unix seconds.
    pub day_start: i64,
    /// Queries the gateway answered on this deployment, whichever indexer served them.
    pub query_count: f64,
}

/// Served-share denominators: each deployment's gateway query count over the window, summed across
/// chains, gateways and days as the old `deployment_daily` query did.
///
/// Summing indexer attempts instead would count a query the gateway retried on a second indexer
/// twice, and understate every served share on that deployment.
pub fn deployment_totals(rows: &[DeploymentDaily]) -> Vec<DeploymentTotalRow> {
    let mut out: Vec<DeploymentTotalRow> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for r in rows {
        match index.get(r.deployment.as_str()) {
            Some(&i) => out[i].total_query_count += r.query_count,
            None => {
                index.insert(&r.deployment, out.len());
                out.push(DeploymentTotalRow {
                    deployment_id: r.deployment.clone(),
                    total_query_count: r.query_count,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{AggregateOpts, aggregate_indexer_metrics};

    const DAY: i64 = 1_789_171_200;

    fn daily(
        indexer: &str,
        deployment: &str,
        queries: f64,
        num_200: Option<f64>,
    ) -> AllocationDaily {
        AllocationDaily {
            indexer: indexer.into(),
            deployment: deployment.into(),
            chain: Some("arbitrum-one".into()),
            gateway_id: "0xff4b7a5efd00ff2ec3518d4f250a27e4c29a2211".into(),
            day_start: DAY,
            query_count: queries,
            num_200,
            latency_ms: Some(120.0),
            blocks_behind: Some(3.0),
            total_query_fees: 0.5,
            buckets: 288,
        }
    }

    fn gateway_day(
        deployment: &str,
        gateway: &str,
        day_start: i64,
        queries: f64,
    ) -> DeploymentDaily {
        DeploymentDaily {
            deployment: deployment.into(),
            chain: Some("arbitrum-one".into()),
            gateway_id: gateway.into(),
            day_start,
            query_count: queries,
        }
    }

    #[test]
    fn day_numbers_count_from_mainnet_launch() {
        assert_eq!(day_number(18_613 * 86_400), 0);
        assert_eq!(day_number(18_613 * 86_400 + 86_399), 0);
        assert_eq!(day_number(DAY), 20_708 - GRAPH_EPOCH_DAYS);
    }

    #[test]
    fn the_published_count_is_the_success_count() {
        let row = QosDailyRow::from(&daily("0xa", "QmX", 1_000.0, Some(987.0)));
        assert_eq!(row.success_count, Some(987.0));
        assert_eq!(row.query_count, 1_000.0);
        assert_eq!(row.day_number, 2_095);
    }

    /// A day with no published count must stay absent all the way to the scorer.
    #[test]
    fn an_unpublished_count_stays_absent() {
        assert_eq!(
            QosDailyRow::from(&daily("0xa", "QmX", 1_000.0, None)).success_count,
            None
        );
    }

    #[test]
    fn totals_sum_gateway_queries_across_gateways_and_days() {
        let rows = [
            gateway_day("QmX", "0xgw1", DAY, 700.0),
            gateway_day("QmX", "0xgw2", DAY, 300.0),
            gateway_day("QmX", "0xgw1", DAY + 86_400, 250.0),
            gateway_day("QmY", "0xgw1", DAY, 5.0),
        ];
        let t = deployment_totals(&rows);
        assert_eq!(
            t,
            vec![
                DeploymentTotalRow {
                    deployment_id: "QmX".into(),
                    total_query_count: 1_250.0
                },
                DeploymentTotalRow {
                    deployment_id: "QmY".into(),
                    total_query_count: 5.0
                },
            ]
        );
    }

    /// Two indexers attempted 1,100 queries on a deployment the gateway answered 1,000 on, because
    /// it retried 100 on a second indexer. Served share is measured against the gateway's 1,000.
    #[test]
    fn served_share_is_against_the_gateway_count_not_attempts() {
        let attempts = [
            daily("0xa", "QmX", 800.0, Some(700.0)),
            daily("0xb", "QmX", 300.0, Some(300.0)),
        ];
        let totals = deployment_totals(&[gateway_day("QmX", "0xgw1", DAY, 1_000.0)]);
        let rows: Vec<QosDailyRow> = attempts.iter().map(QosDailyRow::from).collect();
        let m = aggregate_indexer_metrics(&rows, &totals, &AggregateOpts::today(day_number(DAY)));
        let share = |ix: &str| m.iter().find(|(i, _)| i == ix).unwrap().1[0].served_share;
        assert_eq!(share("0xa"), 0.8);
        assert_eq!(share("0xb"), 0.3);
    }
}
