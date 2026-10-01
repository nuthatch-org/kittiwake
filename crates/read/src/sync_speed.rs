//! How fast indexers actually sync a deployment, from two looks at their status endpoints.
//!
//! The manifest score cannot see event volume or `eth_call`s per event, so Uniswap V2 on Ethereum
//! scores Light and takes weeks (lodestar#319). The chain-health job already asks fifty indexers
//! for every deployment's `latestBlock` every thirty minutes; two consecutive answers from an
//! indexer that is behind are a measured sync speed.
//!
//! Pure, like [`crate::directory`]: the job hands in the observations and stores what comes back.

use std::collections::HashMap;

use serde::Serialize;

/// Fewer blocks behind than this and the indexer is at the head for any purpose here.
pub const MIN_BEHIND: i64 = 100;

/// Two looks further apart than this measure an average over a restart or an outage, not a speed.
pub const MAX_GAP_MS: i64 = 3 * 60 * 60 * 1000;

/// A measurement older than this is not shown: the deployment's indexers have long since finished
/// or given up, and neither is what a new allocator would meet.
pub const KEPT_DAYS: i64 = 14;

/// Closer than this and a few seconds of probe skew is a large share of the interval.
pub const MIN_GAP_MS: i64 = 5 * 60 * 1000;

/// One indexer's answer about one deployment.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub indexer: String,
    pub deployment: String,
    pub network: String,
    pub latest: i64,
    pub head: i64,
    pub at_ms: i64,
}

/// A deployment's measured speed: medians over the indexers seen syncing it in one run.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Speed {
    pub deployment: String,
    pub network: String,
    pub blocks_per_hour: f64,
    /// How fast the chain itself moved over the same interval. A sync slower than this never ends.
    pub chain_blocks_per_hour: f64,
    pub indexers: usize,
    pub head: i64,
    pub measured_at_ms: i64,
}

