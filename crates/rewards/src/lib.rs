//! What a delegator actually earns.
//!
//! Ported from `src/lib/rewards.ts`. These are the figures the composite score's two
//! delegator-facing dimensions are built from, and they are the ones most easily got subtly wrong,
//! so each carries the reasoning rather than only the arithmetic.

pub mod pnl;

use serde::Serialize;

/// One allocation, as far as the reward calculation cares.
#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    pub allocated_grt: f64,
    /// Signal on the deployment this allocation is against.
    pub deployment_signal_grt: f64,
    /// Total stake allocated to that deployment by everyone.
    pub deployment_stake_grt: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AprBreakdown {
    pub annual_indexing_rewards: f64,
    pub annual_delegator_rewards: f64,
    pub active_base: f64,
    pub raw_cut: f64,
    pub effective_cut: f64,
    pub apr: f64,
    /// The same figure without the P95 clamp, kept so a reader can see what the clamp cost. A
    /// number you cannot compare to its unclamped self is a number you are asked to trust.
    pub apr_uncapped: f64,
}

/// Signal-weighted APR for one indexer's delegators.
///
/// The shape of it: rewards on an allocation are proportional to the deployment's signal-to-stake
/// ratio, so an allocation against a heavily signalled and lightly staked subgraph earns far more
/// per token. That is real, but the tail of that distribution is not something a delegator can
/// count on, so the ratio is clamped at the indexer's own 95th percentile before the sum. Without
/// the clamp, a single freakishly over-signalled deployment makes an indexer look twice as
/// rewarding as it is.
///
/// `effective_cut` is what delegators actually experience once the indexer's own stake ratio is
/// accounted for; when it is known and sane it beats the raw cut, which flatters indexers with a
/// large self-stake.
#[allow(clippy::too_many_arguments)]
pub fn delegator_apr_breakdown(
    allocations: &[Allocation],
    protocol_cut_ppm: f64,
    delegated_grt: f64,
    total_network_signal_grt: f64,
    annual_issuance_grt: f64,
    effective_cut: Option<f64>,
    delegated_stake_ratio: Option<f64>,
) -> AprBreakdown {
    let raw_cut = protocol_cut_ppm / 1_000_000.0;
    let eff_valid = effective_cut.is_some_and(|c| c.is_finite() && c <= 1.0);
    let eff_cut = if eff_valid {
        effective_cut.unwrap()
    } else {
        raw_cut
    };

    let empty = AprBreakdown {
        active_base: delegated_grt,
        raw_cut,
        effective_cut: eff_cut,
        ..Default::default()
    };

    if delegated_grt <= 0.0 || total_network_signal_grt == 0.0 || allocations.is_empty() {
        return empty;
    }

    // An allocation against a deployment with no signal or no stake earns nothing to speak of and
    // would divide by zero on the way to saying so.
    let usable: Vec<(f64, f64)> = allocations
        .iter()
        .filter(|a| a.deployment_signal_grt > 0.0 && a.deployment_stake_grt > 0.0)
        .map(|a| {
            (
                a.allocated_grt,
                a.deployment_signal_grt / a.deployment_stake_grt,
            )
        })
        .collect();

    if usable.is_empty() {
        return empty;
    }

    let mut ratios: Vec<f64> = usable.iter().map(|(_, r)| *r).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95_index = ((ratios.len() as f64 * 0.95).floor() as usize).min(ratios.len() - 1);
    let cap = ratios[p95_index];

    let mut total = 0.0;
    let mut total_uncapped = 0.0;
    for (allocated, ratio) in &usable {
        total += annual_issuance_grt * ratio.min(cap) * allocated / total_network_signal_grt;
        total_uncapped += annual_issuance_grt * ratio * allocated / total_network_signal_grt;
    }

    // With a known effective cut and stake ratio, the delegators' share is the part of the pool
    // that is theirs times what survives the cut. Without them, fall back to the raw cut, which
    // attributes the whole remainder to delegators and so runs high.
    let delegator_fraction = match (eff_valid, delegated_stake_ratio) {
        (true, Some(r)) if (0.0..=1.0).contains(&r) => r * (1.0 - effective_cut.unwrap()),
        _ => 1.0 - raw_cut,
    };

    let delegator_rewards = total * delegator_fraction;
    let delegator_rewards_uncapped = total_uncapped * delegator_fraction;

    AprBreakdown {
        annual_indexing_rewards: total,
        annual_delegator_rewards: delegator_rewards,
        active_base: delegated_grt,
        raw_cut,
        effective_cut: eff_cut,
        // Capped at 100: above that the figure is an artefact of a tiny delegation base rather
        // than a return anyone will receive.
        apr: ((delegator_rewards / delegated_grt) * 100.0).min(100.0),
        apr_uncapped: ((delegator_rewards_uncapped / delegated_grt) * 100.0).min(100.0),
    }
}

