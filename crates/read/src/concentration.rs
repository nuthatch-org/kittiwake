//! How the network's allocated stake is distributed across quality bands.
//!
//! Pure and testable, which matters because the interesting claims here are about what the numbers
//! are *not*. An indexer we have not probed is not a bad indexer, and the whole shape of this
//! module is arranged so those two never merge. #67

use serde::Serialize;

/// One indexer's allocation and the quality we measured for it, if we measured one.
#[derive(Debug, Clone, Copy)]
pub struct Allocation {
    pub allocated_grt: f64,
    /// 0-100, or `None` when this indexer has not been measured.
    pub q_score: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Zero,
    Low,
    Unscored,
    Fair,
    Good,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Zero => "Providing ~0 value (Q<15)",
            Tier::Low => "Very low / narrow (Q 15–30)",
            Tier::Unscored => "Not measured",
            Tier::Fair => "Fair (Q 30–60)",
            Tier::Good => "Good (Q≥60)",
        }
    }
}

/// The order the bands are reported in, with `unscored` between the poor and the fair rather than
/// at either end: it is a gap in what we know, not a verdict at one extreme.
const ORDER: &[Tier] = &[
    Tier::Zero,
    Tier::Low,
    Tier::Unscored,
    Tier::Fair,
    Tier::Good,
];

fn tier_of(q: Option<f64>) -> Tier {
    match q {
        None => Tier::Unscored,
        Some(q) if q < 15.0 => Tier::Zero,
        Some(q) if q < 30.0 => Tier::Low,
        Some(q) if q < 60.0 => Tier::Fair,
        Some(_) => Tier::Good,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TierCapture {
    pub tier: Tier,
    pub label: &'static str,
    pub indexers: usize,
    pub alloc_grt: f64,
    /// 0..1 of the total allocation.
    pub alloc_share: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Concentration {
    pub total_allocated_grt: f64,
    pub indexer_count: usize,
    /// 0 is perfectly equal, approaching 1 is one entity holding everything.
    pub gini: f64,
    /// The fewest entities whose combined allocation exceeds half.
    pub nakamoto: usize,
    pub top_n_share: f64,
    pub top_n: usize,
    pub tiers: Vec<TierCapture>,
    /// Allocation held by indexers *measured* to be low or zero value.
    ///
    /// Unscored allocation is deliberately excluded. Folding it in would assert that every indexer
    /// outside our sample is worthless, turning a gap in our coverage into an accusation against
    /// most of the network.
    pub low_value_grt: f64,
    pub low_value_share: f64,
    /// What productive indexers' rewards would scale by if the low-value slice were redistributed.
    /// `null` where every measured indexer is low value, because the factor is then unbounded.
    #[serde(serialize_with = "finite_or_null")]
    pub productive_uplift_factor: f64,
    /// Allocation we hold no measurement for. A coverage figure, not a verdict.
    pub unmeasured_grt: f64,
    pub unmeasured_share: f64,
}

/// `Infinity` is what the arithmetic gives and `null` is what `JSON.stringify` writes, so a
/// consumer reading this field is written against `null`.
fn finite_or_null<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() {
        s.serialize_f64(*v)
    } else {
        s.serialize_none()
    }
}

/// Gini over non-negative values. Zero is perfect equality.
pub fn gini(values: &[f64]) -> f64 {
    let mut xs: Vec<f64> = values.iter().copied().filter(|v| *v > 0.0).collect();
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n == 0 {
        return 0.0;
    }
    let total: f64 = xs.iter().sum();
    if total == 0.0 {
        return 0.0;
    }
    let cum: f64 = xs
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64 + 1.0) * v)
        .sum();
    let n = n as f64;
    (2.0 * cum) / (n * total) - (n + 1.0) / n
}

/// The fewest top holders whose combined share exceeds `threshold`.
pub fn nakamoto(values: &[f64], threshold: f64) -> usize {
    let mut xs: Vec<f64> = values.iter().copied().filter(|v| *v > 0.0).collect();
    xs.sort_by(|a, b| b.total_cmp(a));
    let total: f64 = xs.iter().sum();
    if total == 0.0 {
        return 0;
    }
    let mut cum = 0.0;
    for (i, v) in xs.iter().enumerate() {
        cum += v;
        if cum / total > threshold {
            return i + 1;
        }
    }
    xs.len()
}