/// The per-indexer rates for pairs seen on both looks and still syncing on the first.
///
/// An indexer that advanced at least as far as it was behind was following the head, and its
/// rate is the chain's, not a sync speed.
fn samples<'a>(
    prev: &HashMap<(String, String), Observation>,
    now: &'a [Observation],
) -> Vec<(&'a Observation, f64, f64)> {
    now.iter()
        .filter_map(|o| {
            let p = prev.get(&(o.indexer.clone(), o.deployment.clone()))?;
            let gap = o.at_ms - p.at_ms;
            if !(MIN_GAP_MS..=MAX_GAP_MS).contains(&gap) {
                return None;
            }
            let advanced = o.latest - p.latest;
            if advanced <= 0 || p.head - p.latest <= advanced {
                return None;
            }
            let hours = gap as f64 / 3_600_000.0;
            let chain = (o.head - p.head).max(0) as f64 / hours;
            Some((o, advanced as f64 / hours, chain))
        })
        .collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// One [`Speed`] per deployment that some indexer was seen syncing between `prev` and `now`.
pub fn speeds(prev: &HashMap<(String, String), Observation>, now: &[Observation]) -> Vec<Speed> {
    let mut by_dep: HashMap<&str, Vec<(&Observation, f64, f64)>> = HashMap::new();
    for s in samples(prev, now) {
        by_dep.entry(s.0.deployment.as_str()).or_default().push(s);
    }
    let mut out: Vec<Speed> = by_dep
        .into_iter()
        .map(|(dep, v)| Speed {
            deployment: dep.to_string(),
            network: v[0].0.network.clone(),
            blocks_per_hour: median(v.iter().map(|s| s.1).collect()),
            chain_blocks_per_hour: median(v.iter().map(|s| s.2).collect()),
            indexers: v.len(),
            head: v.iter().map(|s| s.0.head).max().unwrap_or(0),
            measured_at_ms: v.iter().map(|s| s.0.at_ms).max().unwrap_or(0),
        })
        .collect();
    out.sort_by(|a, b| a.deployment.cmp(&b.deployment));
    out
}

/// Days from `start_block` to the head at the measured speed, net of the chain's own. `None` when
/// the sync is no faster than the chain, which is a sync that does not finish.
pub fn days_to_sync(s: &Speed, start_block: i64) -> Option<f64> {
    let net = s.blocks_per_hour - s.chain_blocks_per_hour;
    (net > 0.0).then(|| (s.head - start_block).max(0) as f64 / net / 24.0)
}

/// The directory's label for a measured sync: a day, a week, a month.
pub fn label(days: Option<f64>) -> &'static str {
    match days {
        Some(d) if d < 1.0 => "Light",
        Some(d) if d < 7.0 => "Moderate",
        Some(d) if d < 30.0 => "Heavy",
        _ => "Extreme",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    fn obs(indexer: &str, dep: &str, latest: i64, head: i64, at_ms: i64) -> Observation {
        Observation {
            indexer: indexer.into(),
            deployment: dep.into(),
            network: "mainnet".into(),
            latest,
            head,
            at_ms,
        }
    }

    fn keyed(v: Vec<Observation>) -> HashMap<(String, String), Observation> {
        v.into_iter()
            .map(|o| ((o.indexer.clone(), o.deployment.clone()), o))
            .collect()
    }

    /// Uniswap V2 as measured on 2026-09-30: 7.9M behind, about 12,000 blocks an hour.
    #[test]
    fn a_syncing_indexer_gives_its_speed_and_the_chains() {
        let prev = keyed(vec![obs("a", "d", 15_600_000, 23_500_000, 0)]);
        let now = [obs("a", "d", 15_606_000, 23_500_150, H / 2)];
        let s = speeds(&prev, &now);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].blocks_per_hour, 12_000.0);
        assert_eq!(s[0].chain_blocks_per_hour, 300.0);
        let days = days_to_sync(&s[0], 10_000_835).unwrap();
        assert!((days - 13_499_315.0 / 11_700.0 / 24.0).abs() < 1e-6);
        assert_eq!(label(Some(days)), "Extreme");
    }

    #[test]
    fn an_indexer_following_the_head_is_not_a_sync() {
        let prev = keyed(vec![obs("a", "d", 23_499_990, 23_500_000, 0)]);
        let now = [obs("a", "d", 23_500_140, 23_500_150, H / 2)];
        assert!(speeds(&prev, &now).is_empty());
    }

    #[test]
    fn a_stalled_indexer_a_new_one_and_a_stale_look_are_not_measured() {
        let prev = keyed(vec![
            obs("stalled", "d", 1_000, 50_000, 0),
            obs("stale", "d", 1_000, 50_000, -4 * H),
            obs("quick", "d", 1_000, 50_000, H / 2 - 60_000),
        ]);
        let now = [
            obs("stalled", "d", 1_000, 50_100, H / 2),
            obs("stale", "d", 9_000, 50_100, H / 2),
            obs("quick", "d", 1_100, 50_100, H / 2),
            obs("new", "d", 5_000, 50_100, H / 2),
        ];
        assert!(speeds(&prev, &now).is_empty());
    }

    #[test]
    fn the_speed_is_the_median_over_the_indexers_syncing_it() {
        let prev = keyed(vec![
            obs("a", "d", 0, 1_000_000, 0),
            obs("b", "d", 0, 1_000_000, 0),
            obs("c", "d", 0, 1_000_000, 0),
        ]);
        let now = [
            obs("a", "d", 1_000, 1_000_000, H),
            obs("b", "d", 5_000, 1_000_000, H),
            obs("c", "d", 90_000, 1_000_000, H),
        ];
        let s = &speeds(&prev, &now)[0];
        assert_eq!((s.blocks_per_hour, s.indexers), (5_000.0, 3));
    }

    /// uniswap-v4-base-3 on 2026-09-30: 1,926 blocks an hour on a chain making 1,800.
    #[test]
    fn a_sync_no_faster_than_the_chain_never_finishes() {
        let s = Speed {
            deployment: "d".into(),
            network: "base".into(),
            blocks_per_hour: 1_800.0,
            chain_blocks_per_hour: 1_800.0,
            indexers: 1,
            head: 36_000_000,
            measured_at_ms: 0,
        };
        assert_eq!(days_to_sync(&s, 0), None);
        assert_eq!(label(None), "Extreme");
    }

    #[test]
    fn the_labels_are_a_day_a_week_and_a_month() {
        assert_eq!(label(Some(0.5)), "Light");
        assert_eq!(label(Some(3.0)), "Moderate");
        assert_eq!(label(Some(20.0)), "Heavy");
        assert_eq!(label(Some(45.0)), "Extreme");
    }
}
