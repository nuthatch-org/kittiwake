//! Is this chain still producing blocks at all?
//!
//! Every staleness signal in the stack measures distance to chain head: `blocksBehind`,
//! `syncProgress`, graph-node's own `synced` flag. All of them are relative. When a chain STOPS,
//! the head freezes, the distance to it goes to zero, and every one of those signals reports
//! perfect health - correctly, and to entirely the wrong conclusion. A subgraph on a halted chain
//! is at chain head, has no indexing errors, is attested by its indexers, and renders green
//! everywhere. It will serve the same answer for ever.
//!
//! The only signal that survives is absolute rather than relative: has the head we observe
//! actually changed over wall-clock time. That is what this module tracks. It is deliberately
//! pure - the job owns the IO, the clock and the storage - so the decision is exhaustively
//! testable without any of them.
//!
//! See nuthatch-org/graph-support#15. Moonbeam stopped producing blocks around 2026-08-10 and
//! every subgraph on it entered exactly that state; two Celo subgraphs sat behind an indexer
//! reporting itself 99.98% synced against a head that had not moved in 85 hours.

/// What we remember about one chain between runs.
///
/// `head` is the highest block height ever observed for this chain across every indexer sampled.
/// It never moves backwards: an indexer whose own RPC has stalled cannot drag the recorded head
/// down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainHeadRecord {
    /// Highest block height observed for this chain, across all sampled indexers.
    pub head: i64,
    /// When `head` first reached this value. The clock that stall is measured on. Unix millis.
    pub head_first_seen_at: i64,
    /// When we last got a reading for this chain at all. Unix millis.
    pub last_checked_at: i64,
}

/// - `Live` - the head advanced within the observation window.
/// - `Stalled` - the head has not moved for a while. Could be the chain, could be every indexer
///   serving it. From here those are indistinguishable, and for a consumer of the data they have
///   the same consequence.
/// - `Halted` - it has not moved for long enough that a transient cause is no longer plausible.
/// - `Unknown` - no record, or our own sampling has lapsed. Never claim a chain is stalled on the
///   strength of having stopped looking at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainLiveness {
    Live,
    Stalled,
    Halted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessVerdict {
    pub liveness: ChainLiveness,
    /// Head we last observed, or `None` when there is no record.
    pub head: Option<i64>,
    /// How long we WATCHED the head fail to advance. Measured between `head_first_seen_at` and
    /// `last_checked_at`, never against now - we can only assert non-advancement across the window
    /// we actually observed.
    pub stalled_for_ms: i64,
    /// Age of our own last reading. Large values are why a verdict can be `Unknown`.
    pub observation_age_ms: i64,
}

/// The job samples every thirty minutes, so a perfectly live chain can show up to ~30 minutes of
/// apparent stall purely from the sampling interval. These leave generous headroom above that:
/// three missed advances before we say anything, twelve before we call it halted. The failures
/// worth catching were 22 hours and 85 hours, so precision costs us nothing and false alarms would
/// cost the signal its credibility.
pub const STALL_AFTER_MS: i64 = 90 * 60 * 1000;
pub const HALT_AFTER_MS: i64 = 6 * 60 * 60 * 1000;

/// If our newest reading is older than this, the job has not run (deploy, outage, restart) and we
/// know nothing current. Four sampling intervals.
pub const OBSERVATION_MAX_AGE_MS: i64 = 2 * 60 * 60 * 1000;

/// Fold one observation into the record for a chain.
///
/// Only a STRICTLY higher head restarts the clock. Re-observing the same head, or a lower one from
/// a lagging indexer, leaves `head_first_seen_at` where it is, which is what accumulates the
/// stall.
///
/// Known weakness, deliberately accepted: `head` is the maximum across indexers, so a single node
/// reporting a bogus future height would pin the head above reality and the chain would then read
/// as stalled until the real head caught up. That is a false alarm rather than a false all-clear,
/// which is the direction to fail in.
pub fn advance_chain_head(
    prev: Option<ChainHeadRecord>,
    observed_head: i64,
    now: i64,
) -> ChainHeadRecord {
    if observed_head <= 0 {
        // Not a usable reading. Preserve the record untouched rather than recording a check we
        // cannot stand behind.
        return prev.unwrap_or(ChainHeadRecord {
            head: 0,
            head_first_seen_at: now,
            last_checked_at: now,
        });
    }
    match prev {
        Some(p) if observed_head <= p.head => ChainHeadRecord {
            last_checked_at: now,
            ..p
        },
        _ => ChainHeadRecord {
            head: observed_head,
            head_first_seen_at: now,
            last_checked_at: now,
        },
    }
}

