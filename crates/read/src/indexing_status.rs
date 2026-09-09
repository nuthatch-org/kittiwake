//! Whether a deployment is actually being served, and by whom.
//!
//! This is the pure half of `/api/indexing-status/<deployment>`: everything that decides an answer,
//! with none of the fetching that gathers the evidence. Separated deliberately, because the
//! decisions are subtle and the I/O is not, and a rule that can only be exercised by standing up
//! an indexer is a rule nobody exercises.
//!
//! Four questions, in order:
//!
//! 1. **Is this indexer synced?** Not what it says it is - [`build_indexer_status`] and
//!    [`reconcile_to_network_head`].
//! 2. **Did the paid query path answer?** [`classify_serve_response`] and
//!    [`classify_paid_response`], RFC-006 D1.
//! 3. **Can anyone serve this deployment at all?** [`assess_servability`], RFC-006 D2.
//! 4. **May the page say so?** [`apply_persistence`], RFC-006 D5, which is the difference between
//!    a verdict and a panic.
//!
//! Ported from lodestar's `src/lib/indexing-status.ts`, `servability.ts` and
//! `servability-persistence.ts`. #23.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Within this many blocks of the head counts as synced.
///
/// graph-node's own `synced` flag means "caught up at some point", not "is at the head", so an
/// indexer that fell behind after its initial sync reports `synced: true` while being thousands of
/// blocks adrift. The gap is the ground truth and the flag is a footnote.
pub const SYNC_TOLERANCE_BLOCKS: i64 = 50;

/// Consecutive dead rounds before a deployment may be *rendered* dead.
pub const DEFAULT_DEAD_ROUNDS: usize = 3;

// ---------------------------------------------------------------------------------------------
// 1. Is this indexer synced?
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    Synced,
    Syncing,
    Failed,
    Unreachable,
}

impl SyncState {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncState::Synced => "synced",
            SyncState::Syncing => "syncing",
            SyncState::Failed => "failed",
            SyncState::Unreachable => "unreachable",
        }
    }
    /// A state no further evidence will move. Reconciliation recomputes only the other two.
    fn terminal(self) -> bool {
        matches!(self, SyncState::Failed | SyncState::Unreachable)
    }
}

/// One `indexingStatuses` entry as graph-node reports it, reduced to what decides a verdict.
#[derive(Debug, Clone, Default)]
pub struct RawStatus {
    pub health: Option<String>,
    pub synced: Option<bool>,
    pub chain_head_block: Option<i64>,
    pub latest_block: Option<i64>,
    pub network: Option<String>,
    pub entity_count: Option<String>,
    pub fatal_error: Option<String>,
    pub non_fatal_errors: Vec<String>,
}

/// What we know about one indexer's copy of a deployment.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexerStatus {
    pub indexer_id: String,
    pub indexer_name: Option<String>,
    pub url: String,
    /// Allocated stake in wei, as a decimal string. Kept as text because it does not fit an `f64`
    /// and the fragility share below is computed in integers.
    pub allocated_tokens: String,
    pub state: SyncState,
    pub health: Option<String>,
    pub synced: Option<bool>,
    pub chain_head_block: Option<i64>,
    pub latest_block: Option<i64>,
    pub network: Option<String>,
    pub entity_count: Option<String>,
    pub fatal_error: Option<String>,
    pub non_fatal_errors: Vec<String>,
    pub non_fatal_error_count: usize,
    pub sync_progress: Option<f64>,
    pub blocks_behind: Option<i64>,
    /// The head this indexer's lag was measured against, once reconciled against its peers.
    pub network_chain_head: Option<i64>,
    pub serve_probe: Option<ServeProbe>,
}

impl IndexerStatus {
    /// An indexer with no `/status` answer at all. Not a failure of the deployment: a failure to
    /// ask, which is a different thing and is counted separately.
    pub fn unreachable(
        indexer_id: impl Into<String>,
        indexer_name: Option<String>,
        url: impl Into<String>,
        allocated_tokens: impl Into<String>,
    ) -> Self {
        Self {
            indexer_id: indexer_id.into(),
            indexer_name,
            url: url.into(),
            allocated_tokens: allocated_tokens.into(),
            state: SyncState::Unreachable,
            health: None,
            synced: None,
            chain_head_block: None,
            latest_block: None,
            network: None,
            entity_count: None,
            fatal_error: None,
            non_fatal_errors: Vec::new(),
            non_fatal_error_count: 0,
            sync_progress: None,
            blocks_behind: None,
            network_chain_head: None,
            serve_probe: None,
        }
    }

