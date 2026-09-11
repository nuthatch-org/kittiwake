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

/// A fatal error, as graph-node reports it and not merely its message.
///
/// `deterministic` is the field that matters and the one a message cannot carry: a deterministic
/// failure means this subgraph is broken at this block for everyone and will stay broken, while a
/// non-deterministic one means an indexer's host had a bad minute. `block` says where to look.
/// Reporting only the message collapses those into one sentence a reader has to guess about.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FatalError {
    pub message: String,
    /// The mapping handler that raised it, when graph-node names one.
    pub handler: Option<String>,
    pub block: Option<ErrorBlock>,
    pub deterministic: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorBlock {
    pub number: Option<i64>,
    pub hash: Option<String>,
}

impl FatalError {
    /// A fatal error known only by its message, which is all some graph-node versions give.
    pub fn message(m: impl Into<String>) -> Self {
        Self {
            message: m.into(),
            handler: None,
            block: None,
            deterministic: None,
        }
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
    pub fatal_error: Option<FatalError>,
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
    pub fatal_error: Option<FatalError>,
    pub non_fatal_errors: Vec<String>,
    pub non_fatal_error_count: usize,
    pub sync_progress: Option<f64>,
    pub blocks_behind: Option<i64>,
    /// The head this indexer's lag was measured against, once reconciled against its peers.
    pub network_chain_head: Option<i64>,
    /// Whether graph-node answered at all. Decides which keys the payload carries: an indexer that
    /// said nothing gets the five fields we knew before asking, and not a row of nulls.
    pub answered: bool,
    pub serve_probe: Option<ServeProbe>,
    /// How that verdict was reached. A transport failure that never got a response and an edge
    /// answering 403 HTML both collapse to `broken`, and they point at different culprits.
    pub serve_probe_detail: Option<ProbeDetail>,
}

/// The evidence behind one `serve_probe`, as the panel receives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeDetail {
    pub probe: ServeProbe,
    /// `guard` - refused before a request was made; `transport` - no response arrived;
    /// `response` - one arrived and was classified.
    pub cause: &'static str,
    pub error: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    /// Whether a receipt was attached, which decides which classifier read the response.
    pub paid: bool,
    pub attempts: u32,
    pub elapsed_ms: u64,
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
            answered: false,
            serve_probe: None,
            serve_probe_detail: None,
        }
    }

    /// RFC-006 D1: only a confirmed paid answer counts as servable.
    pub fn servable(&self) -> bool {
        matches!(
            self.serve_probe,
            Some(ServeProbe::Serving) | Some(ServeProbe::AlivePaid)
        )
    }

    /// graph-node's own `health`, passed through rather than derived.
    ///
    /// It is **not** a restatement of the sync state and the two disagree routinely: the incumbent
    /// serves `status: "syncing"` beside `health: "healthy"` for an indexer that is simply behind.
    /// Deriving this from the gap - which the first version of this module did - would have moved
    /// every catching-up indexer out of `healthyCount` and changed a number the panel shows.
    pub fn reported_health(&self) -> Option<&str> {
        self.health.as_deref()
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
    out.answered = true;

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
        r.fatal_error = Some(FatalError::message("handler panicked"));
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

// ---------------------------------------------------------------------------------------------
// Gathering the evidence
// ---------------------------------------------------------------------------------------------

/// How many indexers are probed at once.
///
/// Each one is two requests to a stranger's endpoint, and the slow ones are the point rather than
/// the exception: an indexer that takes five seconds to say nothing is exactly the reading wanted.
/// Eight keeps a hundred-allocation deployment inside a sensible wall clock without opening a
/// hundred sockets to the outside world at once.
pub const PROBE_CONCURRENCY: usize = 8;

/// How long one probe may take before it is called a timeout.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One allocation, as the nest reports it.
#[derive(Debug, Clone)]
pub struct Allocated {
    pub indexer_id: String,
    pub indexer_name: Option<String>,
    pub url: Option<String>,
    pub allocated_tokens: String,
}

/// Parse graph-node's `indexingStatuses[0]` into the fields that decide a verdict.
pub fn parse_raw_status(v: &serde_json::Value) -> RawStatus {
    let chain = v
        .get("chains")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());
    // graph-node sends block numbers as strings, and has been known to send them as numbers.
    // Reading only one of the two spellings is how a head silently becomes `None` and an indexer
    // silently becomes "synced by its own flag".
    let block = |k: &str| -> Option<i64> {
        let n = chain?.get(k)?.get("number")?;
        n.as_i64().or_else(|| n.as_str()?.parse().ok())
    };
    RawStatus {
        health: v.get("health").and_then(|h| h.as_str()).map(str::to_string),
        synced: v.get("synced").and_then(|s| s.as_bool()),
        chain_head_block: block("chainHeadBlock"),
        latest_block: block("latestBlock"),
        network: chain
            .and_then(|c| c.get("network"))
            .and_then(|n| n.as_str())
            .map(str::to_string),
        entity_count: v
            .get("entityCount")
            .and_then(|e| e.as_str())
            .map(str::to_string),
        fatal_error: v.get("fatalError").and_then(|f| {
            // Keyed on the message: graph-node has no fatal error without one, and an object
            // carrying only a block number is not something to report as a failure.
            let message = f.get("message")?.as_str()?.to_string();
            let block = f.get("block").filter(|b| !b.is_null()).map(|b| ErrorBlock {
                // A block number arrives as a string from some graph-node versions and a number
                // from others, which is the same trap `parse_raw_status` already handles for the
                // chain head.
                number: b
                    .get("number")
                    .and_then(|n| n.as_i64().or_else(|| n.as_str()?.parse().ok())),
                hash: b.get("hash").and_then(|h| h.as_str()).map(str::to_string),
            });
            Some(FatalError {
                message,
                handler: f
                    .get("handler")
                    .and_then(|h| h.as_str())
                    .map(str::to_string),
                block,
                deterministic: f.get("deterministic").and_then(|d| d.as_bool()),
            })
        }),
        non_fatal_errors: v
            .get("nonFatalErrors")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.get("message")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Turn one indexer's two probes into a verdict.
fn fold_one(
    alloc: &Allocated,
    status: Option<serde_json::Value>,
    serve: Option<&kittiwake_ext::HttpOutcome>,
    paid: bool,
) -> IndexerStatus {
    let raw = status.as_ref().map(parse_raw_status);
    let mut out = build_indexer_status(
        alloc.indexer_id.clone(),
        alloc.indexer_name.clone(),
        alloc.url.clone().unwrap_or_default(),
        alloc.allocated_tokens.clone(),
        raw.as_ref(),
    );
    if let Some(o) = serve {
        let (probe, cause) = match o.status {
            // No response at all. `broken` when we tried and failed, because a serving path that
            // will not answer is not serving; `unreachable` is for never having asked, which is
            // what the SSRF guard causes.
            None if o.attempts == 0 => (ServeProbe::Unreachable, "guard"),
            None => (ServeProbe::Broken, "transport"),
            Some(code) => {
                let p = if paid {
                    probe_from_paid(classify_paid_response(code, &o.content_type, &o.body))
                } else {
                    classify_serve_response(code, &o.content_type, &o.body)
                };
                (p, "response")
            }
        };
        out.serve_probe = Some(probe);
        out.serve_probe_detail = Some(ProbeDetail {
            probe,
            cause,
            error: o.error.clone(),
            status: o.status,
            content_type: (!o.content_type.is_empty()).then(|| o.content_type.clone()),
            paid,
            attempts: o.attempts,
            elapsed_ms: o.elapsed_ms,
        });
    }
    out
}

/// Probe every allocated indexer and fold the answers into one verdict for the deployment.
///
/// An allocation with no registered URL is `unreachable` without a request being made. That is not
/// the same as a failure and is counted separately, because an indexer that never published an
/// endpoint has not been asked anything.
/// Probe every allocated indexer, paying if this deployment has a payer.
///
/// `payer` rather than a receipt: a TAP receipt names one service provider, so a round that probes
/// six indexers signs six. Handing this one receipt would address it to whichever indexer it named
/// and the other five would reject it, which the classifier reads as "asked to be paid" - a
/// deployment that serves perfectly well, reported as unverified, because of how we asked.
pub async fn probe_all(
    ext: &kittiwake_ext::Ext,
    allocations: &[Allocated],
    ipfs_hash: &str,
    payer: Option<&kittiwake_chain::tap::Payer>,
) -> Vec<IndexerStatus> {
    use futures::StreamExt;

    // Cloned rather than borrowed, for the reason `search::json_docs` records: a stream of futures
    // over borrowed items is not `Send` for every lifetime, and axum needs the whole handler
    // future to be. The compiler's word for it is "implementation of `FnOnce` is not general
    // enough", which does not obviously mean that.
    let (with_url, without_url): (Vec<Allocated>, Vec<Allocated>) = allocations
        .iter()
        .cloned()
        .partition(|a| a.url.as_deref().is_some_and(|u| !u.trim().is_empty()));

    let probed: Vec<IndexerStatus> =
        futures::stream::iter(with_url.into_iter().map(|a| async move {
            let url = a.url.clone().unwrap_or_default();
            // Signing failure falls back to an unpaid probe rather than dropping the indexer: a
            // weaker question still beats reporting nothing about a deployment that may be fine.
            let receipt = payer.and_then(|p| {
                a.indexer_id
                    .parse()
                    .ok()
                    .and_then(|addr| p.receipt_for(addr).ok())
            });
            // Together rather than one after the other: they ask different questions of the same host
            // and the slow one should not be waited for twice.
            let (status, serve) = tokio::join!(
                ext.deployment_status(&url, ipfs_hash),
                ext.probe_serving(&url, ipfs_hash, receipt.as_deref(), PROBE_TIMEOUT),
            );
            fold_one(&a, status, Some(&serve), receipt.is_some())
        }))
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;

    let mut all = probed;
    all.extend(without_url.into_iter().map(|a| {
        IndexerStatus::unreachable(
            a.indexer_id,
            a.indexer_name,
            String::new(),
            a.allocated_tokens,
        )
    }));
    // Deterministic, so two identical rounds render identically and a diff between them means
    // something changed rather than that the futures finished in a different order.
    all.sort_by(|a, b| a.indexer_id.cmp(&b.indexer_id));
    reconcile_to_network_head(all)
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn a_string_block_number_and_a_numeric_one_read_the_same() {
        let as_text = serde_json::json!({
            "health": "healthy", "synced": true,
            "chains": [{"network": "mainnet", "chainHeadBlock": {"number": "100"}, "latestBlock": {"number": "90"}}]
        });
        let as_number = serde_json::json!({
            "health": "healthy", "synced": true,
            "chains": [{"network": "mainnet", "chainHeadBlock": {"number": 100}, "latestBlock": {"number": 90}}]
        });
        let a = parse_raw_status(&as_text);
        let b = parse_raw_status(&as_number);
        assert_eq!(a.chain_head_block, Some(100));
        assert_eq!(b.chain_head_block, Some(100));
        assert_eq!(a.latest_block, b.latest_block);
    }

    /// Reading only one spelling would leave the head `None`, and the verdict would fall back to
    /// graph-node's own `synced` flag - which is the thing this module exists not to trust.
    #[test]
    fn a_missing_head_falls_back_to_the_flag_which_is_why_both_spellings_matter() {
        let v = serde_json::json!({"health": "healthy", "synced": true, "chains": []});
        let raw = parse_raw_status(&v);
        assert!(raw.chain_head_block.is_none());
        assert_eq!(
            build_indexer_status("0xa", None, "u", "0", Some(&raw)).state,
            SyncState::Synced,
            "with nothing to measure, the flag is all there is"
        );
    }

    /// **The whole error, not the sentence.** A shape diff against the incumbent on 2026-09-11
    /// found this route dropping `deterministic`, `handler` and `block` from every fatal error, so
    /// a subgraph permanently broken at a known block read the same as one whose indexer's store
    /// had a bad minute. This fixture is the payload production actually returned.
    #[test]
    fn a_fatal_error_keeps_the_fields_its_message_cannot_carry() {
        let v = serde_json::json!({
            "health": "failed",
            "fatalError": {
                "message": "Failed to transact block operations: store error",
                "handler": serde_json::Value::Null,
                "block": {
                    "number": 504138418i64,
                    "hash": "0xb01b6ee77e3e1b6e5f463b21aa78b1983dd7ddab67e16d3a95503fa7b9eab2a2"
                },
                "deterministic": false
            },
            "chains": []
        });
        let f = parse_raw_status(&v).fatal_error.expect("a fatal error");
        assert_eq!(
            f.deterministic,
            Some(false),
            "the field that decides what to do"
        );
        assert_eq!(f.block.as_ref().unwrap().number, Some(504138418));
        assert!(
            f.block
                .as_ref()
                .unwrap()
                .hash
                .as_deref()
                .unwrap()
                .starts_with("0xb01b6e")
        );
        assert_eq!(f.handler, None);
    }

    /// Some graph-node versions send the block number as a string, which is the same trap the
    /// chain head already has a test for.
    #[test]
    fn a_string_error_block_number_reads_the_same_as_a_numeric_one() {
        let as_text = serde_json::json!({
            "fatalError": {"message": "x", "block": {"number": "504138418"}}, "chains": []
        });
        let as_number = serde_json::json!({
            "fatalError": {"message": "x", "block": {"number": 504138418i64}}, "chains": []
        });
        assert_eq!(
            parse_raw_status(&as_text).fatal_error.unwrap().block,
            parse_raw_status(&as_number).fatal_error.unwrap().block
        );
    }

    /// A fatal error with no message is not a fatal error. Reporting an object that carries only a
    /// block as a failure would mark a healthy deployment broken.
    #[test]
    fn an_error_object_without_a_message_is_not_one() {
        let v = serde_json::json!({"fatalError": {"block": {"number": 1}}, "chains": []});
        assert!(parse_raw_status(&v).fatal_error.is_none());
        let null = serde_json::json!({"fatalError": serde_json::Value::Null, "chains": []});
        assert!(parse_raw_status(&null).fatal_error.is_none());
    }

    #[test]
    fn a_fatal_error_and_the_non_fatal_list_are_both_read() {
        let v = serde_json::json!({
            "health": "failed",
            "fatalError": {"message": "handler panicked"},
            "nonFatalErrors": [{"message": "a"}, {"message": "b"}],
            "entityCount": "1234",
            "chains": []
        });
        let raw = parse_raw_status(&v);
        assert_eq!(
            raw.fatal_error.as_ref().map(|f| f.message.as_str()),
            Some("handler panicked")
        );
        assert_eq!(raw.non_fatal_errors, ["a", "b"]);
        assert_eq!(raw.entity_count.as_deref(), Some("1234"));
    }

    /// A transport failure is `broken`: a serving path that will not answer is not serving.
    /// `unreachable` is reserved for never having asked, which the SSRF guard causes.
    #[test]
    fn no_response_is_broken_when_we_tried_and_unreachable_when_we_did_not() {
        let a = Allocated {
            indexer_id: "0xa".into(),
            indexer_name: None,
            url: Some("http://x".into()),
            allocated_tokens: "0".into(),
        };
        let tried = kittiwake_ext::HttpOutcome {
            error: Some("timeout".into()),
            attempts: 2,
            ..Default::default()
        };
        assert_eq!(
            fold_one(&a, None, Some(&tried), false).serve_probe,
            Some(ServeProbe::Broken)
        );
        let refused = kittiwake_ext::HttpOutcome {
            error: Some("resolves to a private address".into()),
            attempts: 0,
            ..Default::default()
        };
        assert_eq!(
            fold_one(&a, None, Some(&refused), false).serve_probe,
            Some(ServeProbe::Unreachable)
        );
    }

    /// Whether a receipt was attached decides which classifier reads the response, and the two
    /// disagree on exactly the case the paid probe exists for.
    #[test]
    fn the_same_response_reads_differently_with_and_without_a_receipt() {
        let a = Allocated {
            indexer_id: "0xa".into(),
            indexer_name: None,
            url: Some("http://x".into()),
            allocated_tokens: "0".into(),
        };
        let errors = kittiwake_ext::HttpOutcome {
            status: Some(200),
            content_type: "application/json".into(),
            body: r#"{"data":null,"errors":[{"message":"BadResponse"}]}"#.into(),
            attempts: 1,
            ..Default::default()
        };
        assert_eq!(
            fold_one(&a, None, Some(&errors), false).serve_probe,
            Some(ServeProbe::AlivePaid),
            "unpaid: the query stack answered, which is all we can tell"
        );
        assert_eq!(
            fold_one(&a, None, Some(&errors), true).serve_probe,
            Some(ServeProbe::Broken),
            "paid: past the gate and still not answering"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The answer, as the panel reads it
// ---------------------------------------------------------------------------------------------

/// One indexer, on the wire.
///
/// Keys whose value is unknown are **omitted rather than sent as null**, because the handler being
/// replaced leaves them `undefined` and `JSON.stringify` drops the key. An indexer that never
/// answered carries the five things we knew before asking plus its probe verdict, which is what
/// the frozen incumbent serves and what the parity harness compares against. `indexerName` is the
/// exception: it is an explicit null there, so it is an explicit null here.
fn indexer_json(i: &IndexerStatus) -> serde_json::Value {
    fn put(o: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: serde_json::Value) {
        o.insert(k.to_string(), v);
    }
    /// Insert only when there is something to say. This is the whole mechanism: `None` leaves the
    /// key off, exactly as `undefined` does on the other side.
    fn maybe(
        o: &mut serde_json::Map<String, serde_json::Value>,
        k: &str,
        v: Option<serde_json::Value>,
    ) {
        if let Some(v) = v {
            o.insert(k.to_string(), v);
        }
    }

    let mut o = serde_json::Map::new();
    put(&mut o, "indexerId", i.indexer_id.clone().into());
    put(&mut o, "indexerName", serde_json::json!(i.indexer_name));
    put(&mut o, "url", i.url.clone().into());
    put(&mut o, "allocatedTokens", i.allocated_tokens.clone().into());
    put(&mut o, "status", i.state.as_str().into());
    if let Some(p) = i.serve_probe {
        put(&mut o, "serveProbe", serde_json::json!(p));
        put(&mut o, "servable", i.servable().into());
    }
    maybe(
        &mut o,
        "serveProbeDetail",
        i.serve_probe_detail.as_ref().map(|d| serde_json::json!(d)),
    );
    if !i.answered {
        return serde_json::Value::Object(o);
    }

    maybe(&mut o, "health", i.health.clone().map(Into::into));
    maybe(&mut o, "synced", i.synced.map(Into::into));
    maybe(&mut o, "chainHeadBlock", i.chain_head_block.map(Into::into));
    maybe(&mut o, "latestBlock", i.latest_block.map(Into::into));
    maybe(
        &mut o,
        "networkChainHead",
        i.network_chain_head.map(Into::into),
    );
    maybe(&mut o, "network", i.network.clone().map(Into::into));
    maybe(
        &mut o,
        "entityCount",
        i.entity_count.clone().map(Into::into),
    );
    // An object, not a string: the panel reads `.message`, and a bare string would be a shape
    // change dressed up as a value change.
    maybe(
        &mut o,
        "fatalError",
        i.fatal_error
            .as_ref()
            .and_then(|f| serde_json::to_value(f).ok()),
    );
    maybe(
        &mut o,
        "nonFatalErrors",
        (!i.non_fatal_errors.is_empty()).then(|| serde_json::json!(i.non_fatal_errors)),
    );
    put(&mut o, "nonFatalErrorCount", i.non_fatal_error_count.into());
    maybe(&mut o, "syncProgress", i.sync_progress.map(Into::into));
    maybe(&mut o, "blocksBehind", i.blocks_behind.map(Into::into));
    serde_json::Value::Object(o)
}

/// The whole payload for `/api/indexing-status/<deployment>`.
///
/// `total_allocations` is what the nest counted, which can exceed `indexers.len()` because the
/// allocation query is capped. Reporting `indexers.len()` for both would quietly turn a truncation
/// into a smaller network.
#[derive(Debug, Clone, Default)]
pub struct Deployment {
    pub id: String,
    pub ipfs_hash: String,
    /// A name lives in an IPFS document and this route fetches none, so it is normally `None`.
    pub display_name: Option<String>,
    pub signalled_tokens: String,
    pub staked_tokens: String,
    /// What the nest counted, which can exceed the allocations returned: the query is capped.
    pub total_allocations: i64,
}

pub fn shape(
    dep: &Deployment,
    indexers: &[IndexerStatus],
    servability: &Servability,
    rendered: &RenderedServability,
) -> serde_json::Value {
    let count = |f: fn(&IndexerStatus) -> bool| indexers.iter().filter(|i| f(i)).count();
    serde_json::json!({
        "deploymentId": dep.id,
        "ipfsHash": dep.ipfs_hash,
        // Null rather than absent, and null rather than the hash. A name lives in an IPFS document
        // and this route does not fetch one; inventing the hash as a display name would put an
        // unreadable string where the panel expects something a human chose.
        "displayName": dep.display_name,
        "signalledTokens": dep.signalled_tokens,
        "stakedTokens": dep.staked_tokens,
        "indexers": indexers.iter().map(indexer_json).collect::<Vec<_>>(),
        "totalIndexers": indexers.len(),
        "totalAllocations": dep.total_allocations,
        // `synced`/`failed`/`unreachable` are ours, from the block gap. `healthy`/`unhealthy` are
        // graph-node's own word and overlap with them freely: an indexer catching up is `syncing`
        // and `healthy` at the same time, which is what the incumbent serves.
        "syncedCount": count(|i| i.state == SyncState::Synced),
        "healthyCount": count(|i| i.reported_health() == Some("healthy")),
        "unhealthyCount": count(|i| i.reported_health() == Some("unhealthy")),
        "failedCount": count(|i| i.state == SyncState::Failed),
        "unreachableCount": count(|i| i.state == SyncState::Unreachable),
        "servability": servability,
        "servabilityRendered": rendered,
        // The gateway probe left with the API key, so no new round has a witness. Null rather than
        // omitted, because the panel distinguishes "the gateway disagreed" from "nobody asked".
        "gatewayVerdict": serde_json::Value::Null,
    })
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    fn dep(total_allocations: i64) -> Deployment {
        Deployment {
            id: "0xdep".into(),
            ipfs_hash: "Qmdep".into(),
            display_name: None,
            signalled_tokens: "1".into(),
            staked_tokens: "2".into(),
            total_allocations,
        }
    }

    /// An indexer that answered. `answered` is what decides whether the payload carries the
    /// status fields at all, so a fixture that leaves it false is testing the wrong shape.
    fn one(id: &str, state: SyncState, probe: Option<ServeProbe>) -> IndexerStatus {
        let mut s = IndexerStatus::unreachable(id, None, "http://x", "100");
        s.state = state;
        s.serve_probe = probe;
        s.answered = true;
        s
    }

    /// `synced`/`failed`/`unreachable` are ours, from the block gap. `healthy`/`unhealthy` are
    /// graph-node's own word, and the two sets overlap freely.
    ///
    /// Measured against the frozen incumbent on 2026-09-09, which serves an indexer with
    /// `status: "syncing"` and `health: "healthy"` in the same object. An earlier version of this
    /// module derived `health` from the sync gap, which would have moved every catching-up indexer
    /// out of `healthyCount` and quietly changed a number the panel shows.
    #[test]
    fn health_is_graph_nodes_word_and_overlaps_the_sync_state() {
        let mut catching_up = one("0xb", SyncState::Syncing, Some(ServeProbe::AlivePaid));
        catching_up.health = Some("healthy".into());
        let mut wobbly = one("0xc", SyncState::Synced, Some(ServeProbe::AlivePaid));
        wobbly.health = Some("unhealthy".into());
        let mut broken = one("0xd", SyncState::Failed, Some(ServeProbe::Broken));
        broken.health = Some("failed".into());
        // Never asked, so graph-node said nothing: neither healthy nor unhealthy.
        let silent = one("0xe", SyncState::Unreachable, None);

        let indexers = vec![catching_up, wobbly, broken, silent];
        let v = assess_servability(&indexers, None);
        let r = apply_persistence(&[], 3);
        let out = shape(&dep(9), &indexers, &v, &r);

        assert_eq!(out["syncedCount"], 1, "0xc is at the head");
        assert_eq!(out["failedCount"], 1);
        assert_eq!(out["unreachableCount"], 1);
        assert_eq!(out["healthyCount"], 1, "0xb, syncing and healthy at once");
        assert_eq!(
            out["unhealthyCount"], 1,
            "0xc, synced and unhealthy at once"
        );
        assert_eq!(out["totalIndexers"], 4);
        // The nest counted nine; the allocation query returned four. Reporting four for both would
        // turn a truncation into a smaller network.
        assert_eq!(out["totalAllocations"], 9);
    }

    /// An indexer that said nothing carries the five things we knew before asking, plus its probe
    /// verdict. Not a row of nulls: the handler being replaced early-returns before it has any of
    /// the status fields to report, and the frozen incumbent's payload is the same shape.
    #[test]
    fn an_indexer_that_never_answered_carries_only_what_we_knew_before_asking() {
        let mut silent = IndexerStatus::unreachable("0xa", None, "", "100");
        silent.serve_probe = Some(ServeProbe::Unreachable);
        let out = shape(
            &dep(1),
            &[silent],
            &assess_servability(&[], None),
            &apply_persistence(&[], 3),
        );
        let mut keys: Vec<String> = out["indexers"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "allocatedTokens",
                "indexerId",
                "indexerName",
                "servable",
                "serveProbe",
                "status",
                "url"
            ]
        );
    }

    /// Absent rather than null, because the handler being replaced leaves them `undefined` and
    /// `JSON.stringify` drops the key. The frozen incumbent's payload for a healthy indexer has
    /// neither, and the harness compares shape.
    #[test]
    fn a_healthy_indexer_carries_no_error_keys_at_all() {
        let clean = one("0xa", SyncState::Synced, Some(ServeProbe::AlivePaid));
        let out = shape(
            &dep(1),
            &[clean],
            &assess_servability(&[], None),
            &apply_persistence(&[], 3),
        );
        let keys: Vec<&String> = out["indexers"][0].as_object().unwrap().keys().collect();
        assert!(!keys.iter().any(|k| *k == "nonFatalErrors"), "{keys:?}");
        assert_eq!(out["indexers"][0]["fatalError"], serde_json::Value::Null);
        assert_eq!(out["indexers"][0]["nonFatalErrorCount"], 0);
    }

    #[test]
    fn a_missing_name_is_null_rather_than_the_hash() {
        let out = shape(
            &dep(0),
            &[],
            &assess_servability(&[], None),
            &apply_persistence(&[], 3),
        );
        assert!(out["displayName"].is_null());
        assert!(out["gatewayVerdict"].is_null());
    }

    /// The panel reads `fatalError.message`; a bare string would be a shape change wearing the
    /// clothes of a value change, which is how eight routes shipped wrong in the first place.
    #[test]
    fn a_fatal_error_is_an_object_with_a_message() {
        let mut i = one("0xa", SyncState::Failed, None);
        i.fatal_error = Some(FatalError::message("handler panicked"));
        let out = shape(
            &dep(1),
            &[i],
            &assess_servability(&[], None),
            &apply_persistence(&[], 3),
        );
        assert_eq!(
            out["indexers"][0]["fatalError"]["message"],
            "handler panicked"
        );
    }

    /// The key sets, pinned against what the frozen incumbent actually served on 2026-09-09 for
    /// `Qmbsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB`.
    ///
    /// This is the check that matters for #23, because the harness compares shape and eight routes
    /// once shipped a 200 of the wrong one. Reading the TypeScript and believing it is how that
    /// happened; these lists were copied off the wire.
    #[test]
    fn the_payload_carries_exactly_the_keys_the_incumbent_serves() {
        let mut full = one("0xa", SyncState::Syncing, Some(ServeProbe::AlivePaid));
        full.health = Some("healthy".into());
        full.synced = Some(false);
        full.chain_head_block = Some(100);
        full.latest_block = Some(90);
        full.network_chain_head = Some(100);
        full.network = Some("mainnet".into());
        full.entity_count = Some("12".into());
        full.sync_progress = Some(90.0);
        full.blocks_behind = Some(10);
        full.serve_probe_detail = Some(ProbeDetail {
            probe: ServeProbe::AlivePaid,
            cause: "response",
            error: None,
            status: Some(402),
            content_type: Some("application/json".into()),
            paid: true,
            attempts: 1,
            elapsed_ms: 257,
        });

        let v = assess_servability(&[full.clone()], None);
        let out = shape(&dep(2), &[full], &v, &apply_persistence(&[], 3));

        let mut top: Vec<String> = out.as_object().unwrap().keys().cloned().collect();
        top.sort();
        assert_eq!(
            top,
            [
                "deploymentId",
                "displayName",
                "failedCount",
                "gatewayVerdict",
                "healthyCount",
                "indexers",
                "ipfsHash",
                "servability",
                "servabilityRendered",
                "signalledTokens",
                "stakedTokens",
                "syncedCount",
                "totalAllocations",
                "totalIndexers",
                "unhealthyCount",
                "unreachableCount"
            ]
        );

        let mut keys: Vec<String> = out["indexers"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "allocatedTokens",
                "blocksBehind",
                "chainHeadBlock",
                "entityCount",
                "health",
                "indexerId",
                "indexerName",
                "latestBlock",
                "network",
                "networkChainHead",
                "nonFatalErrorCount",
                "servable",
                "serveProbe",
                "serveProbeDetail",
                "status",
                "syncProgress",
                "synced",
                "url"
            ]
        );

        let mut detail: Vec<String> = out["indexers"][0]["serveProbeDetail"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        detail.sort();
        assert_eq!(
            detail,
            [
                "attempts",
                "cause",
                "contentType",
                "elapsedMs",
                "error",
                "paid",
                "probe",
                "status"
            ]
        );

        let mut sv: Vec<String> = out["servability"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        sv.sort();
        assert_eq!(
            sv,
            [
                "dominantOperatorShare",
                "effectiveServingOperators",
                "effectivelyDead",
                "recovering",
                "servingIndexerCount"
            ]
        );

        let mut r: Vec<String> = out["servabilityRendered"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        r.sort();
        assert_eq!(
            r,
            ["deadStreak", "effectivelyDead", "k", "probedAt", "state"]
        );
    }

    /// A single dead round must render `rechecking`, never `dead`. This is the payload-level
    /// statement of the rule, so a future refactor cannot satisfy the unit test and still put the
    /// frightening word on the page.
    #[test]
    fn one_bad_round_never_reaches_the_wire_as_dead() {
        let indexers = vec![one("0xa", SyncState::Syncing, Some(ServeProbe::Broken))];
        let v = assess_servability(&indexers, None);
        assert!(v.effectively_dead, "the instantaneous read is dead");

        let now = Round {
            probed_at: "2026-09-09T00:00:00Z".into(),
            serving_operators: v.effective_serving_operators,
            serving_indexers: v.serving_indexer_count,
            gateway_verdict: None,
        };
        let r = apply_persistence(&[now], 3);
        let out = shape(&dep(1), &indexers, &v, &r);
        assert_eq!(out["servabilityRendered"]["state"], "rechecking");
        assert_eq!(out["servabilityRendered"]["effectivelyDead"], false);
        assert_eq!(
            out["servability"]["effectivelyDead"], true,
            "the raw read is still reported"
        );
    }
}
