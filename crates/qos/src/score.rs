//! Per-deployment quality, blended into one indexer score.
//!
//! Per deployment the score is a weighted product, `R^a · U_lat^b · U_fresh^c`, so a near-zero on
//! any axis sinks that deployment. Deployments are blended by queries served. Until 2026-08-15 the
//! blend weight was served share, which let three sole-served queries outvote a hundred thousand;
//! share now only feeds the served gap.

use serde::{Deserialize, Serialize};

/// The tunables. Each is overridable through [`QualityOpts`] so calibration happens in the open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Defaults {
    /// Wilson z, 95%.
    pub z: f64,
    /// Reliability exponent.
    pub a: f64,
    /// Latency exponent.
    pub b: f64,
    /// Freshness exponent.
    pub c: f64,
    /// Thirty minutes behind scores 1/e. It was 600 s, which graded ordinary L2 lag as an outage
    /// because the oracle's blocks-behind is a daily mean.
    pub tau_fresh_sec: f64,
    /// EWMA half-life for weighting days.
    pub half_life_days: f64,
    /// Queries a deployment needs before it counts toward coverage.
    pub min_credible_n: f64,
    pub coverage_k: f64,
    /// Display divisor, and 1 is where it belongs. At 0.65 it compensated for the served-share
    /// blend; with that gone it pinned 19 of 51 indexers at 100 on 2026-08-15.
    pub scale: f64,
}

pub const DEFAULTS: Defaults = Defaults {
    z: 1.96,
    a: 1.0,
    b: 1.0,
    c: 0.5,
    tau_fresh_sec: 1800.0,
    half_life_days: 10.0,
    min_credible_n: 100.0,
    coverage_k: 3.0,
    scale: 1.0,
};

/// Seconds per block for the oracle's chain name, or `None` for a chain we do not know.
///
/// `None` rather than a 12-second guess: the guess turned a fast chain a few thousand blocks behind
/// into hours stale, and an unknown block time is not evidence of lag.
pub fn block_time_sec(chain_id: Option<&str>) -> Option<f64> {
    let chain = chain_id.filter(|c| !c.is_empty())?.to_lowercase();
    Some(match chain.as_str() {
        "mainnet" | "sepolia" | "moonbeam" => 12.0,
        "arbitrum-one" | "arbitrum" | "arbitrum-sepolia" => 0.25,
        "base" | "base-sepolia" | "optimism" | "optimism-sepolia" | "matic" | "polygon"
        | "polygon-zkevm" | "avalanche" | "linea" | "blast-mainnet" | "boba" => 2.0,
        "bsc" | "chapel" | "scroll" | "xlayer-mainnet" | "chiliz" | "chiliz-testnet" => 3.0,
        "gnosis" | "xdai" | "celo" | "fuse" => 5.0,
        "fantom" | "unichain" | "zksync-era" | "monad" => 1.0,
        "sonic" => 0.5,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Success,
    Accent,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Grade {
    pub grade: &'static str,
    pub variant: Variant,
}

pub fn qos_grade(q: f64) -> Grade {
    let (grade, variant) = if q >= 75.0 {
        ("A", Variant::Success)
    } else if q >= 60.0 {
        ("B", Variant::Accent)
    } else if q >= 45.0 {
        ("C", Variant::Warning)
    } else if q >= 30.0 {
        ("D", Variant::Warning)
    } else {
        ("F", Variant::Error)
    };
    Grade { grade, variant }
}

/// Lower bound of the Wilson interval, in `[0, 1]`. No evidence scores zero.
pub fn wilson_lower_bound(successes: f64, n: f64, z: f64) -> f64 {
    if n <= 0.0 {
        return 0.0;
    }
    let p = (successes / n).clamp(0.0, 1.0);
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = p + z2 / (2.0 * n);
    let margin = z * ((p * (1.0 - p)) / n + z2 / (4.0 * n * n)).sqrt();
    ((center - margin) / denom).clamp(0.0, 1.0)
}

/// Exponential decay in `[0, 1]`: zero scores 1, `tau` scores 1/e.
pub fn decay_util(value: f64, tau: f64) -> f64 {
    if tau <= 0.0 {
        return if value <= 0.0 { 1.0 } else { 0.0 };
    }
    if value <= 0.0 {
        return 1.0;
    }
    (-value / tau).exp()
}

pub fn latency_util(latency_ms: f64, tau_ms: f64) -> f64 {
    decay_util(latency_ms.max(0.0), tau_ms)
}

pub fn freshness_util(time_behind_sec: f64, tau_sec: f64) -> f64 {
    decay_util(time_behind_sec.max(0.0), tau_sec)
}

/// Weight of a datapoint `age_days` old. Today weighs 1.
pub fn ewma_weight(age_days: f64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return if age_days <= 0.0 { 1.0 } else { 0.0 };
    }
    0.5f64.powf(age_days.max(0.0) / half_life_days)
}

pub fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let mid = s.len() / 2;
    if s.len() % 2 == 1 {
        s[mid]
    } else {
        (s[mid - 1] + s[mid]) / 2.0
    }
}

/// One deployment as the scorer sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentMetrics {
    pub deployment: String,
    /// EWMA-weighted queries, and the blend weight.
    pub n: f64,
    /// EWMA-weighted successes, or `None` when the oracle published no success figure. Excluded
    /// from the blend, never read as zero.
    pub successes: Option<f64>,
    pub avg_latency_ms: f64,
    /// The cohort's latency normaliser on this deployment.
    pub latency_tau_ms: f64,
    /// `None` on a chain whose block time is unknown.
    pub time_behind_sec: Option<f64>,
    pub served_share: f64,
    /// The best credible peer's Wilson bound here, when the cohort is big enough to say.
    #[serde(default)]
    pub cohort_best_reliability: Option<f64>,
    /// The freshest credible peer's lag here. A halted subgraph puts everyone at the same lag.
    #[serde(default)]
    pub cohort_floor_time_behind_sec: Option<f64>,
}