    /// RFC-006 D1: only a confirmed paid answer counts as servable.
    pub fn servable(&self) -> bool {
        matches!(
            self.serve_probe,
            Some(ServeProbe::Serving) | Some(ServeProbe::AlivePaid)
        )
    }

    /// The tri-state the page colours by. `failed` outranks the sync gap.
    pub fn health_class(&self) -> &'static str {
        match self.state {
            SyncState::Failed => "failed",
            SyncState::Synced => "healthy",
            _ => "unhealthy",
        }
    }
}

/// Fold one indexer's `/status` answer into a verdict.
pub fn build_indexer_status(
    indexer_id: impl Into<String>,
    indexer_name: Option<String>,
    url: impl Into<String>,
    allocated_tokens: impl Into<String>,
    raw: Option<&RawStatus>,
) -> IndexerStatus {
    let mut out = IndexerStatus::unreachable(indexer_id, indexer_name, url, allocated_tokens);
    let Some(s) = raw else { return out };

    let head = s.chain_head_block.filter(|n| *n > 0);
    let latest = s.latest_block;

    if let (Some(head), Some(latest)) = (head, latest) {
        out.sync_progress = Some(((latest as f64 / head as f64) * 100.0).min(100.0));
        out.blocks_behind = Some((head - latest).max(0));
    }

    // The gap, not the flag. See `SYNC_TOLERANCE_BLOCKS`. This is still the indexer's *self*
    // reported gap; `reconcile_to_network_head` corrects it against its peers.
    let effectively_synced = match out.blocks_behind {
        Some(b) => b <= SYNC_TOLERANCE_BLOCKS,
        None => s.synced.unwrap_or(false),
    };

    out.state = if s.health.as_deref() == Some("failed") || s.fatal_error.is_some() {
        SyncState::Failed
    } else if effectively_synced {
        SyncState::Synced
    } else {
        SyncState::Syncing
    };

    out.health = s.health.clone();
    out.synced = s.synced;
    out.chain_head_block = s.chain_head_block;
    out.latest_block = s.latest_block;
    out.network = s.network.clone();
    out.entity_count = s.entity_count.clone();
    out.fatal_error = s.fatal_error.clone();
    out.non_fatal_error_count = s.non_fatal_errors.len();
    // The last five, because the panel shows a handful and a subgraph in a bad loop can carry
    // thousands. The count above stays honest about how many there were.
    out.non_fatal_errors = s
        .non_fatal_errors
        .iter()
        .rev()
        .take(5)
        .rev()
        .cloned()
        .collect();
    out
}

/// Recompute every indexer's lag against the freshest head any of them reports.
///
/// An indexer whose own firehose has stalled reports `chainHeadBlock` ≈ `latestBlock`, so its
/// self-diff is nearly zero and it reads as caught up while being tens of thousands of blocks
/// behind the real chain. Measuring against the best-informed peer exposes exactly that.
///
/// The peer that *defines* the head keeps its own self-diff, so a genuinely fresh indexer is never
/// made to look worse. Terminal states are preserved: this recomputes the synced/syncing
/// distinction and nothing else.
pub fn reconcile_to_network_head(mut indexers: Vec<IndexerStatus>) -> Vec<IndexerStatus> {
    let head = indexers
        .iter()
        .flat_map(|i| [i.chain_head_block, i.latest_block])
        .flatten()
        .max()
        .unwrap_or(0);
    if head <= 0 {
        return indexers;
    }

    for i in indexers.iter_mut() {
        let Some(latest) = i.latest_block else {
            continue; // no chain data; there is nothing to reconcile
        };
        let behind = (head - latest).max(0);
        i.blocks_behind = Some(behind);
        i.sync_progress = Some(((latest as f64 / head as f64) * 100.0).min(100.0));
        i.network_chain_head = Some(head);
        if !i.state.terminal() {
            i.state = if behind <= SYNC_TOLERANCE_BLOCKS {
                SyncState::Synced
            } else {
                SyncState::Syncing
            };
        }
    }
    indexers
}

// ---------------------------------------------------------------------------------------------
// 2. Did the paid query path answer?
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeProbe {
    /// A paid query came back with data. The strongest reading available.
    Serving,
    /// The serving stack answered, but not with attested data: alive, unverified.
    AlivePaid,
    /// A response arrived and it was not the serving path, or none arrived at all.
    Broken,
    /// Never asked: the URL was refused before a request was made.
    Unreachable,
}

