//! `/api/delegate/recommend`: which indexer to delegate to, given four preference sliders.
//!
//! The one route in this service that answers in the **frontend's** shape rather than in ours.
//! Everywhere else the dashboard puts `/api/indexers-enriched` through
//! `src/lib/enriched-normalise.ts` before reading a field; here it reads `indexer.id`,
//! `indexer.delegationCapacity.utilizationPercent` and `indexer.stakedTokens` straight off the
//! response. Serving our own row names would render the page blank in the same way the cutover
//! did on 2026-09-07, so `legacy_indexer` below is a port of `fromKittiwake` rather than a new
//! serialisation.

use kittiwake_db::EnrichedIndexer;
use kittiwake_score::{Breakdown, WEIGHTS};
use serde_json::{Value, json};

/// Which dimensions each slider amplifies. Every name here must be a key of `WEIGHTS`; a test
/// below holds that, because a stray one would be created at zero and then normalised against,
/// quietly shrinking every real weight.
const PREF_DIMS: [(&str, &[&str]); 4] = [
    ("returns", &["delegatorAPY", "delegatorCut"]),
    ("stability", &["cutStability"]),
    ("safety", &["overDelegation", "selfStake"]),
    ("network", &["queryVolume", "allocationEfficiency", "reo"]),
];

/// The sliders, each 0 to 10 with 5 as neutral.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prefs {
    pub returns: f64,
    pub stability: f64,
    pub safety: f64,
    pub network: f64,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            returns: 5.0,
            stability: 5.0,
            safety: 5.0,
            network: 5.0,
        }
    }
}

impl Prefs {
    /// A slider by name, clamped. Anything unparseable is the neutral 5 rather than zero: a
    /// mistyped query parameter should not silently switch a dimension off.
    pub fn slider(&self, name: &str) -> f64 {
        let raw = match name {
            "returns" => self.returns,
            "stability" => self.stability,
            "safety" => self.safety,
            "network" => self.network,
            _ => 5.0,
        };
        if raw.is_finite() {
            raw.clamp(0.0, 10.0)
        } else {
            5.0
        }
    }
}