/// Decide liveness from a record.
pub fn classify_liveness(record: Option<ChainHeadRecord>, now: i64) -> LivenessVerdict {
    let Some(r) = record.filter(|r| r.head > 0) else {
        return LivenessVerdict {
            liveness: ChainLiveness::Unknown,
            head: None,
            stalled_for_ms: 0,
            observation_age_ms: 0,
        };
    };

    let observation_age_ms = (now - r.last_checked_at).max(0);
    let stalled_for_ms = (r.last_checked_at - r.head_first_seen_at).max(0);

    // We stopped watching. Say so, rather than reporting whatever the last frame happened to show:
    // the entire point of this module is not mistaking absent information for a healthy reading.
    if observation_age_ms > OBSERVATION_MAX_AGE_MS {
        return LivenessVerdict {
            liveness: ChainLiveness::Unknown,
            head: Some(r.head),
            stalled_for_ms,
            observation_age_ms,
        };
    }

    let liveness = if stalled_for_ms >= HALT_AFTER_MS {
        ChainLiveness::Halted
    } else if stalled_for_ms >= STALL_AFTER_MS {
        ChainLiveness::Stalled
    } else {
        ChainLiveness::Live
    };

    LivenessVerdict {
        liveness,
        head: Some(r.head),
        stalled_for_ms,
        observation_age_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_757_000_000_000;

    fn rec(head: i64, first_seen: i64, checked: i64) -> ChainHeadRecord {
        ChainHeadRecord {
            head,
            head_first_seen_at: first_seen,
            last_checked_at: checked,
        }
    }

    #[test]
    fn a_higher_head_restarts_the_stall_clock() {
        let r = advance_chain_head(Some(rec(100, T0, T0 + 5_000)), 101, T0 + 10_000);
        assert_eq!(r.head, 101);
        assert_eq!(r.head_first_seen_at, T0 + 10_000);
    }

    /// The whole mechanism. Re-observing the same head must NOT move the clock, or nothing ever
    /// accumulates a stall and the module reports "live" for ever.
    #[test]
    fn the_same_head_again_accumulates_stall_rather_than_resetting_it() {
        let r = advance_chain_head(Some(rec(100, T0, T0 + 5_000)), 100, T0 + 10_000);
        assert_eq!(r.head_first_seen_at, T0, "the clock must not restart");
        assert_eq!(r.last_checked_at, T0 + 10_000);
    }

    /// A lagging indexer reporting an older head cannot drag the record backwards.
    #[test]
    fn a_lower_head_never_moves_the_record_down() {
        let r = advance_chain_head(Some(rec(100, T0, T0)), 40, T0 + 10_000);
        assert_eq!(r.head, 100);
        assert_eq!(r.head_first_seen_at, T0);
    }

    #[test]
    fn an_unusable_reading_records_nothing() {
        let prev = rec(100, T0, T0);
        assert_eq!(advance_chain_head(Some(prev), 0, T0 + 10_000), prev);
        assert_eq!(advance_chain_head(Some(prev), -5, T0 + 10_000), prev);
    }

    #[test]
    fn a_chain_whose_head_moves_is_live() {
        let v = classify_liveness(Some(rec(100, T0, T0 + 60_000)), T0 + 60_000);
        assert_eq!(v.liveness, ChainLiveness::Live);
    }

    #[test]
    fn ninety_minutes_without_an_advance_is_stalled_and_six_hours_is_halted() {
        let now = T0 + STALL_AFTER_MS;
        assert_eq!(
            classify_liveness(Some(rec(100, T0, now)), now).liveness,
            ChainLiveness::Stalled
        );
        let now = T0 + HALT_AFTER_MS;
        assert_eq!(
            classify_liveness(Some(rec(100, T0, now)), now).liveness,
            ChainLiveness::Halted
        );
    }

    /// The sampling interval is thirty minutes. A chain sampled twice, half an hour apart, with no
    /// advance between them must still read as live or every chain alarms on every other run.
    #[test]
    fn one_missed_advance_at_the_sampling_interval_is_not_yet_an_alarm() {
        let now = T0 + 30 * 60 * 1000;
        assert_eq!(
            classify_liveness(Some(rec(100, T0, now)), now).liveness,
            ChainLiveness::Live
        );
    }

    /// Having stopped looking is not evidence of anything. The verdict that matters most: a
    /// long-stalled record whose observation has lapsed reads `Unknown`, not `Halted`.
    #[test]
    fn a_lapsed_observation_is_unknown_and_not_halted() {
        let last_checked = T0 + HALT_AFTER_MS;
        let now = last_checked + OBSERVATION_MAX_AGE_MS + 1;
        let v = classify_liveness(Some(rec(100, T0, last_checked)), now);
        assert_eq!(v.liveness, ChainLiveness::Unknown);
        assert_eq!(v.head, Some(100), "the last head is still worth reporting");
        assert_eq!(v.stalled_for_ms, HALT_AFTER_MS);
    }

    #[test]
    fn no_record_is_unknown_rather_than_live() {
        let v = classify_liveness(None, T0);
        assert_eq!(v.liveness, ChainLiveness::Unknown);
        assert_eq!(v.head, None);
        assert_eq!(
            classify_liveness(Some(rec(0, T0, T0)), T0).liveness,
            ChainLiveness::Unknown
        );
    }

    /// Stall is measured across the window we observed, never against now: otherwise a service
    /// that was down for a day would report every chain as halted the moment it came back.
    #[test]
    fn stall_is_measured_across_the_observed_window_not_against_now() {
        let v = classify_liveness(Some(rec(100, T0, T0 + 60_000)), T0 + 60_000 + 5 * 3_600_000);
        assert_eq!(v.stalled_for_ms, 60_000);
    }

    #[test]
    fn the_serialised_names_are_the_ones_the_frontend_reads() {
        assert_eq!(
            serde_json::to_string(&ChainLiveness::Halted).unwrap(),
            "\"halted\""
        );
        assert_eq!(
            serde_json::to_string(&ChainLiveness::Unknown).unwrap(),
            "\"unknown\""
        );
    }
}