/// The shape a healthy-but-unpaid indexer-service returns when it wants a receipt.
///
/// Deliberately broad, because indexer-service-rs phrases it differently across versions and the
/// cost of the two errors is not symmetric: reading "needs payment" as `broken` calls a working
/// deployment dead, while reading a genuine error as `alive_paid` merely fails to raise an alarm.
fn payment_shape() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"receipt|payment\s*(is\s*)?required|requires?\s*payment|x-payment|\btap\b|paid\s*quer|free\s*quer|insufficient|unpaid|402",
        )
        .case_insensitive(true)
        .build()
        .expect("payment shape is a constant")
    })
}

/// Our receipt was rejected, which says nothing about whether the deployment serves.
fn payment_reject() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"no\s*tap\s*receipt|receipt\s*was\s*(not\s*)?found|escrow|insufficient\s*(escrow|funds|balance)|receipt.*(invalid|expired|missing|reject)|payment\s*required",
        )
        .case_insensitive(true)
        .build()
        .expect("payment reject is a constant")
    })
}

fn looks_html(content_type: &str, body: &str) -> bool {
    body.trim_start().starts_with('<') || content_type.to_ascii_lowercase().contains("text/html")
}

/// Classify a **receipt-less** probe. Never returns `Unreachable`: that is decided before a
/// response exists.
pub fn classify_serve_response(status: u16, content_type: &str, body: &str) -> ServeProbe {
    // The canonical "serving stack up, wants a receipt".
    if status == 402 {
        return ServeProbe::AlivePaid;
    }
    let html = looks_html(content_type, body);

    if (200..300).contains(&status) {
        // JSON - data or GraphQL errors alike - means the query stack answered. HTML on a 200 is a
        // proxy or a landing page sitting where the serving path should be.
        if html {
            return ServeProbe::Broken;
        }
        return match serde_json::from_str::<serde_json::Value>(body) {
            Ok(_) => ServeProbe::AlivePaid,
            Err(_) => ServeProbe::Broken,
        };
    }

    // Past 4xx only the specific payment shape counts as alive. A bare 400, a 404, a 5xx or an
    // HTML error page is a broken serving path.
    if (400..500).contains(&status) && !html && payment_shape().is_match(body) {
        return ServeProbe::AlivePaid;
    }
    ServeProbe::Broken
}

/// What a paid probe learned, before it is folded back into a [`ServeProbe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaidOutcome {
    /// 2xx JSON with `data` and no errors. Confirmed.
    Serving,
    /// Our receipt or escrow was rejected. Inconclusive: the gate is up and we could not pay.
    PaymentUnfunded,
    /// An error past the gate. The deployment answered and the answer was not data.
    Broken,
}

/// Classify a **receipt-bearing** probe.
pub fn classify_paid_response(status: u16, content_type: &str, body: &str) -> PaidOutcome {
    if status == 402 || payment_reject().is_match(body) {
        return PaidOutcome::PaymentUnfunded;
    }
    if (200..300).contains(&status) && !looks_html(content_type, body) {
        let Ok(j) = serde_json::from_str::<serde_json::Value>(body) else {
            return PaidOutcome::Broken;
        };
        let has_errors = j
            .get("errors")
            .and_then(|e| e.as_array())
            .is_some_and(|a| !a.is_empty());
        let has_data = j.get("data").is_some_and(|d| !d.is_null());
        if has_data && !has_errors {
            return PaidOutcome::Serving;
        }
        return PaidOutcome::Broken;
    }
    PaidOutcome::Broken
}