pub fn compute(rows: &[Allocation], top_n: usize) -> Concentration {
    let allocs: Vec<f64> = rows
        .iter()
        .map(|r| r.allocated_grt)
        .filter(|v| *v > 0.0)
        .collect();
    let total: f64 = allocs.iter().sum();

    let mut counts: Vec<(Tier, usize, f64)> = Vec::new();
    let mut low_value_grt = 0.0;
    let mut unmeasured_grt = 0.0;
    for row in rows.iter().filter(|r| r.allocated_grt > 0.0) {
        let tier = tier_of(row.q_score);
        match counts.iter_mut().find(|(t, _, _)| *t == tier) {
            Some(entry) => {
                entry.1 += 1;
                entry.2 += row.allocated_grt;
            }
            None => counts.push((tier, 1, row.allocated_grt)),
        }
        // Measured and poor is a finding. Unmeasured is a gap. They are counted apart.
        if matches!(tier, Tier::Zero | Tier::Low) {
            low_value_grt += row.allocated_grt;
        }
        if tier == Tier::Unscored {
            unmeasured_grt += row.allocated_grt;
        }
    }

    let share = |v: f64| if total > 0.0 { v / total } else { 0.0 };
    let tiers: Vec<TierCapture> = ORDER
        .iter()
        .filter_map(|t| {
            counts
                .iter()
                .find(|(tier, _, _)| tier == t)
                .map(|(tier, indexers, alloc)| TierCapture {
                    tier: *tier,
                    label: tier.label(),
                    indexers: *indexers,
                    alloc_grt: *alloc,
                    alloc_share: share(*alloc),
                })
        })
        .collect();

    let mut descending = allocs.clone();
    descending.sort_by(|a, b| b.total_cmp(a));
    let low_value_share = share(low_value_grt);

    Concentration {
        total_allocated_grt: total,
        indexer_count: allocs.len(),
        gini: gini(&allocs),
        nakamoto: nakamoto(&allocs, 0.5),
        top_n_share: share(descending.iter().take(top_n).sum()),
        top_n,
        tiers,
        low_value_grt,
        low_value_share,
        productive_uplift_factor: if low_value_share < 1.0 {
            1.0 / (1.0 - low_value_share)
        } else {
            f64::INFINITY
        },
        unmeasured_grt,
        unmeasured_share: share(unmeasured_grt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(grt: f64, q: Option<f64>) -> Allocation {
        Allocation {
            allocated_grt: grt,
            q_score: q,
        }
    }

    /// The claim this whole module is arranged around. An indexer we never probed is not a bad
    /// indexer, and counting it as one would turn a gap in our coverage into an accusation.
    #[test]
    fn unmeasured_allocation_is_a_gap_and_not_a_finding() {
        let c = compute(
            &[
                row(100.0, None),
                row(100.0, Some(5.0)),
                row(100.0, Some(90.0)),
            ],
            6,
        );
        assert_eq!(c.unmeasured_grt, 100.0);
        assert_eq!(c.low_value_grt, 100.0, "only the measured-and-poor one");
        assert!((c.unmeasured_share - 1.0 / 3.0).abs() < 1e-12);
        assert!((c.low_value_share - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_score_lands_in_the_band_its_thresholds_name() {
        assert_eq!(tier_of(None), Tier::Unscored);
        assert_eq!(tier_of(Some(0.0)), Tier::Zero);
        assert_eq!(tier_of(Some(14.9)), Tier::Zero);
        assert_eq!(tier_of(Some(15.0)), Tier::Low);
        assert_eq!(tier_of(Some(29.9)), Tier::Low);
        assert_eq!(tier_of(Some(30.0)), Tier::Fair);
        assert_eq!(tier_of(Some(59.9)), Tier::Fair);
        assert_eq!(tier_of(Some(60.0)), Tier::Good);
        assert_eq!(tier_of(Some(100.0)), Tier::Good);
    }

    /// `unscored` sits between the poor bands and the fair one, because it is a gap in what we
    /// know rather than a verdict at either extreme.
    #[test]
    fn the_bands_are_reported_in_their_declared_order() {
        let c = compute(
            &[
                row(1.0, Some(90.0)),
                row(1.0, None),
                row(1.0, Some(5.0)),
                row(1.0, Some(40.0)),
                row(1.0, Some(20.0)),
            ],
            6,
        );
        let order: Vec<Tier> = c.tiers.iter().map(|t| t.tier).collect();
        assert_eq!(
            order,
            vec![
                Tier::Zero,
                Tier::Low,
                Tier::Unscored,
                Tier::Fair,
                Tier::Good
            ]
        );
        // A band nobody is in does not appear at all.
        let only_good = compute(&[row(1.0, Some(90.0))], 6);
        assert_eq!(only_good.tiers.len(), 1);
    }

    #[test]
    fn gini_is_zero_when_everybody_holds_the_same() {
        assert_eq!(gini(&[10.0, 10.0, 10.0, 10.0]), 0.0);
        assert_eq!(gini(&[]), 0.0);
        assert_eq!(
            gini(&[0.0, 0.0]),
            0.0,
            "no positive values is not concentration"
        );
        // One holder of everything approaches one, and never exceeds it.
        let concentrated = gini(&[1000.0, 1.0, 1.0, 1.0, 1.0]);
        assert!(
            concentrated > 0.7 && concentrated < 1.0,
            "got {concentrated}"
        );
    }

    #[test]
    fn nakamoto_counts_the_fewest_holders_past_a_half() {
        // One holder already has more than half.
        assert_eq!(nakamoto(&[60.0, 20.0, 20.0], 0.5), 1);
        // Two are needed here, since one is exactly half and the test is strict.
        assert_eq!(nakamoto(&[50.0, 50.0], 0.5), 2);
        assert_eq!(nakamoto(&[25.0, 25.0, 25.0, 25.0], 0.5), 3);
        assert_eq!(nakamoto(&[], 0.5), 0);
    }

    /// Redistributing everything leaves nothing to scale, so the factor is unbounded, and JSON
    /// carries that as `null` rather than as a number nobody can use.
    #[test]
    fn an_unbounded_uplift_is_null_on_the_wire() {
        let all_poor = compute(&[row(100.0, Some(1.0)), row(50.0, Some(10.0))], 6);
        assert_eq!(all_poor.low_value_share, 1.0);
        assert!(all_poor.productive_uplift_factor.is_infinite());
        let json = serde_json::to_value(&all_poor).unwrap();
        assert_eq!(json["productiveUpliftFactor"], serde_json::Value::Null);

        let mixed = compute(&[row(50.0, Some(1.0)), row(50.0, Some(90.0))], 6);
        assert!((mixed.productive_uplift_factor - 2.0).abs() < 1e-12);
    }

    #[test]
    fn an_indexer_allocating_nothing_is_not_in_the_denominator() {
        let c = compute(&[row(0.0, Some(90.0)), row(100.0, Some(90.0))], 6);
        assert_eq!(c.indexer_count, 1);
        assert_eq!(c.total_allocated_grt, 100.0);
        assert_eq!(c.tiers[0].indexers, 1);
    }

    #[test]
    fn the_wire_names_are_the_incumbents() {
        let json = serde_json::to_value(compute(&[row(1.0, Some(90.0))], 6)).unwrap();
        for key in [
            "totalAllocatedGrt",
            "indexerCount",
            "gini",
            "nakamoto",
            "topNShare",
            "topN",
            "tiers",
            "lowValueGrt",
            "lowValueShare",
            "productiveUpliftFactor",
            "unmeasuredGrt",
            "unmeasuredShare",
        ] {
            assert!(json.get(key).is_some(), "Concentration is missing {key}");
        }
        for key in ["tier", "label", "indexers", "alloc_grt", "alloc_share"] {
            assert!(
                json["tiers"][0].get(key).is_some(),
                "TierCapture is missing {key}"
            );
        }
        assert_eq!(json["tiers"][0]["tier"], "good");
    }
}