/// The published weights, amplified by the sliders and renormalised to sum to a hundred.
///
/// Kept in `WEIGHTS` order rather than sorted, because `reasons` picks the top three contributions
/// and a tie between two dimensions is broken by this order at both ends of the migration.
pub fn weights(prefs: &Prefs) -> Vec<(&'static str, f64)> {
    let mut w: Vec<(&'static str, f64)> = WEIGHTS.iter().map(|(k, v)| (*k, *v as f64)).collect();
    for (pref, dims) in PREF_DIMS {
        let mult = prefs.slider(pref) / 5.0;
        for (key, value) in w.iter_mut() {
            if dims.contains(key) {
                *value *= mult;
            }
        }
    }
    let total: f64 = w.iter().map(|(_, v)| v).sum();
    if total == 0.0 {
        return w;
    }
    let factor = 100.0 / total;
    for (_, value) in w.iter_mut() {
        *value *= factor;
    }
    w
}

/// The per-dimension score the refresh job stored, or `None` for a row it has not written one for.
pub fn breakdown(row: &EnrichedIndexer) -> Option<Breakdown> {
    serde_json::from_value(row.score_breakdown.clone()?).ok()
}

/// What the ranking sorts on.
///
/// Without a breakdown, the composite score. Defaulting a missing one to zeroes would score every
/// indexer 0 and hand a delegator whichever one sorted first, presented as a considered pick.
pub fn rank_score(
    row: &EnrichedIndexer,
    breakdown: Option<&Breakdown>,
    weights: &[(&str, f64)],
) -> f64 {
    match breakdown {
        None => num(row.score.as_deref()),
        Some(b) => weights
            .iter()
            .map(|(dim, w)| b.dimension(dim).unwrap_or(0) as f64 * w / 100.0)
            .sum(),
    }
}

/// Whether this indexer can be recommended at all: oracle-eligible, with delegation headroom, and
/// not taking almost everything.
///
/// The cut comparison is in PPM because that is the unit the incumbent filters in, and our column
/// is a percentage. Comparing 54.9 against 900000 is always true, which would recommend an indexer
/// taking the lot.
pub fn eligible(row: &EnrichedIndexer) -> bool {
    row.reo_status.as_deref() == Some("eligible")
        && num(row.delegation_capacity_pct.as_deref()) < 90.0
        && reward_cut_ppm(row) < 900_000.0
}

fn reward_cut_ppm(row: &EnrichedIndexer) -> f64 {
    (num(row.reward_cut.as_deref()) * 10_000.0).round()
}

/// Why this indexer, in the order the dimensions contributed.
pub fn reasons(
    row: &EnrichedIndexer,
    breakdown: Option<&Breakdown>,
    weights: &[(&str, f64)],
) -> Vec<String> {
    let Some(b) = breakdown else {
        // Say plainly that the sliders did not move anything, rather than dressing a composite
        // ranking up as a preference-weighted pick.
        let mut out = vec![format!(
            "Overall score {}/100",
            to_fixed(num(row.score.as_deref()), 0)
        )];
        let apr = num(row.delegator_apr.as_deref());
        if apr != 0.0 {
            out.push(format!("{}% estimated APR", to_fixed(apr, 1)));
        }
        out.push(format!(
            "{}% effective cut",
            to_fixed(num(row.effective_cut.as_deref()), 1)
        ));
        out.push(
            "Ranked on overall score; preference sliders need per-dimension data this feed does \
             not carry"
                .into(),
        );
        return out;
    };

    let mut ranked: Vec<(&str, f64)> = weights
        .iter()
        .map(|(dim, w)| (*dim, b.dimension(dim).unwrap_or(0) as f64 * w / 100.0))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
        .into_iter()
        .take(3)
        .filter_map(|(dim, _)| reason_for(row, dim))
        .collect()
}

fn reason_for(row: &EnrichedIndexer, dim: &str) -> Option<String> {
    let text = match dim {
        "delegatorAPY" => {
            let apy = row
                .rolling_apy_30d
                .as_deref()
                .map(|v| num(Some(v)))
                .unwrap_or_else(|| num(row.delegator_apr.as_deref()));
            format!("{}% estimated APR", to_fixed(apy, 1))
        }
        "delegatorCut" => {
            let share = (1.0 - num(row.reward_cut.as_deref()) / 100.0) * 100.0;
            format!("{}% delegator reward share", to_fixed(share, 0))
        }
        // The incumbent reads `lastDelegationParameterUpdate`, which the normaliser sets to zero
        // because our feed does not carry it, so the "cut stable" line it renders is measured from
        // the epoch. Omitted here rather than ported wrong; it returns with the column.
        "cutStability" => return None,
        "overDelegation" => format!(
            "{}% delegation capacity used",
            to_fixed(num(row.delegation_capacity_pct.as_deref()), 0)
        ),
        "selfStake" => {
            let k = (num(row.self_stake_grt.as_deref()) / 1000.0).round();
            if k >= 1000.0 {
                format!("{}M GRT self-staked", to_fixed(k / 1000.0, 1))
            } else {
                format!("{}K GRT self-staked", to_fixed(k, 0))
            }
        }
        "reo" => "REO eligible".into(),
        "queryVolume" => {
            let fees = num(row.query_fees_collected_grt.as_deref());
            if fees >= 1000.0 {
                format!("{}K GRT in query fees", to_fixed(fees / 1000.0, 0))
            } else {
                format!("{} GRT in query fees", to_fixed(fees, 0))
            }
        }
        "allocationEfficiency" => "High allocation efficiency".into(),
        "transparency" => match &row.ens_name {
            Some(ens) => format!("ENS: {ens}"),
            None => "Verified identity".into(),
        },
        "delegationTrend" => {
            if num(row.net_flow_grt_7d.as_deref()) > 0.0 {
                "Net delegation inflow (7d)".into()
            } else {
                "Stable delegation (7d)".into()
            }
        }
        _ => return None,
    };
    Some(text)
}

/// One row in the shape `src/lib/enriched-normalise.ts` would have produced from it.
///
/// Absent fields are null or zero on purpose, matching the normaliser: the delegate page renders
/// a dash for a missing figure, and a placeholder would read as a number somebody measured.
pub fn legacy_indexer(row: &EnrichedIndexer) -> Value {
    let self_stake = num(row.self_stake_grt.as_deref());
    let delegated = num(row.delegated_grt.as_deref());
    // Only the three the page styles on. Anything else is `unknown`, because an unrecognised
    // status renders as an unstyled badge rather than as the warning it probably is.
    let reo = row
        .reo_status
        .as_deref()
        .filter(|s| matches!(*s, "eligible" | "ineligible" | "unknown"))
        .unwrap_or("unknown");
    json!({
        "id": row.address.to_lowercase(),
        "name": row.name,
        "ensName": row.ens_name,
        "url": row.url,
        "geoHash": row.geo_hash,

        "selfStakeGRT": jsnum(self_stake),
        "delegatedGRT": jsnum(delegated),
        "delegatedThawingGRT": 0,
        "delegatorAPR": jsnum(num(row.delegator_apr.as_deref())),
        "effectiveCut": jsnum(num(row.effective_cut.as_deref())),
        "indexingRewardCut": jsnum(reward_cut_ppm(row)),
        "allocationCount": row.allocation_count.unwrap_or(0),
        "allocatedTokens": grt_to_wei(row.allocated_grt.as_deref()),
        "delegationCapacity": {
            "maxCapacity": 0,
            "usedCapacity": 0,
            "availableCapacity": 0,
            "utilizationPercent": jsnum(num(row.delegation_capacity_pct.as_deref())),
        },
        "reoStatus": reo,
        "recentActivity": {
            "delegationsIn7d": row.delegations_in_7d.unwrap_or(0),
            "undelegationsIn7d": row.undelegations_in_7d.unwrap_or(0),
            "netFlowGRT": jsnum(num(row.net_flow_grt_7d.as_deref())),
        },
        "score": row.score.as_deref().map(|s| jsnum(num(Some(s)))),
        "scoreGrade": row.score_grade,
        "scoreBreakdown": breakdown(row).map_or(Value::Null, |b| {
            WEIGHTS
                .iter()
                .map(|(dim, _)| (dim.to_string(), json!(b.dimension(dim).unwrap_or(0))))
                .collect::<serde_json::Map<_, _>>()
                .into()
        }),

        "queryFeeCut": 0,
        "delegatorParameterCooldown": 0,
        "lastDelegationParameterUpdate": 0,

        "rollingAPY30d": row.rolling_apy_30d.as_deref().map(|s| jsnum(num(Some(s)))),
        "rollingAPY90d": row.rolling_apy_90d.as_deref().map(|s| jsnum(num(Some(s)))),
        "queryFeesCollectedGRT": row.query_fees_collected_grt.as_deref().map(|s| jsnum(num(Some(s)))),
        "overDelegationDilution": Value::Null,
        "reoSource": Value::Null,
        "reoRenewalTimestamp": Value::Null,
        "reoExpiresAt": Value::Null,
        "reoDaysRemaining": Value::Null,
        "stakedTokens": grt_to_wei(row.self_stake_grt.as_deref()),
        "delegatedTokens": grt_to_wei(row.delegated_grt.as_deref()),
        "lockedTokens": "0",
        "rewardsEarned": grt_to_wei(row.rewards_earned_grt.as_deref()),
        "delegatorShares": "0",
        "createdAt": 0,
    })
}

/// GRT back to wei as a decimal string.
///
/// The dashboard reads `stakedTokens` and `allocatedTokens` through `weiToGRT`, so passing GRT
/// through unscaled shows every allocation as zero: a wrong number, which is worse than the dash
/// it replaces.
fn grt_to_wei(grt: Option<&str>) -> String {
    let Some(grt) = grt else { return "0".into() };
    let (whole, frac) = grt.split_once('.').unwrap_or((grt, ""));
    let padded: String = frac
        .chars()
        .chain(std::iter::repeat('0'))
        .take(18)
        .collect();
    let Ok(whole) = (if whole.is_empty() { "0" } else { whole }).parse::<u128>() else {
        return "0".into();
    };
    let Ok(frac) = padded.parse::<u128>() else {
        return "0".into();
    };
    match whole
        .checked_mul(10u128.pow(18))
        .and_then(|w| w.checked_add(frac))
    {
        Some(wei) => wei.to_string(),
        None => "0".into(),
    }
}

/// `Number.prototype.toFixed`, which is not `{:.n}`.
///
/// Rust rounds a tie to even and JavaScript rounds it away from zero, so an effective cut stored
/// as `32.25` is `32.3` on the dashboard today and would silently become `32.2` here. Built from
/// the rounded integer rather than by formatting a divided float, so there is only ever one
/// rounding step.
fn to_fixed(value: f64, decimals: u32) -> String {
    if !value.is_finite() {
        return "0".into();
    }
    let scale = 10f64.powi(decimals as i32);
    let n = (value.abs() * scale).round() as u128;
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if decimals == 0 {
        return format!("{sign}{n}");
    }
    let digits = n.to_string();
    let width = decimals as usize;
    let padded = format!("{digits:0>width$}", width = width + 1);
    let (whole, frac) = padded.split_at(padded.len() - width);
    format!("{sign}{whole}.{frac}")
}

/// A number written the way `JSON.stringify` writes it: `2000000`, never `2000000.0`.
///
/// serde renders an `f64` with its point on, and the parity harness compares parsed numbers, so
/// an untouched whole number would read as a value change on every row.
fn jsnum(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        return json!(value as i64);
    }
    json!(value)
}