/// Fold a paid outcome into the single verdict the rest of the pipeline reads.
///
/// `PaymentUnfunded` becomes `AlivePaid`: the gate is up and we simply could not pay, which is
/// exactly "alive but unverified" and must never read as broken.
pub fn probe_from_paid(outcome: PaidOutcome) -> ServeProbe {
    match outcome {
        PaidOutcome::Serving => ServeProbe::Serving,
        PaidOutcome::PaymentUnfunded => ServeProbe::AlivePaid,
        PaidOutcome::Broken => ServeProbe::Broken,
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Can anyone serve this deployment?
// ---------------------------------------------------------------------------------------------

/// RFC-006 D2, the instantaneous read over one round of probes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Servability {
    /// Distinct operators with at least one servable indexer.
    pub effective_serving_operators: usize,
    pub serving_indexer_count: usize,
    /// Nobody can serve. Queries fail whatever the sync figures say.
    pub effectively_dead: bool,
    /// Dead now, but something is catching up.
    pub recovering: bool,
    /// The largest operator's share of allocated stake, 0 to 1.
    ///
    /// A fragility warning and **never** a reason to call anything dead. Ten indexers behind one
    /// operator look like headroom to the gateway and share one fate in practice.
    pub dominant_operator_share: f64,
}

fn wei(s: &str) -> u128 {
    s.split('.').next().unwrap_or("0").parse().unwrap_or(0)
}

/// Turn per-indexer probes into a deployment-level verdict.
///
/// Keyed on **service alone**: clustering never decides "dead" (RFC-006 §0/§1). `operator_of` only
/// refines the operator count and the fragility share, and defaults to treating each indexer as
/// its own operator.
pub fn assess_servability(
    indexers: &[IndexerStatus],
    operator_of: Option<&dyn Fn(&str) -> String>,
) -> Servability {
    let key = |id: &str| match operator_of {
        Some(f) => f(id),
        None => id.to_string(),
    };

    let serving: Vec<&IndexerStatus> = indexers.iter().filter(|i| i.servable()).collect();
    let serving_operators: HashSet<String> = serving.iter().map(|i| key(&i.indexer_id)).collect();
    let effectively_dead = serving_operators.is_empty();

    let mut by_operator: HashMap<String, u128> = HashMap::new();
    let mut total: u128 = 0;
    for i in indexers {
        let s = wei(&i.allocated_tokens);
        *by_operator.entry(key(&i.indexer_id)).or_default() += s;
        total += s;
    }
    // In integers to four places, then divided. A wei total overflows an `f64`'s exact range long
    // before it overflows a `u128`, and the share is the one number here that must not drift.
    let dominant = by_operator.values().copied().max().unwrap_or(0);
    let dominant_operator_share = dominant
        .saturating_mul(10_000)
        .checked_div(total)
        .map_or(0.0, |n| n as f64 / 10_000.0);

    Servability {
        effective_serving_operators: serving_operators.len(),
        serving_indexer_count: serving.len(),
        effectively_dead,
        recovering: effectively_dead && indexers.iter().any(|i| i.state == SyncState::Syncing),
        dominant_operator_share,
    }
}

// ---------------------------------------------------------------------------------------------
// 4. May the page say so?
// ---------------------------------------------------------------------------------------------

/// What the gateway said in a round.
///
/// The gateway is no longer asked - the probe left with the key - so new rounds record `None`.
/// The type outlives the probe because persisted rounds still carry their verdict and the rule
/// below reads them as it always did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayVerdict {
    Served,
    BadIndexers,
    NoIndexers,
    NotFound,
    Error,
}

/// One probe round, as persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct Round {
    pub probed_at: String,
    pub serving_operators: usize,
    pub serving_indexers: usize,
    pub gateway_verdict: Option<GatewayVerdict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderedState {
    /// Somebody served this round.
    Ok,
    /// Dead this round, but not for long enough to say so.
    Rechecking,
    /// Dead by our probes and served by the gateway in the same round.
    Conflicting,
    /// K consecutive dead rounds, none of them served by the gateway.
    Dead,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedServability {
    pub state: RenderedState,
    /// What the banner may say. True only for [`RenderedState::Dead`].
    pub effectively_dead: bool,
    pub dead_streak: usize,
    pub k: usize,
    /// When the newest round was probed, so a cached verdict is visibly a snapshot.
    pub probed_at: Option<String>,
}

/// K, floored at two. A first-ever dead round is never rendered dead.
pub fn dead_rounds_threshold(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n >= 2)
        .unwrap_or(DEFAULT_DEAD_ROUNDS)
}