/// Realised return over a window, from what closed allocations actually paid delegators.
///
/// Preferred over the estimate above when there is enough history: it is what happened rather than
/// what should happen.
pub fn rolling_apy(closed: &[(f64, i64)], delegated_grt: f64, window_days: f64, now: i64) -> f64 {
    if delegated_grt <= 0.0 || closed.is_empty() || window_days <= 0.0 {
        return 0.0;
    }
    let cutoff = now - (window_days as i64) * 86_400;
    let rewards: f64 = closed
        .iter()
        .filter(|(_, at)| *at >= cutoff)
        .map(|(r, _)| r)
        .sum();
    if rewards <= 0.0 {
        return 0.0;
    }
    (rewards / delegated_grt) * (365.0 / window_days) * 100.0
}

/// Tokens per share in an indexer's delegation pool.
///
/// Thawing tokens are subtracted: they are spoken for and no longer back a share.
pub fn pool_exchange_rate(delegated_grt: f64, thawing_grt: f64, shares: f64) -> f64 {
    if shares == 0.0 {
        return 1.0;
    }
    (delegated_grt - thawing_grt) / shares
}

/// Annualised growth of the pool's exchange rate between two points.
pub fn exchange_rate_apy(current: f64, historical: f64, window_days: f64) -> f64 {
    if historical <= 0.0 || current <= 0.0 || window_days <= 0.0 {
        return 0.0;
    }
    let ratio = current / historical;
    // A pool that shrank is reported as zero rather than as a negative return: the loss is real
    // but it is not an annual rate, and compounding it would be nonsense.
    if ratio <= 1.0 {
        return 0.0;
    }
    (ratio.powf(365.0 / window_days) - 1.0) * 100.0
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Capacity {
    pub max_grt: f64,
    pub used_grt: f64,
    pub available_grt: f64,
    pub utilization_percent: f64,
}

/// How much delegation an indexer can still take before diluting its existing delegators.
pub fn delegation_capacity(self_stake_grt: f64, delegated_grt: f64, ratio: f64) -> Capacity {
    let max = self_stake_grt * ratio;
    Capacity {
        max_grt: max,
        used_grt: delegated_grt,
        available_grt: (max - delegated_grt).max(0.0),
        // No capacity at all reads as fully used, not as empty: an indexer with no self-stake
        // cannot accept delegation, and reporting 0% would invite it.
        utilization_percent: if max > 0.0 {
            (delegated_grt / max) * 100.0
        } else {
            100.0
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_788_000_000;

    fn alloc(allocated: f64, signal: f64, stake: f64) -> Allocation {
        Allocation {
            allocated_grt: allocated,
            deployment_signal_grt: signal,
            deployment_stake_grt: stake,
        }
    }

    /// The clamp exists so one freakish deployment cannot double an indexer's advertised return.
    #[test]
    fn one_over_signalled_deployment_cannot_inflate_the_apr() {
        let mut allocations: Vec<Allocation> =
            (0..20).map(|_| alloc(1000.0, 100.0, 1000.0)).collect();
        // Issuance sized so both figures land well below the 100% cap; at the cap the comparison
        // would be between two ceilings and would prove nothing.
        let ordinary =
            delegator_apr_breakdown(&allocations, 100_000.0, 100_000.0, 1e6, 1e6, None, None);

        // One deployment with a thousand times the usual signal-to-stake ratio.
        allocations.push(alloc(1000.0, 100_000.0, 1000.0));
        let with_freak =
            delegator_apr_breakdown(&allocations, 100_000.0, 100_000.0, 1e6, 1e6, None, None);

        assert!(
            with_freak.apr < 50.0 && with_freak.apr_uncapped < 100.0,
            "both must be off the cap"
        );

        assert!(
            with_freak.apr_uncapped > with_freak.apr * 2.0,
            "unclamped {} vs clamped {}",
            with_freak.apr_uncapped,
            with_freak.apr
        );
        assert!(
            with_freak.apr < ordinary.apr * 1.3,
            "the clamp should keep the outlier from dominating: {} vs {}",
            with_freak.apr,
            ordinary.apr
        );
    }

    #[test]
    fn no_delegation_means_no_apr_rather_than_a_division_by_zero() {
        let b = delegator_apr_breakdown(&[alloc(1.0, 1.0, 1.0)], 0.0, 0.0, 1e6, 1e8, None, None);
        assert_eq!(b.apr, 0.0);
        assert!(b.apr.is_finite());
    }

    #[test]
    fn deployments_with_no_signal_are_skipped_not_divided_by() {
        let b = delegator_apr_breakdown(
            &[alloc(1000.0, 0.0, 1000.0), alloc(1000.0, 0.0, 0.0)],
            100_000.0,
            100_000.0,
            1e6,
            1e8,
            None,
            None,
        );
        assert_eq!(b.apr, 0.0);
        assert!(b.apr.is_finite());
    }

    #[test]
    fn the_effective_cut_is_used_only_when_it_is_sane() {
        let a = [alloc(1000.0, 100.0, 1000.0)];
        let nonsense =
            delegator_apr_breakdown(&a, 100_000.0, 1000.0, 1e6, 1e8, Some(1.5), Some(0.5));
        assert_eq!(
            nonsense.effective_cut, 0.1,
            "a cut above 1 is not a cut; fall back to raw"
        );
        let sane = delegator_apr_breakdown(&a, 100_000.0, 1000.0, 1e6, 1e8, Some(0.05), Some(0.5));
        assert_eq!(sane.effective_cut, 0.05);
    }

    #[test]
    fn an_apr_is_capped_at_one_hundred_percent() {
        let a = [alloc(1e9, 1e9, 1.0)];
        let b = delegator_apr_breakdown(&a, 0.0, 0.000_1, 1.0, 1e12, None, None);
        assert_eq!(b.apr, 100.0);
    }

    #[test]
    fn a_realised_return_counts_only_what_closed_inside_the_window() {
        let inside = NOW - 10 * 86_400;
        let outside = NOW - 40 * 86_400;
        let apy = rolling_apy(&[(100.0, inside), (900.0, outside)], 1000.0, 30.0, NOW);
        // 100 of rewards on 1000 delegated over 30 days, annualised.
        assert!((apy - (100.0 / 1000.0) * (365.0 / 30.0) * 100.0).abs() < 1e-9);
    }

    #[test]
    fn thawing_tokens_no_longer_back_a_share() {
        assert_eq!(pool_exchange_rate(1000.0, 0.0, 1000.0), 1.0);
        assert_eq!(pool_exchange_rate(1000.0, 200.0, 1000.0), 0.8);
        assert_eq!(
            pool_exchange_rate(1000.0, 0.0, 0.0),
            1.0,
            "no shares is a rate of one, not a panic"
        );
    }

    #[test]
    fn a_shrinking_pool_reports_zero_rather_than_a_compounded_loss() {
        assert_eq!(exchange_rate_apy(0.9, 1.0, 30.0), 0.0);
        assert!(exchange_rate_apy(1.01, 1.0, 30.0) > 0.0);
        assert_eq!(exchange_rate_apy(1.0, 0.0, 30.0), 0.0);
    }

    #[test]
    fn an_indexer_with_no_self_stake_is_full_not_empty() {
        let c = delegation_capacity(0.0, 0.0, 16.0);
        assert_eq!(c.utilization_percent, 100.0);
        assert_eq!(c.available_grt, 0.0);
    }

    #[test]
    fn capacity_is_self_stake_times_the_ratio() {
        let c = delegation_capacity(100_000.0, 800_000.0, 16.0);
        assert_eq!(c.max_grt, 1_600_000.0);
        assert_eq!(c.available_grt, 800_000.0);
        assert_eq!(c.utilization_percent, 50.0);
    }
}

/// L1 blocks in a year, which is what issuance is denominated against.
///
/// **One figure, in one place, resolving a split that had been live for months.** The dashboard
/// carried two: `network-math.ts` used 2,610,223 - 365.25 days at 12.09 seconds - for the issuance
/// percentage a reader sees, and `refresh.ts` used 2,628,000 - 365 days at a flat 12 - for the
/// scores those readers were ranking indexers by. About 0.7% apart, and apart since both were
/// written, so the number on the page and the number behind the ranking were never the same number.
///
/// 12.09 seconds is the measured average slot time including missed slots; 12 is the nominal one.
/// The measured figure is the more defensible, so it wins.
///
/// **Why this does not move any ranking.** Issuance scales every indexer's APR by the same factor,
/// and the score's APR bands are interpolated monotonically, so a uniform multiplier preserves
/// order exactly. Absolute scores shift by well under a point; nobody changes places.
pub const L1_BLOCKS_PER_YEAR: f64 = 2_610_223.0;

#[cfg(test)]
mod blocks_per_year_tests {
    use super::L1_BLOCKS_PER_YEAR;

    /// Pinned, because the whole value of this constant is that there is exactly one of it. A
    /// second figure appearing anywhere is the bug it was written to close.
    #[test]
    fn the_figure_is_the_measured_one_and_is_stated_once() {
        assert_eq!(L1_BLOCKS_PER_YEAR, 2_610_223.0);
        // 365.25 days at 12.09 seconds, rounded, which is where it comes from.
        assert_eq!(
            ((365.25_f64 * 24.0 * 3600.0) / 12.09).round(),
            L1_BLOCKS_PER_YEAR
        );
    }
}