/// A numeric column that Postgres sent as text. Absent, unparseable and non-finite all read as
/// zero, which is what the normaliser's `num` does.
fn num(value: Option<&str>) -> f64 {
    match value.and_then(|v| v.trim().parse::<f64>().ok()) {
        Some(n) if n.is_finite() => n,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: &str) -> EnrichedIndexer {
        EnrichedIndexer {
            address: address.into(),
            name: None,
            ens_name: None,
            url: Some("https://indexer.example".into()),
            geo_hash: None,
            self_stake_grt: Some("1500000.5".into()),
            delegated_grt: Some("2000000".into()),
            allocated_grt: Some("1200000".into()),
            reward_cut: Some("54.9".into()),
            effective_cut: Some("32.25".into()),
            delegator_apr: Some("16.035678036558".into()),
            delegation_capacity_pct: Some("41.2".into()),
            score: Some("81".into()),
            score_grade: Some("A".into()),
            allocation_count: Some(12),
            delegations_in_7d: Some(3),
            undelegations_in_7d: Some(1),
            net_flow_grt_7d: Some("4200".into()),
            query_fees_collected_grt: Some("125000".into()),
            rewards_earned_grt: Some("900".into()),
            rolling_apy_30d: Some("14.5".into()),
            rolling_apy_90d: None,
            distinct_data_services: Some(2),
            reo_status: Some("eligible".into()),
            last_updated: None,
            q_score: None,
            score_breakdown: None,
        }
    }

    #[test]
    fn a_named_indexer_is_recommended_under_its_name() {
        let named = EnrichedIndexer {
            name: Some("pinax.eth".into()),
            ens_name: Some("pinax.eth".into()),
            ..row("0xaabb")
        };
        let value = legacy_indexer(&named);
        assert_eq!(value["name"], "pinax.eth");
        assert_eq!(value["ensName"], "pinax.eth");
        assert_eq!(
            reason_for(&named, "transparency").as_deref(),
            Some("ENS: pinax.eth")
        );
        assert_eq!(
            reason_for(&row("0xaabb"), "transparency").as_deref(),
            Some("Verified identity")
        );
    }

    /// Every slider names dimensions that exist. One that did not would be created at zero and
    /// then counted in the normalisation, shrinking every real weight by a few percent.
    /// The golden file, and the only test here that proves the delegate page will render.
    ///
    /// Produced by running the fixture above through the incumbent's own
    /// `normaliseEnrichedResponse` (`nuthatch-org/lodestar`, `src/lib/enriched-normalise.ts`)
    /// under tsx and pasting the result. Whole numbers are written without a point because that
    /// is what `JSON.stringify` does, and the parity harness compares parsed numbers.
    #[test]
    fn the_row_matches_what_the_incumbent_normaliser_produces() {
        let expected: Value = serde_json::from_str(
            r#"{
        "id": "0xaabb",
        "name": null,
        "ensName": null,
        "url": "https://indexer.example",
        "geoHash": null,
        "selfStakeGRT": 1500000.5,
        "delegatedGRT": 2000000,
        "delegatedThawingGRT": 0,
        "delegatorAPR": 16.035678036558,
        "effectiveCut": 32.25,
        "indexingRewardCut": 549000,
        "allocationCount": 12,
        "allocatedTokens": "1200000000000000000000000",
        "delegationCapacity": {
                "maxCapacity": 0,
                "usedCapacity": 0,
                "availableCapacity": 0,
                "utilizationPercent": 41.2
        },
        "reoStatus": "eligible",
        "recentActivity": {
                "delegationsIn7d": 3,
                "undelegationsIn7d": 1,
                "netFlowGRT": 4200
        },
        "score": 81,
        "scoreGrade": "A",
        "scoreBreakdown": null,
        "queryFeeCut": 0,
        "delegatorParameterCooldown": 0,
        "lastDelegationParameterUpdate": 0,
        "rollingAPY30d": 14.5,
        "rollingAPY90d": null,
        "queryFeesCollectedGRT": 125000,
        "overDelegationDilution": null,
        "reoSource": null,
        "reoRenewalTimestamp": null,
        "reoExpiresAt": null,
        "reoDaysRemaining": null,
        "stakedTokens": "1500000500000000000000000",
        "delegatedTokens": "2000000000000000000000000",
        "lockedTokens": "0",
        "rewardsEarned": "900000000000000000000",
        "delegatorShares": "0",
        "createdAt": 0
    }"#,
        )
        .unwrap();
        assert_eq!(legacy_indexer(&row("0xAaBb")), expected);
    }

    #[test]
    fn every_preference_names_real_dimensions() {
        for (_, dims) in PREF_DIMS {
            for dim in dims {
                assert!(
                    WEIGHTS.iter().any(|(k, _)| k == dim),
                    "{dim} is not a scored dimension"
                );
            }
        }
    }

    #[test]
    fn the_weights_still_sum_to_a_hundred_whatever_the_sliders_say() {
        for prefs in [
            Prefs::default(),
            Prefs {
                returns: 10.0,
                stability: 0.0,
                safety: 0.0,
                network: 0.0,
            },
            Prefs {
                returns: 0.0,
                stability: 10.0,
                safety: 3.0,
                network: 7.0,
            },
        ] {
            let total: f64 = weights(&prefs).iter().map(|(_, v)| v).sum();
            assert!((total - 100.0).abs() < 1e-9, "{prefs:?} summed to {total}");
        }
    }

    /// Neutral sliders must leave the published weights exactly as published, or the default
    /// recommendation is not the one the score page explains.
    #[test]
    fn neutral_sliders_are_the_published_weights() {
        for (key, value) in weights(&Prefs::default()) {
            let published = WEIGHTS.iter().find(|(k, _)| *k == key).unwrap().1 as f64;
            assert!((value - published).abs() < 1e-9, "{key}: {value}");
        }
    }

    /// A slider at zero switches its dimensions off; the rest absorb the weight.
    #[test]
    fn a_slider_at_zero_removes_its_dimensions() {
        let w = weights(&Prefs {
            returns: 0.0,
            ..Prefs::default()
        });
        let get = |key: &str| w.iter().find(|(k, _)| *k == key).unwrap().1;
        assert_eq!(get("delegatorAPY"), 0.0);
        assert_eq!(get("delegatorCut"), 0.0);
        assert!(get("reo") > 20.0, "reo was {}", get("reo"));
    }

    /// The one that decides who gets recommended. Our column is a percentage and the filter is in
    /// PPM: unconverted, an indexer taking 95% compares 95 against 900000 and passes.
    #[test]
    fn the_cut_filter_is_in_ppm_and_excludes_the_greedy() {
        let mut greedy = row("0xaa");
        greedy.reward_cut = Some("95".into());
        assert!(!eligible(&greedy));

        let mut exactly_ninety = row("0xbb");
        exactly_ninety.reward_cut = Some("90".into());
        assert!(!eligible(&exactly_ninety));

        let mut just_under = row("0xcc");
        just_under.reward_cut = Some("89.99".into());
        assert!(eligible(&just_under));
    }

    #[test]
    fn eligibility_needs_the_oracle_and_headroom() {
        assert!(eligible(&row("0xaa")));

        let mut ineligible = row("0xaa");
        ineligible.reo_status = Some("unknown".into());
        assert!(!eligible(&ineligible));

        let mut full = row("0xaa");
        full.delegation_capacity_pct = Some("90".into());
        assert!(!eligible(&full));
    }

    /// Without a breakdown the ranking is the composite score, and the reasons say so rather than
    /// crediting sliders that moved nothing.
    #[test]
    fn without_a_breakdown_the_ranking_is_the_composite_score() {
        let w = weights(&Prefs::default());
        let r = row("0xaa");
        assert_eq!(rank_score(&r, None, &w), 81.0);

        let said = reasons(&r, None, &w);
        assert_eq!(said[0], "Overall score 81/100");
        assert_eq!(said[1], "16.0% estimated APR");
        assert_eq!(said[2], "32.3% effective cut");
        assert!(said[3].contains("preference sliders"), "{}", said[3]);
    }

    /// The refresh job writes the breakdown with serde and this reads it back with serde. A rename
    /// on either side would turn every stored row into `None` and silently switch the sliders off.
    #[test]
    fn a_stored_breakdown_is_read_back_as_written() {
        let b = Breakdown {
            reo: 100,
            allocation_efficiency: 40,
            delegator_apy: 70,
            ..Breakdown::default()
        };
        let mut r = row("0xaa");
        r.score_breakdown = Some(serde_json::to_value(b).unwrap());
        assert_eq!(breakdown(&r), Some(b));

        // Served under the dimension names lodestar's `scoreBreakdown` type uses.
        let value = legacy_indexer(&r);
        assert_eq!(value["scoreBreakdown"]["allocationEfficiency"], 40);
        assert_eq!(value["scoreBreakdown"]["delegatorAPY"], 70);
    }

    /// The path that comes alive when the breakdown is stored: the sliders reorder the answer.
    #[test]
    fn a_breakdown_lets_the_sliders_reorder_the_answer() {
        let earner = Breakdown {
            delegator_apy: 100,
            delegator_cut: 100,
            reo: 0,
            ..Breakdown::default()
        };
        let compliant = Breakdown {
            reo: 100,
            query_volume: 100,
            allocation_efficiency: 100,
            ..Breakdown::default()
        };
        let r = row("0xaa");

        let neutral = weights(&Prefs::default());
        assert!(
            rank_score(&r, Some(&compliant), &neutral) > rank_score(&r, Some(&earner), &neutral),
            "on the published weights the compliant one leads"
        );

        let for_returns = weights(&Prefs {
            returns: 10.0,
            network: 0.0,
            ..Prefs::default()
        });
        assert!(
            rank_score(&r, Some(&earner), &for_returns)
                > rank_score(&r, Some(&compliant), &for_returns),
            "with returns wound up and network off, the earner leads"
        );
    }

    #[test]
    fn a_breakdown_names_the_three_dimensions_that_earned_it() {
        let b = Breakdown {
            reo: 100,
            self_stake: 100,
            allocation_efficiency: 100,
            ..Breakdown::default()
        };
        let said = reasons(&row("0xaa"), Some(&b), &weights(&Prefs::default()));
        assert_eq!(
            said,
            vec![
                "REO eligible",
                "High allocation efficiency",
                "1.5M GRT self-staked",
            ]
        );
    }

    /// The tie that Rust and JavaScript disagree about. `32.25` renders as `32.3` on the
    /// dashboard, and `{:.1}` would put `32.2` in the reason beside it.
    #[test]
    fn a_half_rounds_away_from_zero_the_way_javascript_does() {
        assert_eq!(to_fixed(32.25, 1), "32.3");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(1.5, 0), "2");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(16.035678036558, 1), "16.0");
        assert_eq!(to_fixed(0.04, 1), "0.0");
        assert_eq!(to_fixed(1.0, 1), "1.0");
        assert_eq!(to_fixed(f64::NAN, 1), "0");
    }

    /// GRT to wei, because the dashboard divides by 1e18 on the way out. Getting this wrong shows
    /// every allocation as zero.
    #[test]
    fn grt_becomes_wei_without_losing_the_fraction() {
        assert_eq!(grt_to_wei(Some("1")), "1000000000000000000");
        assert_eq!(grt_to_wei(Some("1500000.5")), "1500000500000000000000000");
        assert_eq!(grt_to_wei(Some("0.000000000000000001")), "1");
        assert_eq!(grt_to_wei(Some("0")), "0");
        assert_eq!(grt_to_wei(None), "0");
        // More precision than wei has. Truncated rather than rounded, and never a panic.
        assert_eq!(grt_to_wei(Some("0.0000000000000000019")), "1");
        assert_eq!(grt_to_wei(Some("not a number")), "0");
    }

    /// The delegate page reads these off the response without normalising, so the names are the
    /// contract. `id` is lowercased because it is compared against the connected wallet's choice.
    #[test]
    fn the_row_is_served_in_the_shape_the_delegate_page_reads() {
        let value = legacy_indexer(&row("0xAaBb"));
        assert_eq!(value["id"], "0xaabb");
        assert_eq!(value["indexingRewardCut"], 549_000.0);
        assert_eq!(value["delegationCapacity"]["utilizationPercent"], 41.2);
        assert_eq!(value["recentActivity"]["delegationsIn7d"], 3);
        assert_eq!(value["stakedTokens"], "1500000500000000000000000");
        assert_eq!(value["scoreGrade"], "A");
        assert!(value["scoreBreakdown"].is_null());
        assert!(value["ensName"].is_null());
        assert_eq!(value["rollingAPY90d"], Value::Null);
    }

    /// An unknown REO string is `unknown`, not passed through: the page styles on the three it
    /// knows and would render an unstyled badge for anything else.
    #[test]
    fn an_unrecognised_oracle_status_reads_as_unknown() {
        let mut r = row("0xaa");
        r.reo_status = Some("probably?".into());
        assert_eq!(legacy_indexer(&r)["reoStatus"], "unknown");
    }
}