/// RFC-006 D5: turn a short history of rounds into what the page may actually render.
///
/// Two rules, and both exist because of a real morning. One timed-out probe against a
/// one-indexer deployment produced the scariest label on the page and cached it for three minutes.
///
/// - **Death needs persistence**, K consecutive dead rounds. Recovery is instant, because one
///   serving round is proof of life and there is nothing to be cautious about.
/// - **The gateway is the stronger witness.** A round it served can never be dead, whatever our
///   own probes saw, and it resets the streak. Our probes failing while the gateway serves usually
///   means our egress or our guard is the problem rather than the network.
///
/// `history` runs oldest to newest, the last entry being the round just probed.
pub fn apply_persistence(history: &[Round], k: usize) -> RenderedServability {
    let threshold = k.max(2);
    let Some(current) = history.last() else {
        return RenderedServability {
            state: RenderedState::Ok,
            effectively_dead: false,
            dead_streak: 0,
            k: threshold,
            probed_at: None,
        };
    };
    let dead = |r: &Round| r.serving_operators == 0;
    let gateway_served = |r: &Round| r.gateway_verdict == Some(GatewayVerdict::Served);

    let at = Some(current.probed_at.clone());
    if !dead(current) {
        return RenderedServability {
            state: RenderedState::Ok,
            effectively_dead: false,
            dead_streak: 0,
            k: threshold,
            probed_at: at,
        };
    }
    if gateway_served(current) {
        return RenderedServability {
            state: RenderedState::Conflicting,
            effectively_dead: false,
            dead_streak: 0,
            k: threshold,
            probed_at: at,
        };
    }

    let streak = history
        .iter()
        .rev()
        .take_while(|r| dead(r) && !gateway_served(r))
        .count();

    RenderedServability {
        state: if streak >= threshold {
            RenderedState::Dead
        } else {
            RenderedState::Rechecking
        },
        effectively_dead: streak >= threshold,
        dead_streak: streak,
        k: threshold,
        probed_at: at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(health: &str, synced: bool, head: i64, latest: i64) -> RawStatus {
        RawStatus {
            health: Some(health.into()),
            synced: Some(synced),
            chain_head_block: Some(head),
            latest_block: Some(latest),
            ..Default::default()
        }
    }

    fn at(id: &str, state: SyncState, probe: Option<ServeProbe>, stake: &str) -> IndexerStatus {
        let mut s = IndexerStatus::unreachable(id, None, "http://x", stake);
        s.state = state;
        s.serve_probe = probe;
        s
    }

    // -- 1. sync ------------------------------------------------------------------------------

    /// The whole reason the gap is used instead of the flag. graph-node says "synced" meaning
    /// "caught up at some point"; this indexer fell behind afterwards and is 5,000 blocks adrift.
    #[test]
    fn an_indexer_claiming_synced_while_thousands_behind_is_syncing() {
        let s = build_indexer_status(
            "0xa",
            None,
            "u",
            "0",
            Some(&raw("healthy", true, 105_000, 100_000)),
        );
        assert_eq!(s.state, SyncState::Syncing);
        assert_eq!(s.blocks_behind, Some(5_000));
    }

    #[test]
    fn within_the_tolerance_is_synced_and_one_block_past_it_is_not() {
        let inside = build_indexer_status(
            "0xa",
            None,
            "u",
            "0",
            Some(&raw("healthy", false, 1_000, 1_000 - SYNC_TOLERANCE_BLOCKS)),
        );
        assert_eq!(inside.state, SyncState::Synced);
        let outside = build_indexer_status(
            "0xa",
            None,
            "u",
            "0",
            Some(&raw("healthy", false, 1_000, 999 - SYNC_TOLERANCE_BLOCKS)),
        );
        assert_eq!(outside.state, SyncState::Syncing);
    }

    /// A fatal error outranks the gap. An indexer perfectly at the head with a failed subgraph is
    /// serving nothing.
    #[test]
    fn a_fatal_error_beats_being_at_the_head() {
        let mut r = raw("healthy", true, 100, 100);
        r.fatal_error = Some("handler panicked".into());
        assert_eq!(
            build_indexer_status("0xa", None, "u", "0", Some(&r)).state,
            SyncState::Failed
        );
    }

    #[test]
    fn no_answer_at_all_is_unreachable_rather_than_failed() {
        let s = build_indexer_status("0xa", None, "u", "0", None);
        assert_eq!(s.state, SyncState::Unreachable);
        assert!(s.blocks_behind.is_none());
    }

    /// With no block numbers there is no gap to measure, so the flag is all there is.
    #[test]
    fn without_block_numbers_the_flag_is_the_only_evidence() {
        let s = RawStatus {
            health: Some("healthy".into()),
            synced: Some(true),
            ..Default::default()
        };
        assert_eq!(
            build_indexer_status("0xa", None, "u", "0", Some(&s)).state,
            SyncState::Synced
        );
    }

    /// The panel shows a handful; a subgraph in a bad loop can carry thousands. The count stays
    /// honest about how many there were.
    #[test]
    fn only_the_last_five_non_fatal_errors_are_kept_and_the_count_is_not_truncated() {
        let s = RawStatus {
            health: Some("healthy".into()),
            non_fatal_errors: (1..=9).map(|n| format!("e{n}")).collect(),
            ..Default::default()
        };
        let out = build_indexer_status("0xa", None, "u", "0", Some(&s));
        assert_eq!(out.non_fatal_errors, ["e5", "e6", "e7", "e8", "e9"]);
        assert_eq!(out.non_fatal_error_count, 9);
    }

    /// The defect this exists for: a stalled firehose reports head ≈ latest, so the indexer's own
    /// arithmetic says it is caught up. Against its peers it is 50,000 blocks behind.
    #[test]
    fn an_indexer_whose_own_head_has_stalled_is_caught_by_its_peers() {
        let stalled = build_indexer_status(
            "0xa",
            None,
            "u",
            "0",
            Some(&raw("healthy", true, 50_000, 50_000)),
        );
        assert_eq!(stalled.state, SyncState::Synced, "it says so itself");
        let fresh = build_indexer_status(
            "0xb",
            None,
            "u",
            "0",
            Some(&raw("healthy", true, 100_000, 100_000)),
        );

        let out = reconcile_to_network_head(vec![stalled, fresh]);
        assert_eq!(out[0].state, SyncState::Syncing);
        assert_eq!(out[0].blocks_behind, Some(50_000));
        assert_eq!(out[0].network_chain_head, Some(100_000));
        // The peer that defines the head must not be made to look worse.
        assert_eq!(out[1].state, SyncState::Synced);
        assert_eq!(out[1].blocks_behind, Some(0));
    }

    #[test]
    fn reconciliation_does_not_revive_a_failed_or_unreachable_indexer() {
        let mut failed =
            build_indexer_status("0xa", None, "u", "0", Some(&raw("failed", true, 100, 100)));
        failed.state = SyncState::Failed;
        let fresh =
            build_indexer_status("0xb", None, "u", "0", Some(&raw("healthy", true, 100, 100)));
        let out = reconcile_to_network_head(vec![failed, fresh]);
        assert_eq!(out[0].state, SyncState::Failed);
    }

    #[test]
    fn reconciliation_is_order_independent_and_a_no_op_without_any_chain_data() {
        let a = build_indexer_status(
            "0xa",
            None,
            "u",
            "0",
            Some(&raw("healthy", true, 90_000, 90_000)),
        );
        let b = build_indexer_status(
            "0xb",
            None,
            "u",
            "0",
            Some(&raw("healthy", true, 100_000, 100_000)),
        );
        let one = reconcile_to_network_head(vec![a.clone(), b.clone()]);
        let two = reconcile_to_network_head(vec![b, a]);
        assert_eq!(one[0].blocks_behind, two[1].blocks_behind);

        let blind = vec![IndexerStatus::unreachable("0xc", None, "u", "0")];
        assert_eq!(reconcile_to_network_head(blind.clone()), blind);
    }

    // -- 2. the serving probe -----------------------------------------------------------------

    #[test]
    fn a_402_is_the_serving_stack_asking_to_be_paid() {
        assert_eq!(
            classify_serve_response(402, "application/json", ""),
            ServeProbe::AlivePaid
        );
    }

    /// A 200 of HTML is a proxy or a landing page sitting where the serving path should be, and it
    /// is the failure most likely to be mistaken for health.
    #[test]
    fn html_on_a_200_is_broken_and_json_is_not() {
        assert_eq!(
            classify_serve_response(200, "text/html", "<!doctype html>"),
            ServeProbe::Broken
        );
        assert_eq!(
            classify_serve_response(200, "application/json", "<html>"),
            ServeProbe::Broken,
            "the body decides even when the header does not"
        );
        assert_eq!(
            classify_serve_response(200, "application/json", r#"{"errors":[{"message":"x"}]}"#),
            ServeProbe::AlivePaid,
            "GraphQL errors still mean the query stack answered"
        );
        assert_eq!(
            classify_serve_response(200, "application/json", "not json"),
            ServeProbe::Broken
        );
    }

    #[test]
    fn only_a_payment_shaped_4xx_reads_as_alive() {
        assert_eq!(
            classify_serve_response(400, "application/json", "no tap receipt attached"),
            ServeProbe::AlivePaid
        );
        assert_eq!(
            classify_serve_response(400, "application/json", "bad request"),
            ServeProbe::Broken
        );
        assert_eq!(
            classify_serve_response(404, "application/json", "receipt"),
            ServeProbe::AlivePaid
        );
        assert_eq!(
            classify_serve_response(503, "application/json", "receipt"),
            ServeProbe::Broken,
            "a 5xx is a broken serving path whatever the body says"
        );
        assert_eq!(
            classify_serve_response(400, "text/html", "<p>payment required</p>"),
            ServeProbe::Broken,
            "an HTML error page is an edge, not an indexer"
        );
    }

    // -- the paid classifier ------------------------------------------------------------------

    #[test]
    fn a_paid_query_returning_data_is_the_only_confirmed_reading() {
        assert_eq!(
            classify_paid_response(
                200,
                "application/json",
                r#"{"data":{"_meta":{"block":{"number":1}}}}"#
            ),
            PaidOutcome::Serving
        );
    }

    /// The iExec mode: past the payment gate and still not answering. This is the failure the paid
    /// probe exists to see, and a receipt-less probe would have called it alive.
    #[test]
    fn an_error_past_the_gate_is_broken_rather_than_alive() {
        assert_eq!(
            classify_paid_response(
                200,
                "application/json",
                r#"{"data":null,"errors":[{"message":"BadResponse"}]}"#
            ),
            PaidOutcome::Broken
        );
        assert_eq!(
            classify_paid_response(
                200,
                "application/json",
                r#"{"data":{"x":1},"errors":[{"message":"partial"}]}"#
            ),
            PaidOutcome::Broken,
            "data beside errors is not a clean answer"
        );
    }

    /// Our receipt being refused says nothing about the deployment, so it must not read as broken.
    #[test]
    fn a_rejected_receipt_is_inconclusive_and_folds_back_to_alive() {
        for body in [
            "no tap receipt",
            "insufficient escrow balance",
            "receipt was not found",
            "the receipt is invalid",
        ] {
            assert_eq!(
                classify_paid_response(400, "application/json", body),
                PaidOutcome::PaymentUnfunded,
                "{body}"
            );
        }
        assert_eq!(
            probe_from_paid(PaidOutcome::PaymentUnfunded),
            ServeProbe::AlivePaid
        );
    }

    // -- 3. servability -----------------------------------------------------------------------

    #[test]
    fn one_serving_indexer_is_enough_for_the_deployment_not_to_be_dead() {
        let v = assess_servability(
            &[
                at("0xa", SyncState::Synced, Some(ServeProbe::Serving), "100"),
                at("0xb", SyncState::Failed, Some(ServeProbe::Broken), "100"),
            ],
            None,
        );
        assert!(!v.effectively_dead);
        assert_eq!(v.serving_indexer_count, 1);
        assert_eq!(v.effective_serving_operators, 1);
    }

    #[test]
    fn nobody_serving_is_dead_and_a_syncing_peer_makes_it_recovering() {
        let dead = assess_servability(
            &[at("0xa", SyncState::Failed, Some(ServeProbe::Broken), "1")],
            None,
        );
        assert!(dead.effectively_dead && !dead.recovering);

        let rescue = assess_servability(
            &[
                at("0xa", SyncState::Failed, Some(ServeProbe::Broken), "1"),
                at("0xb", SyncState::Syncing, Some(ServeProbe::Broken), "1"),
            ],
            None,
        );
        assert!(rescue.effectively_dead && rescue.recovering);
    }

    /// Clustering refines the operator count and the fragility share. It must never be able to
    /// turn a serving deployment into a dead one, which is RFC-006 §0.
    #[test]
    fn collapsing_identities_changes_the_operator_count_and_never_the_verdict() {
        let indexers = [
            at("0xa", SyncState::Synced, Some(ServeProbe::Serving), "50"),
            at("0xb", SyncState::Synced, Some(ServeProbe::Serving), "50"),
        ];
        let apart = assess_servability(&indexers, None);
        let together = assess_servability(&indexers, Some(&|_id: &str| "one".to_string()));

        assert_eq!(apart.effective_serving_operators, 2);
        assert_eq!(together.effective_serving_operators, 1);
        assert!(!apart.effectively_dead && !together.effectively_dead);
        assert_eq!(apart.dominant_operator_share, 0.5);
        assert_eq!(together.dominant_operator_share, 1.0);
    }

    /// Wei totals well past what an `f64` holds exactly. The share is the one figure here that
    /// must not drift, so it is computed in integers.
    #[test]
    fn the_fragility_share_is_exact_at_wei_scale() {
        let v = assess_servability(
            &[
                at("0xa", SyncState::Synced, None, "750000000000000000000000"),
                at("0xb", SyncState::Synced, None, "250000000000000000000000"),
            ],
            None,
        );
        assert_eq!(v.dominant_operator_share, 0.75);
    }

    #[test]
    fn no_stake_anywhere_is_a_share_of_zero_rather_than_a_division_by_it() {
        let v = assess_servability(&[at("0xa", SyncState::Synced, None, "0")], None);
        assert_eq!(v.dominant_operator_share, 0.0);
    }

    // -- 4. persistence -----------------------------------------------------------------------

    fn round(serving: usize, gateway: Option<GatewayVerdict>) -> Round {
        Round {
            probed_at: "2026-09-09T00:00:00Z".into(),
            serving_operators: serving,
            serving_indexers: serving,
            gateway_verdict: gateway,
        }
    }

    /// The morning this rule exists for: one timed-out probe against a one-indexer deployment put
    /// the scariest label on the page and cached it.
    #[test]
    fn a_single_dead_round_is_rechecking_and_never_dead() {
        let r = apply_persistence(&[round(0, None)], 3);
        assert_eq!(r.state, RenderedState::Rechecking);
        assert!(!r.effectively_dead);
        assert_eq!(r.dead_streak, 1);
    }

    #[test]
    fn k_consecutive_dead_rounds_are_dead_and_one_fewer_is_not() {
        let three = vec![round(0, None), round(0, None), round(0, None)];
        assert_eq!(apply_persistence(&three, 3).state, RenderedState::Dead);
        assert!(apply_persistence(&three, 3).effectively_dead);
        assert_eq!(
            apply_persistence(&three[..2], 3).state,
            RenderedState::Rechecking
        );
    }

    /// Recovery is instant: one serving round is proof of life and there is nothing to be cautious
    /// about in the good direction.
    #[test]
    fn one_serving_round_clears_any_streak_immediately() {
        let h = vec![
            round(0, None),
            round(0, None),
            round(0, None),
            round(1, None),
        ];
        let r = apply_persistence(&h, 3);
        assert_eq!(r.state, RenderedState::Ok);
        assert_eq!(r.dead_streak, 0);
    }

    /// The gateway is the stronger witness. Our probes failing while it serves usually means our
    /// egress or our guard is the problem rather than the network.
    #[test]
    fn a_gateway_served_round_is_conflicting_and_resets_the_streak() {
        let now = apply_persistence(&[round(0, None), round(0, Some(GatewayVerdict::Served))], 2);
        assert_eq!(now.state, RenderedState::Conflicting);
        assert!(!now.effectively_dead);

        // And an older gateway-served round ends the streak counted back from the newest.
        let h = vec![
            round(0, None),
            round(0, Some(GatewayVerdict::Served)),
            round(0, None),
            round(0, None),
        ];
        let r = apply_persistence(&h, 3);
        assert_eq!(r.dead_streak, 2);
        assert_eq!(r.state, RenderedState::Rechecking);
    }

    /// A gateway verdict that is not `served` is not a witness to anything.
    #[test]
    fn a_gateway_that_also_failed_does_not_rescue_the_round() {
        let h = vec![
            round(0, Some(GatewayVerdict::BadIndexers)),
            round(0, Some(GatewayVerdict::NoIndexers)),
        ];
        assert_eq!(apply_persistence(&h, 2).state, RenderedState::Dead);
    }

    #[test]
    fn an_empty_history_says_nothing_rather_than_something_frightening() {
        let r = apply_persistence(&[], 3);
        assert_eq!(r.state, RenderedState::Ok);
        assert!(r.probed_at.is_none());
    }

    /// K below two would render the first dead round dead, which is the whole thing this prevents.
    #[test]
    fn the_threshold_is_floored_at_two_however_it_is_configured() {
        assert_eq!(apply_persistence(&[round(0, None)], 1).k, 2);
        assert_eq!(apply_persistence(&[round(0, None)], 0).k, 2);
        assert_eq!(dead_rounds_threshold(Some("1")), DEFAULT_DEAD_ROUNDS);
        assert_eq!(dead_rounds_threshold(Some("nonsense")), DEFAULT_DEAD_ROUNDS);
        assert_eq!(dead_rounds_threshold(None), DEFAULT_DEAD_ROUNDS);
        assert_eq!(dead_rounds_threshold(Some("5")), 5);
    }
}
