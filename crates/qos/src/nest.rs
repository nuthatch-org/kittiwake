//! The QoS nest's rollup, shaped for the scorer.
//!
//! One `qos_allocation_daily` row is an indexer on a deployment, chain and gateway for one UTC day,
//! summed from Edge & Node's five-minute oracle buckets. The scorer was written against Lodestar's
//! old `qos_daily`, so this is the only place the two shapes meet.

use serde::{Deserialize, Serialize};

use crate::aggregate::{DeploymentTotalRow, QosDailyRow};

/// Days are counted from The Graph's mainnet launch, 2020-12-17, as the oracle numbers them.
pub const GRAPH_EPOCH_DAYS: i64 = 18_613;

pub fn day_number(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(86_400) - GRAPH_EPOCH_DAYS
}

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
    /// Mean latency weighted by 200 responses.
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

/// Served-share denominators from the same rows.
///
/// The old denominator was the gateway's per-deployment query count from the oracle's query-result
/// topic. This sums indexer attempts instead, which runs higher wherever a gateway retried a query
/// on a second indexer, so served share and the served gap read slightly lower than they did.
pub fn deployment_totals(rows: &[AllocationDaily]) -> Vec<DeploymentTotalRow> {
    let mut out: Vec<DeploymentTotalRow> = Vec::new();
    for r in rows {
        match out.iter_mut().find(|t| t.deployment_id == r.deployment) {
            Some(t) => t.total_query_count += r.query_count,
            None => out.push(DeploymentTotalRow {
                deployment_id: r.deployment.clone(),
                total_query_count: r.query_count,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
            day_start: 1_789_171_200,
            query_count: queries,
            num_200,
            latency_ms: Some(120.0),
            blocks_behind: Some(3.0),
            total_query_fees: 0.5,
            buckets: 288,
        }
    }

    #[test]
    fn day_numbers_count_from_mainnet_launch() {
        assert_eq!(day_number(18_613 * 86_400), 0);
        assert_eq!(day_number(18_613 * 86_400 + 86_399), 0);
        assert_eq!(day_number(1_789_171_200), 20_708 - GRAPH_EPOCH_DAYS);
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
    fn totals_sum_attempts_per_deployment() {
        let rows = [
            daily("0xa", "QmX", 700.0, None),
            daily("0xb", "QmX", 300.0, None),
            daily("0xa", "QmY", 5.0, None),
        ];
        let t = deployment_totals(&rows);
        assert_eq!(t.len(), 2);
        assert_eq!(
            t[0],
            DeploymentTotalRow {
                deployment_id: "QmX".into(),
                total_query_count: 1_000.0
            }
        );
        assert_eq!(t[1].total_query_count, 5.0);
    }
}
