//! From daily QoS rows to per-indexer [`DeploymentMetrics`], and the served gap.
//!
//! Outputs keep first-seen order, as the TypeScript's `Map`s did, so a deployment breakdown lists in
//! the same order on both sides.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::score::{
    COHORT_MIN_PEERS, DEFAULTS, DeploymentMetrics, QualityOpts, QualityResult, block_time_sec,
    compute_quality, ewma_weight, median, wilson_lower_bound,
};

/// One indexer on one deployment on one day, as the old `qos_daily` table held it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QosDailyRow {
    pub indexer_address: String,
    pub deployment_id: String,
    pub day_number: i64,
    pub query_count: f64,
    /// `None` when no success figure was published for the day. Not a zero.
    pub success_count: Option<f64>,
    pub avg_latency_ms: Option<f64>,
    pub blocks_behind: Option<f64>,
    pub chain_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentTotalRow {
    pub deployment_id: String,
    pub total_query_count: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateOpts {
    pub today_day_number: i64,
    pub half_life_days: Option<f64>,
    pub latency_tau_mult: Option<f64>,
    /// Volume a peer needs before it counts toward a deployment's cohort figures.
    pub min_credible_n: Option<f64>,
    pub scale: Option<f64>,
}

impl AggregateOpts {
    pub fn today(today_day_number: i64) -> Self {
        Self {
            today_day_number,
            half_life_days: None,
            latency_tau_mult: None,
            min_credible_n: None,
            scale: None,
        }
    }
}

/// Latency tau is this times the cohort median. At 1x every median indexer scores 1/e by
/// construction; 2.5x lifts it to about 0.67 so latency marks down the slow, not the average.
pub const LATENCY_TAU_MULT: f64 = 2.5;

struct PerPair {
    indexer: String,
    deployment: String,
    /// All days: the latency and freshness denominator.
    w_query: f64,
    /// Only days that published a success figure, so the Wilson bound's numerator and denominator
    /// come from the same days.
    w_query_measured: f64,
    w_success: f64,
    w_lat_numer: f64,
    w_blocks_numer: f64,
    /// Unweighted, for served share.
    raw_query: f64,
    chain_id: Option<String>,
}

fn is_blank(chain: &Option<String>) -> bool {
    chain.as_deref().is_none_or(str::is_empty)
}

fn push_grouped<K: Eq + std::hash::Hash + Clone, V>(
    order: &mut Vec<(K, Vec<V>)>,
    index: &mut HashMap<K, usize>,
    key: &K,
    value: V,
) {
    match index.get(key) {
        Some(&i) => order[i].1.push(value),
        None => {
            index.insert(key.clone(), order.len());
            order.push((key.clone(), vec![value]));
        }
    }
}

/// Group daily rows into per-indexer deployment metrics: EWMA day decay, query-weighted latency and
/// freshness, the cohort's latency tau, best reliability and lag floor, and served share.
pub fn aggregate_indexer_metrics(
    qos_rows: &[QosDailyRow],
    deployment_totals: &[DeploymentTotalRow],
    opts: &AggregateOpts,
) -> Vec<(String, Vec<DeploymentMetrics>)> {
    let half_life = opts.half_life_days.unwrap_or(DEFAULTS.half_life_days);
    let tau_mult = opts.latency_tau_mult.unwrap_or(LATENCY_TAU_MULT);

    let mut pairs: Vec<PerPair> = Vec::new();
    let mut pair_index: HashMap<(&str, &str), usize> = HashMap::new();
    for r in qos_rows {
        let w = ewma_weight((opts.today_day_number - r.day_number) as f64, half_life);
        let i = *pair_index
            .entry((&r.indexer_address, &r.deployment_id))
            .or_insert_with(|| {
                pairs.push(PerPair {
                    indexer: r.indexer_address.clone(),
                    deployment: r.deployment_id.clone(),
                    w_query: 0.0,
                    w_query_measured: 0.0,
                    w_success: 0.0,
                    w_lat_numer: 0.0,
                    w_blocks_numer: 0.0,
                    raw_query: 0.0,
                    chain_id: r.chain_id.clone(),
                });
                pairs.len() - 1
            });
        let p = &mut pairs[i];
        let wq = w * r.query_count;
        p.w_query += wq;
        if let Some(s) = r.success_count {
            p.w_query_measured += wq;
            p.w_success += w * s;
        }
        p.w_lat_numer += wq * r.avg_latency_ms.unwrap_or(0.0);
        p.w_blocks_numer += wq * r.blocks_behind.unwrap_or(0.0);
        p.raw_query += r.query_count;
        if is_blank(&p.chain_id) && !is_blank(&r.chain_id) {
            p.chain_id = r.chain_id.clone();
        }
    }

    let mut lat_by_deployment: HashMap<&str, Vec<f64>> = HashMap::new();
    for p in pairs.iter().filter(|p| p.w_query > 0.0) {
        lat_by_deployment
            .entry(&p.deployment)
            .or_default()
            .push(p.w_lat_numer / p.w_query);
    }
    let tau_by_deployment: HashMap<&str, f64> = lat_by_deployment
        .iter()
        .map(|(dep, lats)| {
            let m = median(lats);
            (*dep, if m > 0.0 { m * tau_mult } else { 1.0 })
        })
        .collect();

    // What separates "this indexer is failing" from "this subgraph is failing" is what credible
    // peers achieved on the same deployment.
    let min_credible_n = opts.min_credible_n.unwrap_or(DEFAULTS.min_credible_n);
    let mut cohort_r: HashMap<&str, Vec<f64>> = HashMap::new();
    let mut cohort_lag: HashMap<&str, Vec<f64>> = HashMap::new();
    for p in pairs
        .iter()
        .filter(|p| p.w_query_measured >= min_credible_n)
    {
        cohort_r
            .entry(&p.deployment)
            .or_default()
            .push(wilson_lower_bound(
                p.w_success,
                p.w_query_measured,
                DEFAULTS.z,
            ));
        if let Some(block_sec) = block_time_sec(p.chain_id.as_deref())
            && p.w_query > 0.0
        {
            cohort_lag
                .entry(&p.deployment)
                .or_default()
                .push((p.w_blocks_numer / p.w_query) * block_sec);
        }
    }
    let cohort_best_r: HashMap<&str, f64> = cohort_r
        .iter()
        .filter(|(_, rs)| rs.len() >= COHORT_MIN_PEERS)
        .map(|(dep, rs)| (*dep, rs.iter().copied().fold(f64::NEG_INFINITY, f64::max)))
        .collect();
    let cohort_floor_lag: HashMap<&str, f64> = cohort_lag
        .iter()
        .filter(|(_, lags)| lags.len() >= COHORT_MIN_PEERS)
        .map(|(dep, lags)| (*dep, lags.iter().copied().fold(f64::INFINITY, f64::min)))
        .collect();

    let total_by_deployment: HashMap<&str, f64> = deployment_totals
        .iter()
        .map(|d| (d.deployment_id.as_str(), d.total_query_count))
        .collect();

    let mut out = Vec::new();
    let mut out_index = HashMap::new();
    for p in pairs.iter().filter(|p| p.w_query > 0.0) {
        let avg_lat = p.w_lat_numer / p.w_query;
        let avg_blocks = p.w_blocks_numer / p.w_query;
        let dep_total = total_by_deployment
            .get(p.deployment.as_str())
            .copied()
            .unwrap_or(0.0);
        let measured = p.w_query_measured > 0.0;
        let metric = DeploymentMetrics {
            deployment: p.deployment.clone(),
            // Where nothing was published, total volume is carried so the deployment still shows
            // up as unmeasured.
            n: if measured {
                p.w_query_measured
            } else {
                p.w_query
            },
            successes: measured.then_some(p.w_success),
            avg_latency_ms: avg_lat,
            latency_tau_ms: tau_by_deployment
                .get(p.deployment.as_str())
                .copied()
                .unwrap_or(avg_lat),
            time_behind_sec: block_time_sec(p.chain_id.as_deref()).map(|s| avg_blocks * s),
            served_share: if dep_total > 0.0 {
                p.raw_query / dep_total
            } else {
                0.0
            },
            cohort_best_reliability: cohort_best_r.get(p.deployment.as_str()).copied(),
            cohort_floor_time_behind_sec: cohort_floor_lag.get(p.deployment.as_str()).copied(),
        };
        push_grouped(&mut out, &mut out_index, &p.indexer, metric);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationRow {
    pub indexer_address: String,
    pub deployment_id: String,
    pub allocated_grt: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Phase2Metric {
    /// Mean over allocated deployments of allocation share minus served share. High and positive
    /// means holding allocation while the gateway routes around you.
    pub served_gap: f64,
    /// GRT allocated per successful query served.
    pub efficiency: f64,
    pub successful_queries: f64,
    pub total_allocated_grt: f64,
    pub allocated_deployments: u32,
}

/// Served gap and efficiency, keyed by the lowercased address of every indexer holding an
/// allocation, because a leech by definition holds allocations.
pub fn compute_phase2_metrics(
    allocations: &[AllocationRow],
    qos_rows: &[QosDailyRow],
    deployment_totals: &[DeploymentTotalRow],
) -> Vec<(String, Phase2Metric)> {
    let mut alloc_by_pair: HashMap<(String, &str), f64> = HashMap::new();
    let mut total_alloc_by_dep: HashMap<&str, f64> = HashMap::new();
    let mut deps_by_indexer: Vec<(String, Vec<&str>)> = Vec::new();
    let mut indexer_index: HashMap<String, usize> = HashMap::new();
    let mut seen: HashSet<(String, &str)> = HashSet::new();
    for a in allocations {
        let ix = a.indexer_address.to_lowercase();
        *alloc_by_pair
            .entry((ix.clone(), &a.deployment_id))
            .or_default() += a.allocated_grt;
        *total_alloc_by_dep.entry(&a.deployment_id).or_default() += a.allocated_grt;
        if seen.insert((ix.clone(), &a.deployment_id)) {
            push_grouped(
                &mut deps_by_indexer,
                &mut indexer_index,
                &ix,
                a.deployment_id.as_str(),
            );
        }
    }

    let mut served_by_pair: HashMap<(String, &str), f64> = HashMap::new();
    let mut success_by_indexer: HashMap<String, f64> = HashMap::new();
    for r in qos_rows {
        let ix = r.indexer_address.to_lowercase();
        *served_by_pair
            .entry((ix.clone(), &r.deployment_id))
            .or_default() += r.query_count;
        if let Some(s) = r.success_count {
            *success_by_indexer.entry(ix).or_default() += s;
        }
    }
    let total_served_by_dep: HashMap<&str, f64> = deployment_totals
        .iter()
        .map(|d| (d.deployment_id.as_str(), d.total_query_count))
        .collect();

    deps_by_indexer
        .into_iter()
        .map(|(ix, deps)| {
            let (mut gap_sum, mut total_allocated) = (0.0, 0.0);
            for &dep in &deps {
                let key = (ix.clone(), dep);
                let alloc = alloc_by_pair.get(&key).copied().unwrap_or(0.0);
                total_allocated += alloc;
                let total_alloc = total_alloc_by_dep.get(dep).copied().unwrap_or(0.0);
                let alloc_share = if total_alloc > 0.0 {
                    alloc / total_alloc
                } else {
                    0.0
                };
                let total_served = total_served_by_dep.get(dep).copied().unwrap_or(0.0);
                let served_share = if total_served > 0.0 {
                    served_by_pair.get(&key).copied().unwrap_or(0.0) / total_served
                } else {
                    0.0
                };
                gap_sum += alloc_share - served_share;
            }
            let successful_queries = success_by_indexer.get(&ix).copied().unwrap_or(0.0);
            let metric = Phase2Metric {
                served_gap: if deps.is_empty() {
                    0.0
                } else {
                    gap_sum / deps.len() as f64
                },
                efficiency: total_allocated / (successful_queries + 1.0),
                successful_queries,
                total_allocated_grt: total_allocated,
                allocated_deployments: deps.len() as u32,
            };
            (ix, metric)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexerQuality {
    pub indexer: String,
    #[serde(flatten)]
    pub quality: QualityResult,
}

/// Raw rows to one quality result per indexer. Only `scale` reaches the scorer, as before.
pub fn score_indexers(
    qos_rows: &[QosDailyRow],
    deployment_totals: &[DeploymentTotalRow],
    opts: &AggregateOpts,
) -> Vec<IndexerQuality> {
    let quality_opts = QualityOpts {
        scale: opts.scale,
        ..QualityOpts::default()
    };
    aggregate_indexer_metrics(qos_rows, deployment_totals, opts)
        .into_iter()
        .map(|(indexer, rows)| IndexerQuality {
            indexer,
            quality: compute_quality(&rows, &quality_opts),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64, digits: i32) -> bool {
        (actual - expected).abs() < 10f64.powi(-digits) / 2.0
    }

    fn row(indexer: &str, deployment: &str, day: i64) -> QosDailyRow {
        QosDailyRow {
            indexer_address: indexer.into(),
            deployment_id: deployment.into(),
            day_number: day,
            query_count: 0.0,
            success_count: Some(0.0),
            avg_latency_ms: Some(0.0),
            blocks_behind: Some(0.0),
            chain_id: Some("arbitrum-one".into()),
        }
    }

    fn served(
        indexer: &str,
        deployment: &str,
        day: i64,
        queries: f64,
        successes: Option<f64>,
    ) -> QosDailyRow {
        QosDailyRow {
            query_count: queries,
            success_count: successes,
            ..row(indexer, deployment, day)
        }
    }

    fn only<'a>(m: &'a [(String, Vec<DeploymentMetrics>)], indexer: &str) -> &'a DeploymentMetrics {
        &m.iter()
            .find(|(ix, _)| ix == indexer)
            .expect("indexer present")
            .1[0]
    }

    fn total(deployment: &str, queries: f64) -> DeploymentTotalRow {
        DeploymentTotalRow {
            deployment_id: deployment.into(),
            total_query_count: queries,
        }
    }

    const TODAY: i64 = 100;

    #[test]
    fn applies_day_decay_and_query_weights_latency_and_blocks() {
        let rows = [
            QosDailyRow {
                avg_latency_ms: Some(50.0),
                blocks_behind: Some(10.0),
                ..served("0xa", "X", 100, 1_000.0, Some(1_000.0))
            },
            QosDailyRow {
                avg_latency_ms: Some(150.0),
                blocks_behind: Some(10.0),
                ..served("0xa", "X", 90, 1_000.0, Some(500.0))
            },
        ];
        let m =
            aggregate_indexer_metrics(&rows, &[total("X", 5_000.0)], &AggregateOpts::today(TODAY));
        let a = only(&m, "0xa");
        assert!(close(a.n, 1_500.0, 6));
        assert!(close(a.successes.unwrap(), 1_250.0, 6));
        assert!(close(a.avg_latency_ms, 83.333, 2));
        assert!(close(a.served_share, 0.4, 6));
        assert!(close(a.time_behind_sec.unwrap(), 2.5, 6));
    }

    #[test]
    fn cohort_latency_tau_is_the_median_across_indexers() {
        let rows: Vec<_> = [("0xa", 50.0), ("0xb", 150.0), ("0xc", 250.0)]
            .into_iter()
            .map(|(ix, lat)| QosDailyRow {
                avg_latency_ms: Some(lat),
                ..served(ix, "X", 100, 1_000.0, Some(1_000.0))
            })
            .collect();
        let raw = aggregate_indexer_metrics(
            &rows,
            &[],
            &AggregateOpts {
                latency_tau_mult: Some(1.0),
                ..AggregateOpts::today(TODAY)
            },
        );
        assert!(
            raw.iter()
                .all(|(_, m)| close(m[0].latency_tau_ms, 150.0, 6))
        );
        let lenient = aggregate_indexer_metrics(&rows, &[], &AggregateOpts::today(TODAY));
        assert!(
            lenient
                .iter()
                .all(|(_, m)| close(m[0].latency_tau_ms, 375.0, 6))
        );
    }

    #[test]
    fn a_day_with_no_success_figure_leaves_the_wilson_denominator() {
        let rows = [
            served("0xa", "X", 100, 1_000.0, Some(990.0)),
            served("0xa", "X", 100, 500.0, None),
        ];
        let m = aggregate_indexer_metrics(&rows, &[], &AggregateOpts::today(TODAY));
        let a = only(&m, "0xa");
        assert!(close(a.n, 1_000.0, 6));
        assert!(close(a.successes.unwrap(), 990.0, 6));
    }

    #[test]
    fn a_deployment_with_no_success_figure_is_unmeasured_not_failed() {
        let m = aggregate_indexer_metrics(
            &[served("0xa", "X", 100, 800.0, None)],
            &[],
            &AggregateOpts::today(TODAY),
        );
        let a = only(&m, "0xa");
        assert_eq!(a.successes, None);
        assert!(close(a.n, 800.0, 6));
    }

    #[test]
    fn an_unknown_chain_yields_no_freshness_reading() {
        let r = QosDailyRow {
            blocks_behind: Some(5_000.0),
            chain_id: Some("some-new-rollup".into()),
            ..served("0xa", "X", 100, 1_000.0, Some(1_000.0))
        };
        let m = aggregate_indexer_metrics(&[r], &[], &AggregateOpts::today(TODAY));
        assert_eq!(only(&m, "0xa").time_behind_sec, None);
    }

    #[test]
    fn xdai_converts_as_gnosis() {
        let r = QosDailyRow {
            blocks_behind: Some(4.0),
            chain_id: Some("xdai".into()),
            ..served("0xa", "X", 100, 10.0, Some(10.0))
        };
        let m = aggregate_indexer_metrics(&[r], &[], &AggregateOpts::today(TODAY));
        assert!(close(only(&m, "0xa").time_behind_sec.unwrap(), 20.0, 6));
    }

    #[test]
    fn reads_cohort_best_and_lag_floor_off_credible_peers() {
        let rows: Vec<_> = [
            ("0xa", 50.0),
            ("0xb", 3_000.0),
            ("0xc", 2_500.0),
            ("0xd", 1_000.0),
        ]
        .into_iter()
        .map(|(ix, s)| QosDailyRow {
            blocks_behind: Some(20_000.0),
            ..served(ix, "X", TODAY, 5_000.0, Some(s))
        })
        .collect();
        let m = aggregate_indexer_metrics(&rows, &[], &AggregateOpts::today(TODAY));
        let a = only(&m, "0xa");
        let best = a.cohort_best_reliability.unwrap();
        assert!(best > 0.58 && best < 0.6);
        assert!(close(a.cohort_floor_time_behind_sec.unwrap(), 5_000.0, 6));
    }

    #[test]
    fn withholds_cohort_figures_without_enough_credible_peers() {
        let rows = [
            served("0xa", "X", TODAY, 5_000.0, Some(50.0)),
            served("0xb", "X", TODAY, 5_000.0, Some(4_000.0)),
            served("0xc", "X", TODAY, 20.0, Some(20.0)),
        ];
        let m = aggregate_indexer_metrics(&rows, &[], &AggregateOpts::today(TODAY));
        let a = only(&m, "0xa");
        assert_eq!(a.cohort_best_reliability, None);
        assert_eq!(a.cohort_floor_time_behind_sec, None);
    }

    #[test]
    fn served_share_is_zero_when_the_deployment_total_is_unknown() {
        let m = aggregate_indexer_metrics(
            &[served("0xa", "Z", 100, 500.0, Some(500.0))],
            &[],
            &AggregateOpts::today(TODAY),
        );
        assert_eq!(only(&m, "0xa").served_share, 0.0);
    }

    #[test]
    fn groups_deployments_under_one_indexer() {
        let rows = [
            served("0xa", "X", 100, 100.0, Some(100.0)),
            served("0xa", "Y", 100, 100.0, Some(100.0)),
        ];
        let m = aggregate_indexer_metrics(&rows, &[], &AggregateOpts::today(TODAY));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1.len(), 2);
    }

    fn alloc(indexer: &str, deployment: &str, grt: f64) -> AllocationRow {
        AllocationRow {
            indexer_address: indexer.into(),
            deployment_id: deployment.into(),
            allocated_grt: grt,
        }
    }

    fn phase2<'a>(m: &'a [(String, Phase2Metric)], indexer: &str) -> &'a Phase2Metric {
        &m.iter()
            .find(|(ix, _)| ix == indexer)
            .expect("indexer present")
            .1
    }

    #[test]
    fn allocated_heavily_but_routed_around_is_a_large_positive_gap() {
        let allocations = [
            alloc("0xleech", "X", 900_000.0),
            alloc("0xworker", "X", 100_000.0),
        ];
        let qos = [
            QosDailyRow {
                avg_latency_ms: Some(50.0),
                blocks_behind: Some(5.0),
                ..served("0xworker", "X", 100, 9_900.0, Some(9_900.0))
            },
            QosDailyRow {
                avg_latency_ms: Some(3_000.0),
                blocks_behind: Some(9_000.0),
                ..served("0xleech", "X", 100, 100.0, Some(50.0))
            },
        ];
        let m = compute_phase2_metrics(&allocations, &qos, &[total("X", 10_000.0)]);
        let (leech, worker) = (phase2(&m, "0xleech"), phase2(&m, "0xworker"));
        assert!(leech.served_gap > 0.8);
        assert!(worker.served_gap < 0.0);
        assert!(leech.efficiency > worker.efficiency);
    }

    #[test]
    fn efficiency_is_allocated_grt_per_useful_query() {
        let m = compute_phase2_metrics(
            &[alloc("0xa", "X", 1_000.0)],
            &[QosDailyRow {
                avg_latency_ms: Some(10.0),
                blocks_behind: Some(1.0),
                ..served("0xa", "X", 100, 100.0, Some(99.0))
            }],
            &[total("X", 100.0)],
        );
        assert!(close(phase2(&m, "0xa").efficiency, 10.0, 6));
    }

    #[test]
    fn allocated_but_never_served_has_the_maximum_gap() {
        let m =
            compute_phase2_metrics(&[alloc("0xghost", "X", 500.0)], &[], &[total("X", 5_000.0)]);
        let g = phase2(&m, "0xghost");
        assert!(close(g.served_gap, 1.0, 6));
        assert_eq!(g.successful_queries, 0.0);
    }

    #[test]
    fn one_result_per_indexer_and_the_workhorse_wins() {
        let rows = [
            QosDailyRow {
                avg_latency_ms: Some(40.0),
                blocks_behind: Some(4.0),
                ..served("0xgood", "X", 100, 10_000.0, Some(9_990.0))
            },
            QosDailyRow {
                avg_latency_ms: Some(3_000.0),
                blocks_behind: Some(200_000.0),
                ..served("0xleech", "X", 100, 12.0, Some(12.0))
            },
        ];
        let results = score_indexers(&rows, &[total("X", 10_012.0)], &AggregateOpts::today(100));
        assert_eq!(results.len(), 2);
        let q = |ix: &str| {
            results
                .iter()
                .find(|r| r.indexer == ix)
                .unwrap()
                .quality
                .q_score
        };
        assert!(q("0xgood") > q("0xleech"));
        assert!(q("0xleech") < 20.0);
    }
}