/// Below this cohort best, a deployment is degraded for everyone and graded against the cohort.
pub const COHORT_DEGRADED_BELOW: f64 = 0.9;

/// Credible peers needed before a cohort figure is trusted.
pub const COHORT_MIN_PEERS: usize = 3;

/// Reliability graded against what the cohort proves achievable, only where the whole cohort is
/// degraded. The worst of a bad bunch still scores badly.
pub fn cohort_adjusted_reliability(r: f64, cohort_best: Option<f64>) -> f64 {
    match cohort_best {
        Some(best) if best < COHORT_DEGRADED_BELOW && best > 0.0 => (r / best).min(1.0),
        _ => r,
    }
}

/// Seconds behind, net of the lag the whole cohort shares.
pub fn cohort_adjusted_time_behind(
    time_behind_sec: Option<f64>,
    cohort_floor_sec: Option<f64>,
) -> Option<f64> {
    let t = time_behind_sec?;
    Some(match cohort_floor_sec {
        Some(floor) => (t - floor).max(0.0),
        None => t,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityOpts {
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub c: Option<f64>,
    pub tau_fresh_sec: Option<f64>,
    pub min_credible_n: Option<f64>,
    pub coverage_k: Option<f64>,
    pub z: Option<f64>,
    pub scale: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityResult {
    /// 0 to 100.
    pub q_score: f64,
    pub reliability: f64,
    pub lat_util: f64,
    /// `None` when no deployment had a usable freshness reading.
    pub fresh_util: Option<f64>,
    pub coverage: f64,
    pub credible_deployments: u32,
    /// Deployments with volume but no published success figure.
    pub unmeasured_deployments: u32,
}

/// One deployment's arithmetic, so a grade can be explained rather than asserted.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentContribution {
    pub deployment: String,
    pub n: f64,
    /// Share of the composite this deployment accounts for.
    pub weight: f64,
    /// Raw Wilson bound, before any cohort adjustment.
    pub reliability: Option<f64>,
    /// What the score used; differs only on a cohort-degraded deployment.
    pub reliability_used: Option<f64>,
    pub cohort_best_reliability: Option<f64>,
    pub lat_util: f64,
    pub fresh_util: Option<f64>,
    pub time_behind_sec: Option<f64>,
    /// The part of the lag that is this indexer's own.
    pub time_behind_own_sec: Option<f64>,
    pub served_share: f64,
    pub q: Option<f64>,
    pub measured: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QualityDetail {
    pub result: QualityResult,
    pub deployments: Vec<DeploymentContribution>,
}

pub fn compute_quality(rows: &[DeploymentMetrics], opts: &QualityOpts) -> QualityResult {
    compute_quality_detail(rows, opts).result
}

/// The score plus its per-deployment working.
///
/// A deployment with no success figure or no volume is dropped from the blend, and an unknown
/// chain omits the freshness factor, so a missing measurement costs a component rather than
/// convicting the indexer on it.
pub fn compute_quality_detail(rows: &[DeploymentMetrics], opts: &QualityOpts) -> QualityDetail {
    let a = opts.a.unwrap_or(DEFAULTS.a);
    let b = opts.b.unwrap_or(DEFAULTS.b);
    let c = opts.c.unwrap_or(DEFAULTS.c);
    let tau_fresh_sec = opts.tau_fresh_sec.unwrap_or(DEFAULTS.tau_fresh_sec);
    let min_credible_n = opts.min_credible_n.unwrap_or(DEFAULTS.min_credible_n);
    let coverage_k = opts.coverage_k.unwrap_or(DEFAULTS.coverage_k);
    let z = opts.z.unwrap_or(DEFAULTS.z);
    let scale = opts.scale.unwrap_or(DEFAULTS.scale);

    let unmeasured_deployments = rows
        .iter()
        .filter(|r| r.successes.is_none() || r.n <= 0.0)
        .count() as u32;
    let empty = QualityResult {
        q_score: 0.0,
        reliability: 0.0,
        lat_util: 0.0,
        fresh_util: None,
        coverage: 0.0,
        credible_deployments: 0,
        unmeasured_deployments,
    };
    if rows.is_empty() {
        return QualityDetail {
            result: empty,
            deployments: Vec::new(),
        };
    }

    let (mut w_sum, mut q_blend, mut r_blend, mut lat_blend) = (0.0, 0.0, 0.0, 0.0);
    let (mut fresh_blend, mut fresh_w_sum) = (0.0, 0.0);
    let mut credible_deployments = 0u32;
    let mut deployments = Vec::with_capacity(rows.len());

    for row in rows {
        let u_lat = latency_util(row.avg_latency_ms, row.latency_tau_ms);
        let own_time_behind =
            cohort_adjusted_time_behind(row.time_behind_sec, row.cohort_floor_time_behind_sec);
        let u_fresh = own_time_behind.map(|t| freshness_util(t, tau_fresh_sec));

        let successes = match row.successes {
            Some(s) if row.n > 0.0 => s,
            _ => {
                deployments.push(DeploymentContribution {
                    deployment: row.deployment.clone(),
                    n: row.n,
                    weight: 0.0,
                    reliability: None,
                    reliability_used: None,
                    cohort_best_reliability: row.cohort_best_reliability,
                    lat_util: u_lat,
                    fresh_util: u_fresh,
                    time_behind_sec: row.time_behind_sec,
                    time_behind_own_sec: own_time_behind,
                    served_share: row.served_share,
                    q: None,
                    measured: false,
                });
                continue;
            }
        };

        let r = wilson_lower_bound(successes, row.n, z);
        let r_used = cohort_adjusted_reliability(r, row.cohort_best_reliability);
        let q = r_used.powf(a) * u_lat.powf(b) * u_fresh.map_or(1.0, |f| f.powf(c));

        let w = row.n;
        w_sum += w;
        q_blend += w * q;
        r_blend += w * r_used;
        lat_blend += w * u_lat;
        if let Some(f) = u_fresh {
            fresh_blend += w * f;
            fresh_w_sum += w;
        }
        if row.n >= min_credible_n {
            credible_deployments += 1;
        }

        deployments.push(DeploymentContribution {
            deployment: row.deployment.clone(),
            n: row.n,
            weight: 0.0,
            reliability: Some(r),
            reliability_used: Some(r_used),
            cohort_best_reliability: row.cohort_best_reliability,
            lat_util: u_lat,
            fresh_util: u_fresh,
            time_behind_sec: row.time_behind_sec,
            time_behind_own_sec: own_time_behind,
            served_share: row.served_share,
            q: Some(q),
            measured: true,
        });
    }

    if w_sum <= 0.0 {
        return QualityDetail {
            result: empty,
            deployments,
        };
    }
    for d in &mut deployments {
        d.weight = if d.measured { d.n / w_sum } else { 0.0 };
    }

    let aggregate = q_blend / w_sum;
    // In [0.5, 1]: breadth is rewarded without crushing a focused, honest indexer.
    let credible = f64::from(credible_deployments);
    let coverage = 0.5 + 0.5 * (credible / (credible + coverage_k));

    QualityDetail {
        result: QualityResult {
            q_score: ((100.0 * aggregate * coverage) / scale).min(100.0),
            reliability: r_blend / w_sum,
            lat_util: lat_blend / w_sum,
            fresh_util: (fresh_w_sum > 0.0).then(|| fresh_blend / fresh_w_sum),
            coverage,
            credible_deployments,
            unmeasured_deployments,
        },
        deployments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vitest's `toBeCloseTo(expected, digits)`, so the ported assertions keep their tolerance.
    fn close(actual: f64, expected: f64, digits: i32) -> bool {
        (actual - expected).abs() < 10f64.powi(-digits) / 2.0
    }

    fn dep(
        deployment: &str,
        n: f64,
        successes: Option<f64>,
        lat: f64,
        tau: f64,
        behind: Option<f64>,
        share: f64,
    ) -> DeploymentMetrics {
        DeploymentMetrics {
            deployment: deployment.into(),
            n,
            successes,
            avg_latency_ms: lat,
            latency_tau_ms: tau,
            time_behind_sec: behind,
            served_share: share,
            cohort_best_reliability: None,
            cohort_floor_time_behind_sec: None,
        }
    }

    fn perfect(deployment: &str) -> DeploymentMetrics {
        dep(
            deployment,
            10_000.0,
            Some(9_990.0),
            40.0,
            100.0,
            Some(10.0),
            0.3,
        )
    }

    fn quality(rows: &[DeploymentMetrics]) -> QualityResult {
        compute_quality(rows, &QualityOpts::default())
    }

    #[test]
    fn wilson_scores_no_evidence_as_zero() {
        assert_eq!(wilson_lower_bound(0.0, 0.0, DEFAULTS.z), 0.0);
        assert_eq!(wilson_lower_bound(0.0, 100.0, DEFAULTS.z), 0.0);
    }

    #[test]
    fn wilson_matches_the_published_value() {
        assert!(close(wilson_lower_bound(80.0, 100.0, DEFAULTS.z), 0.711, 2));
    }

    /// The property the whole score rests on: a few perfect queries cannot beat a workhorse.
    #[test]
    fn a_few_perfect_queries_cannot_beat_a_workhorse() {
        let leech = wilson_lower_bound(10.0, 10.0, DEFAULTS.z);
        let workhorse = wilson_lower_bound(9_990.0, 10_000.0, DEFAULTS.z);
        assert!(workhorse > leech);
    }

    #[test]
    fn wilson_shrinks_small_samples() {
        let small = wilson_lower_bound(10.0, 10.0, DEFAULTS.z);
        let large = wilson_lower_bound(10_000.0, 10_000.0, DEFAULTS.z);
        assert!(large > small);
        assert!(small < 0.8);
        assert!(large > 0.999);
    }

    #[test]
    fn wilson_is_bounded_and_monotonic_in_n() {
        let a = wilson_lower_bound(80.0, 100.0, DEFAULTS.z);
        let b = wilson_lower_bound(800.0, 1_000.0, DEFAULTS.z);
        let c = wilson_lower_bound(8_000.0, 10_000.0, DEFAULTS.z);
        assert!(a < b && b < c && c <= 1.0);
        assert!(wilson_lower_bound(0.0, 50.0, DEFAULTS.z) >= 0.0);
    }

    #[test]
    fn decay_is_one_at_zero_and_one_over_e_at_tau() {
        assert_eq!(decay_util(0.0, 100.0), 1.0);
        assert!(close(
            decay_util(100.0, 100.0),
            1.0 / std::f64::consts::E,
            6
        ));
        assert!(decay_util(200.0, 100.0) < decay_util(100.0, 100.0));
        assert_eq!(decay_util(0.0, 0.0), 1.0);
        assert_eq!(decay_util(5.0, 0.0), 0.0);
    }

    #[test]
    fn latency_and_freshness_utilities() {
        assert_eq!(latency_util(0.0, 200.0), 1.0);
        assert!(latency_util(2_000.0, 200.0) < 0.0001);
        assert_eq!(freshness_util(0.0, DEFAULTS.tau_fresh_sec), 1.0);
        assert!(close(
            freshness_util(600.0, 600.0),
            1.0 / std::f64::consts::E,
            6
        ));
        assert!(freshness_util(60_000.0, 600.0) < 1e-40);
    }

    #[test]
    fn ewma_halves_per_half_life() {
        assert_eq!(ewma_weight(0.0, 10.0), 1.0);
        assert!(close(ewma_weight(10.0, 10.0), 0.5, 9));
        assert!(close(ewma_weight(20.0, 10.0), 0.25, 9));
        assert_eq!(ewma_weight(0.0, 0.0), 1.0);
        assert_eq!(ewma_weight(3.0, 0.0), 0.0);
    }

    #[test]
    fn median_odd_even_empty() {
        assert_eq!(median(&[5.0, 1.0, 3.0]), 3.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn block_times_for_known_chains_and_aliases() {
        assert_eq!(block_time_sec(Some("arbitrum-one")), Some(0.25));
        assert_eq!(block_time_sec(Some("mainnet")), Some(12.0));
        assert_eq!(block_time_sec(Some("BSC")), Some(3.0));
        assert_eq!(block_time_sec(Some("xdai")), Some(5.0));
        assert_eq!(block_time_sec(Some("sonic")), Some(0.5));
    }

    #[test]
    fn an_unknown_chain_has_no_block_time() {
        assert_eq!(block_time_sec(Some("some-new-rollup")), None);
        assert_eq!(block_time_sec(None), None);
        assert_eq!(block_time_sec(Some("")), None);
    }

    #[test]
    fn grade_thresholds() {
        assert_eq!(qos_grade(75.0).grade, "A");
        assert_eq!(qos_grade(74.9).grade, "B");
        assert_eq!(qos_grade(60.0).grade, "B");
        assert_eq!(qos_grade(45.0).grade, "C");
        assert_eq!(
            qos_grade(30.0),
            Grade {
                grade: "D",
                variant: Variant::Warning
            }
        );
        assert_eq!(
            qos_grade(29.9),
            Grade {
                grade: "F",
                variant: Variant::Error
            }
        );
    }

    #[test]
    fn empty_input_scores_zero() {
        let r = quality(&[]);
        assert_eq!(r.q_score, 0.0);
        assert_eq!(r.credible_deployments, 0);
    }

    #[test]
    fn a_near_zero_on_any_axis_sinks_the_deployment() {
        let r = quality(&[dep(
            "d",
            10_000.0,
            Some(10_000.0),
            10_000.0,
            100.0,
            Some(0.0),
            1.0,
        )]);
        assert!(r.q_score < 1.0);
        assert!(r.lat_util < 0.0001);
        assert!(r.reliability > 0.99);
    }

    #[test]
    fn a_leech_scores_far_below_a_broad_workhorse() {
        let leech = quality(&[dep(
            "L",
            12.0,
            Some(12.0),
            1_500.0,
            100.0,
            Some(50_000.0),
            1.0,
        )]);
        let rows: Vec<_> = (0..9).map(|i| perfect(&format!("d{i}"))).collect();
        let workhorse = quality(&rows);
        assert!(workhorse.q_score > 50.0);
        assert!(leech.q_score < 10.0);
        assert!(workhorse.q_score > leech.q_score * 5.0);
    }

    #[test]
    fn coverage_rewards_breadth() {
        let one = quality(&[perfect("a")]);
        let five: Vec<_> = ["a", "b", "c", "d", "e"].into_iter().map(perfect).collect();
        let five = quality(&five);
        assert!(five.coverage > one.coverage);
        assert!(one.coverage >= 0.5 && five.coverage <= 1.0);
        assert!(five.q_score > one.q_score);
    }

    #[test]
    fn volume_below_min_credible_n_does_not_count_toward_coverage() {
        let r = quality(&[dep("d", 50.0, Some(50.0), 10.0, 100.0, Some(0.0), 1.0)]);
        assert_eq!(r.credible_deployments, 0);
        assert!(close(r.coverage, 0.5, 9));
    }

    #[test]
    fn sub_metrics_are_volume_weighted() {
        let r = quality(&[
            dep("a", 3_000.0, Some(3_000.0), 0.0, 100.0, Some(0.0), 0.1),
            dep("b", 1_000.0, Some(0.0), 0.0, 100.0, Some(0.0), 1.0),
        ]);
        assert!(r.reliability > 0.7 && r.reliability < 0.8);
        assert!(close(r.lat_util, 1.0, 6));
    }

    /// Blending by served share let twenty sole-served trickles outvote the firehose twelve to one.
    #[test]
    fn a_sole_served_trickle_cannot_outweigh_the_firehose() {
        let firehose = dep(
            "busy",
            100_000.0,
            Some(100_000.0),
            50.0,
            250.0,
            Some(5.0),
            0.08,
        );
        let mut rows = vec![firehose.clone()];
        rows.extend((0..20).map(|i| {
            dep(
                &format!("t{i}"),
                3.0,
                Some(3.0),
                50.0,
                250.0,
                Some(5.0),
                1.0,
            )
        }));
        let alone = quality(&[firehose]);
        let with_trickles = quality(&rows);
        assert!(with_trickles.reliability > 0.98);
        assert!(alone.reliability - with_trickles.reliability < 0.02);
    }

    #[test]
    fn an_unpublished_success_figure_is_excluded_not_scored_zero() {
        let r = quality(&[
            dep("known", 1_000.0, Some(1_000.0), 50.0, 250.0, Some(0.0), 0.5),
            dep("silent", 5_000.0, None, 50.0, 250.0, Some(0.0), 0.5),
        ]);
        assert!(r.reliability > 0.98);
        assert_eq!(r.unmeasured_deployments, 1);
        assert_eq!(r.credible_deployments, 1);
    }

    #[test]
    fn all_unmeasured_scores_nothing() {
        let r = quality(&[dep("a", 500.0, None, 10.0, 100.0, Some(0.0), 1.0)]);
        assert_eq!(r.q_score, 0.0);
        assert_eq!(r.unmeasured_deployments, 1);
    }

    #[test]
    fn an_unknown_chain_omits_freshness() {
        let known = dep("a", 1_000.0, Some(1_000.0), 0.0, 100.0, Some(0.0), 1.0);
        let unknown = DeploymentMetrics {
            deployment: "b".into(),
            time_behind_sec: None,
            ..known.clone()
        };
        let r = quality(&[unknown]);
        assert!(close(r.q_score, quality(&[known]).q_score, 6));
        assert_eq!(r.fresh_util, None);
    }

    fn on_broken_deployment(success_rate: f64) -> DeploymentMetrics {
        DeploymentMetrics {
            cohort_best_reliability: Some(wilson_lower_bound(
                5_000.0 * 0.7555,
                5_000.0,
                DEFAULTS.z,
            )),
            ..dep(
                "notional-exponent-shaped",
                5_000.0,
                Some(5_000.0 * success_rate),
                250.0,
                625.0,
                Some(1_128.0),
                0.08,
            )
        }
    }

    #[test]
    fn doing_as_well_as_anyone_on_a_broken_subgraph_is_not_marked_down() {
        assert!(close(
            quality(&[on_broken_deployment(0.7555)]).reliability,
            1.0,
            2
        ));
    }

    #[test]
    fn the_worst_of_a_bad_bunch_still_scores_badly() {
        assert!(quality(&[on_broken_deployment(0.0095)]).reliability < 0.05);
    }

    #[test]
    fn no_curve_where_the_cohort_is_healthy() {
        let r = quality(&[DeploymentMetrics {
            cohort_best_reliability: Some(0.99),
            ..dep("d", 5_000.0, Some(2_500.0), 0.0, 100.0, Some(0.0), 0.5)
        }]);
        assert!(r.reliability < 0.52);
    }

    #[test]
    fn a_halted_subgraphs_shared_lag_is_subtracted() {
        let stuck = DeploymentMetrics {
            cohort_floor_time_behind_sec: Some(21_600.0),
            ..dep(
                "halted",
                5_000.0,
                Some(5_000.0),
                0.0,
                100.0,
                Some(21_600.0),
                0.2,
            )
        };
        assert!(close(
            quality(std::slice::from_ref(&stuck)).fresh_util.unwrap(),
            1.0,
            6
        ));
        let worse = DeploymentMetrics {
            time_behind_sec: Some(21_600.0 + 7_200.0),
            ..stuck
        };
        assert!(quality(&[worse]).fresh_util.unwrap() < 0.02);
    }

    #[test]
    fn freshness_bites_on_a_known_chain() {
        let r = quality(&[dep(
            "a",
            1_000.0,
            Some(1_000.0),
            0.0,
            100.0,
            Some(36_000.0),
            1.0,
        )]);
        assert!(r.fresh_util.unwrap() < 0.001);
        assert!(r.q_score < 5.0);
    }
}
