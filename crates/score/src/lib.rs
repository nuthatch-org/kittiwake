//! The composite indexer score: eleven dimensions, transparent weights, no black boxes.
//!
//! Ported from `src/lib/risk-score.ts` with the anchors unchanged, because the weights and
//! thresholds are published on the dashboard and a silent drift between the two would make the
//! published table a lie.
//!
//! Two design decisions worth keeping in view, both of which look like bugs until you know why:
//!
//! - **Self-stake is scored on the absolute amount, not as a ratio of total stake.** Attracting
//!   delegation is a sign of trust; scoring the ratio would punish an indexer for being popular.
//! - **An operationally excellent indexer that takes everything still scores badly.** The delegator
//!   cut is weighted at ten and a hundred-percent reward cut caps cut stability at five, because
//!   the score is for delegators, and a delegator earns nothing from such an indexer.

use serde::{Deserialize, Serialize};

/// Oracle-sourced eligibility under GIP-0079.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reo {
    Eligible,
    Ineligible,
    /// The oracle could not be read. Neither reward nor punish: a score is a claim about the
    /// indexer, and "we could not check" is a claim about us.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grade {
    A,
    B,
    C,
    D,
    F,
}

impl Grade {
    pub fn as_str(self) -> &'static str {
        match self {
            Grade::A => "A",
            Grade::B => "B",
            Grade::C => "C",
            Grade::D => "D",
            Grade::F => "F",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    pub reo: i32,
    pub allocation_efficiency: i32,
    pub self_stake: i32,
    pub delegator_cut: i32,
    pub over_delegation: i32,
    pub transparency: i32,
    pub delegator_apy: i32,
    pub query_volume: i32,
    pub cut_stability: i32,
    pub delegation_trend: i32,
    pub data_service_diversity: i32,
}

/// The published weights. They must sum to 100; a test below enforces it, because a weight added
/// without adjusting the others would quietly rescale every score on the site.
pub const WEIGHTS: [(&str, i32); 11] = [
    ("reo", 20),
    ("allocationEfficiency", 13),
    ("selfStake", 12),
    ("delegatorCut", 10),
    ("overDelegation", 9),
    ("transparency", 8),
    ("delegatorAPY", 8),
    ("queryVolume", 6),
    ("cutStability", 6),
    ("dataServiceDiversity", 5),
    ("delegationTrend", 3),
];

#[derive(Debug, Clone, Serialize)]
pub struct Score {
    pub composite: i32,
    pub breakdown: Breakdown,
    pub grade: Grade,
}

/// Everything the score is computed from. All of it comes from the nest or the chain; none of it
/// is opinion.
#[derive(Debug, Clone)]
pub struct Input {
    pub reo: Reo,
    pub self_stake_grt: f64,
    pub last_delegation_parameter_update: i64,
    pub delegator_parameter_cooldown: i64,
    pub allocation_count: i64,
    pub allocated_grt: f64,
    pub provisioned_grt: Option<f64>,
    /// Percent of delegation capacity used. At or above 100 there is no room for a new delegator
    /// without diluting the existing ones.
    pub delegation_utilization: f64,
    pub has_ens: bool,
    pub has_url: bool,
    pub has_display_name: bool,
    pub reward_cut_ppm: f64,
    pub query_fee_cut_ppm: f64,
    /// What delegators actually experience, accounting for the indexer's own stake ratio. Preferred
    /// over the raw cut when it can be computed.
    pub effective_cut_percent: Option<f64>,
    pub query_fees_collected_grt: f64,
    /// Allocation share minus served share, when known. A better signal than fees: it says whether
    /// the indexer serves what it allocates to.
    pub served_gap: Option<f64>,
    pub net_flow_grt_7d: f64,
    pub delegated_grt: f64,
    pub rolling_apy_30d: Option<f64>,
    pub delegator_apr: f64,
    pub distinct_data_services: i64,
    /// Seconds since the epoch. Passed in rather than read, so the score is a pure function and a
    /// test can pin the clock.
    pub now: i64,
}

/// Linear interpolation between anchors given highest-first, returning the top score at or above
/// the first anchor and the bottom score below the last.
fn interpolate(value: f64, anchors: &[(f64, f64)], below_last: f64) -> i32 {
    if value >= anchors[0].0 {
        return anchors[0].1.round() as i32;
    }
    for w in anchors.windows(2) {
        let (hi, hi_score) = w[0];
        let (lo, lo_score) = w[1];
        if value >= lo {
            let t = if (hi - lo).abs() < f64::EPSILON {
                0.0
            } else {
                (value - lo) / (hi - lo)
            };
            return (lo_score + t * (hi_score - lo_score)).round() as i32;
        }
    }
    below_last.round() as i32
}

fn score_reo(reo: Reo) -> i32 {
    match reo {
        Reo::Eligible => 100,
        Reo::Ineligible => 0,
        Reo::Unknown => 50,
    }
}

/// Absolute GRT, deliberately. Anchored at the protocol minimum of 100k.
fn score_self_stake(grt: f64) -> i32 {
    if grt <= 0.0 {
        return 0;
    }
    interpolate(
        grt,
        &[
            (10_000_000.0, 100.0),
            (5_000_000.0, 95.0),
            (1_000_000.0, 80.0),
            (500_000.0, 65.0),
            (200_000.0, 50.0),
            (100_000.0, 35.0),
            (0.0, 5.0),
        ],
        5.0,
    )
}

fn score_cut_stability(last_update: i64, cooldown: i64, reward_cut_ppm: f64, now: i64) -> i32 {
    let inner = || -> i32 {
        let days = (now - last_update) as f64 / 86_400.0;
        let mut s = if days >= 180.0 {
            100
        } else if days >= 90.0 {
            85
        } else if days >= 30.0 {
            65
        } else if days >= 7.0 {
            45
        } else {
            30
        };
        // A cooldown is a promise not to change the parameters without notice, which is worth
        // something on its own.
        if cooldown > 0 {
            s = (s + 10).min(100);
        }
        s
    };

    // A hundred-percent reward cut is perfectly stable and perfectly useless to a delegator. Hard
    // cap it, or "never changes" becomes a way to score well while paying nothing.
    if reward_cut_ppm >= 1_000_000.0 {
        return 5;
    }
    if reward_cut_ppm >= 900_000.0 {
        return inner().min(30);
    }
    inner()
}

fn score_allocation_efficiency(
    count: i64,
    allocated_grt: f64,
    provisioned_grt: Option<f64>,
) -> i32 {
    if count == 0 {
        return 0;
    }
    let Some(provisioned) = provisioned_grt.filter(|p| *p > 0.0) else {
        // Allocating, but we cannot see what against. Partial credit rather than a zero, because
        // the missing figure is ours, not theirs.
        return 40;
    };
    let ratio = (allocated_grt / provisioned).min(1.0);
    if ratio >= 0.8 {
        100
    } else if ratio >= 0.6 {
        80
    } else if ratio >= 0.4 {
        60
    } else if ratio >= 0.2 {
        40
    } else {
        20
    }
}

fn score_over_delegation(utilization_percent: f64) -> i32 {
    if utilization_percent >= 100.0 {
        0
    } else if utilization_percent >= 95.0 {
        15
    } else if utilization_percent >= 85.0 {
        35
    } else if utilization_percent >= 70.0 {
        55
    } else if utilization_percent >= 50.0 {
        75
    } else {
        100
    }
}

fn score_transparency(ens: bool, url: bool, display_name: bool) -> i32 {
    (if ens { 40 } else { 0 }) + (if url { 30 } else { 0 }) + (if display_name { 30 } else { 0 })
}

fn score_query_volume(fees_grt: f64) -> i32 {
    if fees_grt <= 0.0 {
        return 0;
    }
    interpolate(
        fees_grt,
        &[
            (100_000.0, 100.0),
            (50_000.0, 90.0),
            (10_000.0, 70.0),
            (1_000.0, 50.0),
            (100.0, 30.0),
            (0.0, 15.0),
        ],
        0.0,
    )
}

/// Allocation share minus served share, with a five-point tolerance before it counts against.
fn score_served_gap(gap: f64) -> i32 {
    const TOLERANCE: f64 = 0.05;
    let over = ((gap - TOLERANCE) / (1.0 - TOLERANCE)).clamp(0.0, 1.0);
    (100.0 * (1.0 - over)).round() as i32
}

fn score_delegator_cut(reward_ppm: f64, query_fee_ppm: f64, effective_percent: Option<f64>) -> i32 {
    let cut = match effective_percent {
        Some(e) => e.clamp(0.0, 100.0),
        None => (reward_ppm / 10_000.0).min(100.0),
    };
    // Anchors ascend by cut here, so they are walked lowest-first rather than through `interpolate`.
    let anchors: [(f64, f64); 9] = [
        (0.0, 100.0),
        (5.0, 95.0),
        (10.0, 85.0),
        (15.0, 75.0),
        (20.0, 68.0),
        (25.0, 60.0),
        (50.0, 35.0),
        (75.0, 15.0),
        (100.0, 0.0),
    ];
    let reward_score = if cut <= anchors[0].0 {
        anchors[0].1
    } else if cut >= anchors[anchors.len() - 1].0 {
        anchors[anchors.len() - 1].1
    } else {
        let mut found = anchors[0].1;
        for w in anchors.windows(2) {
            let (lo, lo_s) = w[0];
            let (hi, hi_s) = w[1];
            if cut >= lo && cut <= hi {
                let t = (cut - lo) / (hi - lo);
                found = (lo_s + t * (hi_s - lo_s)).round();
                break;
            }
        }
        found
    };

    // Taking every query fee costs up to fifteen points on top.
    let query_percent = (query_fee_ppm / 10_000.0).min(100.0);
    let penalty = ((query_percent / 100.0) * 15.0).round();
    ((reward_score - penalty).max(0.0)).round() as i32
}

fn score_delegator_apy(rolling_30d: Option<f64>, apr: f64) -> i32 {
    let apy = match rolling_30d {
        Some(r) if r > 0.0 => r,
        _ => apr,
    };
    if apy <= 0.0 {
        return 0;
    }
    interpolate(
        apy,
        &[
            (20.0, 100.0),
            (15.0, 90.0),
            (10.0, 75.0),
            (7.0, 60.0),
            (5.0, 50.0),
            (3.0, 35.0),
            (1.0, 20.0),
            (0.0, 0.0),
        ],
        0.0,
    )
}

fn score_data_services(n: i64) -> i32 {
    match n {
        0 => 0,
        1 => 40,
        2 => 75,
        _ => 100,
    }
}

fn score_delegation_trend(net_flow_grt: f64, delegated_grt: f64) -> i32 {
    // No delegation history is not a warning; it is an absence. Neutral.
    if delegated_grt == 0.0 {
        return 50;
    }
    let pct = (net_flow_grt / delegated_grt) * 100.0;
    if pct >= 2.0 {
        100
    } else if pct >= 0.5 {
        80
    } else if pct >= 0.0 {
        60
    } else if pct >= -1.0 {
        40
    } else if pct >= -3.0 {
        20
    } else {
        0
    }
}

fn grade_from(score: i32) -> Grade {
    if score >= 80 {
        Grade::A
    } else if score >= 65 {
        Grade::B
    } else if score >= 50 {
        Grade::C
    } else if score >= 35 {
        Grade::D
    } else {
        Grade::F
    }
}

pub fn calculate(input: &Input) -> Score {
    let breakdown = Breakdown {
        reo: score_reo(input.reo),
        self_stake: score_self_stake(input.self_stake_grt),
        // The served gap is the better signal when we have it: fees say an indexer was paid, the
        // gap says whether it serves what it allocated to.
        query_volume: match input.served_gap {
            Some(gap) => score_served_gap(gap),
            None => score_query_volume(input.query_fees_collected_grt),
        },
        delegator_cut: score_delegator_cut(
            input.reward_cut_ppm,
            input.query_fee_cut_ppm,
            input.effective_cut_percent,
        ),
        cut_stability: score_cut_stability(
            input.last_delegation_parameter_update,
            input.delegator_parameter_cooldown,
            input.reward_cut_ppm,
            input.now,
        ),
        allocation_efficiency: score_allocation_efficiency(
            input.allocation_count,
            input.allocated_grt,
            input.provisioned_grt,
        ),
        over_delegation: score_over_delegation(input.delegation_utilization),
        transparency: score_transparency(input.has_ens, input.has_url, input.has_display_name),
        delegation_trend: score_delegation_trend(input.net_flow_grt_7d, input.delegated_grt),
        delegator_apy: score_delegator_apy(input.rolling_apy_30d, input.delegator_apr),
        data_service_diversity: score_data_services(input.distinct_data_services),
    };

    let composite = weighted(&breakdown);
    Score {
        composite,
        breakdown,
        grade: grade_from(composite),
    }
}

fn weighted(b: &Breakdown) -> i32 {
    let by_key = |k: &str| -> i32 {
        match k {
            "reo" => b.reo,
            "allocationEfficiency" => b.allocation_efficiency,
            "selfStake" => b.self_stake,
            "delegatorCut" => b.delegator_cut,
            "overDelegation" => b.over_delegation,
            "transparency" => b.transparency,
            "delegatorAPY" => b.delegator_apy,
            "queryVolume" => b.query_volume,
            "cutStability" => b.cut_stability,
            "dataServiceDiversity" => b.data_service_diversity,
            "delegationTrend" => b.delegation_trend,
            _ => 0,
        }
    };
    let sum: f64 = WEIGHTS
        .iter()
        .map(|(k, w)| by_key(k) as f64 * (*w as f64 / 100.0))
        .sum();
    sum.round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_788_000_000;

    fn base() -> Input {
        Input {
            reo: Reo::Unknown,
            self_stake_grt: 100_000.0,
            last_delegation_parameter_update: NOW - 200 * 86_400,
            delegator_parameter_cooldown: 0,
            allocation_count: 10,
            allocated_grt: 800_000.0,
            provisioned_grt: Some(1_000_000.0),
            delegation_utilization: 40.0,
            has_ens: true,
            has_url: true,
            has_display_name: true,
            reward_cut_ppm: 100_000.0,
            query_fee_cut_ppm: 0.0,
            effective_cut_percent: None,
            query_fees_collected_grt: 10_000.0,
            served_gap: None,
            net_flow_grt_7d: 0.0,
            delegated_grt: 1_000_000.0,
            rolling_apy_30d: None,
            delegator_apr: 7.0,
            distinct_data_services: 1,
            now: NOW,
        }
    }

    /// The published weights sum to a hundred. A weight added without adjusting the others would
    /// silently rescale every score on the site.
    #[test]
    fn the_weights_sum_to_one_hundred() {
        assert_eq!(WEIGHTS.iter().map(|(_, w)| w).sum::<i32>(), 100);
        assert_eq!(WEIGHTS.len(), 11);
    }

    #[test]
    fn a_perfect_indexer_scores_a_hundred() {
        let input = Input {
            reo: Reo::Eligible,
            self_stake_grt: 20_000_000.0,
            allocated_grt: 1_000_000.0,
            delegation_utilization: 10.0,
            reward_cut_ppm: 0.0,
            query_fees_collected_grt: 200_000.0,
            net_flow_grt_7d: 100_000.0,
            delegator_apr: 25.0,
            distinct_data_services: 3,
            delegator_parameter_cooldown: 86_400,
            ..base()
        };
        let s = calculate(&input);
        assert_eq!(s.composite, 100, "{:?}", s.breakdown);
        assert_eq!(s.grade, Grade::A);
    }

    /// What taking everything actually costs, which is less than the published design principle
    /// claims.
    ///
    /// The dashboard says "an operationally excellent indexer that takes 100% of rewards still
    /// scores poorly because delegators earn nothing". The arithmetic disagrees: cut is weighted
    /// at ten and stability at six, so an otherwise flawless indexer taking everything scores
    /// **70, a B**. This test pins the real number rather than the aspiration. Either the weights
    /// or the claim should move; that is a decision about the model, not a defect in this port.
    #[test]
    fn taking_everything_costs_thirty_points_but_not_the_grade() {
        let greedy = Input {
            reo: Reo::Eligible,
            self_stake_grt: 20_000_000.0,
            reward_cut_ppm: 1_000_000.0,
            query_fee_cut_ppm: 1_000_000.0,
            delegator_apr: 0.0,
            ..base()
        };
        let s = calculate(&greedy);
        assert_eq!(s.breakdown.delegator_cut, 0);
        assert_eq!(
            s.breakdown.cut_stability, 5,
            "a total cut caps stability rather than rewarding it"
        );
        assert_eq!(s.breakdown.delegator_apy, 0);
        assert_eq!(s.composite, 70, "breakdown {:?}", s.breakdown);
        assert_eq!(
            s.grade,
            Grade::B,
            "the published claim would need this to be D or F"
        );
    }

    /// Attracting delegation must not lower the self-stake score. Scoring the ratio would punish
    /// an indexer for being trusted.
    #[test]
    fn delegation_does_not_dilute_the_self_stake_score() {
        let lonely = Input {
            delegated_grt: 0.0,
            ..base()
        };
        let popular = Input {
            delegated_grt: 50_000_000.0,
            ..base()
        };
        assert_eq!(
            calculate(&lonely).breakdown.self_stake,
            calculate(&popular).breakdown.self_stake
        );
    }

    #[test]
    fn the_protocol_minimum_self_stake_scores_thirty_five() {
        assert_eq!(score_self_stake(100_000.0), 35);
        assert_eq!(score_self_stake(500_000.0), 65);
        assert_eq!(score_self_stake(1_000_000.0), 80);
        assert_eq!(score_self_stake(10_000_000.0), 100);
        assert_eq!(score_self_stake(0.0), 0);
    }

    #[test]
    fn an_unreadable_oracle_neither_rewards_nor_punishes() {
        assert_eq!(score_reo(Reo::Unknown), 50);
        assert_eq!(score_reo(Reo::Eligible), 100);
        assert_eq!(score_reo(Reo::Ineligible), 0);
    }

    #[test]
    fn a_full_delegation_pool_scores_zero_for_safety() {
        assert_eq!(score_over_delegation(100.0), 0);
        assert_eq!(score_over_delegation(99.0), 15);
        assert_eq!(score_over_delegation(49.0), 100);
    }

    #[test]
    fn no_delegation_history_is_neutral_rather_than_a_warning() {
        assert_eq!(score_delegation_trend(0.0, 0.0), 50);
        assert_eq!(score_delegation_trend(-100.0, 0.0), 50);
    }

    #[test]
    fn the_effective_cut_beats_the_raw_one_when_it_is_known() {
        let raw = score_delegator_cut(500_000.0, 0.0, None);
        let effective = score_delegator_cut(500_000.0, 0.0, Some(10.0));
        assert_eq!(raw, 35, "50% raw cut");
        assert_eq!(effective, 85, "10% effective cut");
    }

    #[test]
    fn taking_every_query_fee_costs_fifteen_points() {
        assert_eq!(
            score_delegator_cut(0.0, 1_000_000.0, None),
            score_delegator_cut(0.0, 0.0, None) - 15
        );
    }

    #[test]
    fn allocating_without_visible_provision_gets_partial_credit_not_zero() {
        assert_eq!(score_allocation_efficiency(5, 100.0, None), 40);
        assert_eq!(score_allocation_efficiency(0, 100.0, Some(100.0)), 0);
        assert_eq!(score_allocation_efficiency(5, 800.0, Some(1000.0)), 100);
    }

    #[test]
    fn the_served_gap_replaces_query_fees_when_it_is_known() {
        let with_gap = Input {
            served_gap: Some(0.0),
            query_fees_collected_grt: 0.0,
            ..base()
        };
        assert_eq!(
            calculate(&with_gap).breakdown.query_volume,
            100,
            "no gap is a perfect score"
        );
        let bad_gap = Input {
            served_gap: Some(1.0),
            ..base()
        };
        assert_eq!(calculate(&bad_gap).breakdown.query_volume, 0);
    }

    #[test]
    fn grades_fall_on_the_published_boundaries() {
        assert_eq!(grade_from(80), Grade::A);
        assert_eq!(grade_from(79), Grade::B);
        assert_eq!(grade_from(65), Grade::B);
        assert_eq!(grade_from(64), Grade::C);
        assert_eq!(grade_from(50), Grade::C);
        assert_eq!(grade_from(49), Grade::D);
        assert_eq!(grade_from(35), Grade::D);
        assert_eq!(grade_from(34), Grade::F);
    }

    #[test]
    fn a_score_is_always_in_range() {
        for stake in [0.0, 1.0, 1e6, 1e9] {
            for cut in [0.0, 500_000.0, 1_000_000.0] {
                for util in [0.0, 50.0, 100.0, 150.0] {
                    let s = calculate(&Input {
                        self_stake_grt: stake,
                        reward_cut_ppm: cut,
                        delegation_utilization: util,
                        ..base()
                    });
                    assert!(
                        (0..=100).contains(&s.composite),
                        "composite {}",
                        s.composite
                    );
                }
            }
        }
    }
}
