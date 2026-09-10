//! The Direct Indexer Payments agreement lifecycle, assembled from the dips-nest event tables.
//!
//! Every table read here is empty on Arbitrum One today, and that is the reason to write it now
//! rather than on the day it fills. The alternative is composing a view in a hurry, against real
//! money, with everybody watching.
//!
//! The fold is pure and takes the rows as an argument, which matters more than usual: there is no
//! populated nest to test against, so a fixture is the only exercise this code will get before it
//! matters. #67

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Table names come from `schema.json` in nightswatchhq/dips-nest rather than from guesswork, and
/// the convention has a trap in it: `RCACollected` becomes `recurring_collector__r_c_a_collected`,
/// not `..._rca_collected`, and the reasonable guess is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    OfferStored,
    OfferCancelled,
    Accepted,
    Updated,
    Added,
    Rejected,
    Collected,
    Canceled,
    Removed,
}

/// In the order the stages happen, which is the order the fold walks them in.
pub const STAGES: &[Stage] = &[
    Stage::OfferStored,
    Stage::OfferCancelled,
    Stage::Accepted,
    Stage::Updated,
    Stage::Added,
    Stage::Rejected,
    Stage::Collected,
    Stage::Canceled,
    Stage::Removed,
];

impl Stage {
    pub fn table(self) -> &'static str {
        match self {
            Stage::OfferStored => "recurring_collector__offer_stored",
            Stage::OfferCancelled => "recurring_collector__offer_cancelled",
            Stage::Accepted => "recurring_collector__agreement_accepted",
            Stage::Updated => "recurring_collector__agreement_updated",
            Stage::Added => "recurring_agreement_manager__agreement_added",
            Stage::Rejected => "recurring_agreement_manager__agreement_rejected",
            Stage::Collected => "recurring_collector__r_c_a_collected",
            Stage::Canceled => "recurring_collector__agreement_canceled",
            Stage::Removed => "recurring_agreement_manager__agreement_removed",
        }
    }

    /// The name the response calls it, which is the incumbent's key rather than this enum's.
    pub fn key(self) -> &'static str {
        match self {
            Stage::OfferStored => "offerStored",
            Stage::OfferCancelled => "offerCancelled",
            Stage::Accepted => "accepted",
            Stage::Updated => "updated",
            Stage::Added => "added",
            Stage::Rejected => "rejected",
            Stage::Collected => "collected",
            Stage::Canceled => "canceled",
            Stage::Removed => "removed",
        }
    }

    /// How far through the lifecycle a stage is.
    ///
    /// Not simply "the latest block wins". `collected` is a recurring event that says nothing about
    /// whether the agreement later ended, so a collection must never demote a cancellation.
    fn rank(self) -> u8 {
        match self {
            Stage::OfferStored => 1,
            Stage::OfferCancelled => 2,
            Stage::Accepted | Stage::Updated => 3,
            Stage::Added | Stage::Collected => 4,
            Stage::Rejected => 5,
            Stage::Canceled => 6,
            Stage::Removed => 7,
        }
    }

    fn status(self) -> Status {
        match self {
            Stage::OfferStored => Status::Offered,
            Stage::OfferCancelled => Status::OfferWithdrawn,
            Stage::Accepted | Stage::Updated => Status::Accepted,
            Stage::Added | Stage::Collected => Status::Active,
            Stage::Rejected => Status::Rejected,
            Stage::Canceled => Status::Cancelled,
            Stage::Removed => Status::Removed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Offered,
    OfferWithdrawn,
    Accepted,
    Active,
    Rejected,
    Cancelled,
    Removed,
}

impl Status {
    fn key(self) -> &'static str {
        match self {
            Status::Offered => "offered",
            Status::OfferWithdrawn => "offer-withdrawn",
            Status::Accepted => "accepted",
            Status::Active => "active",
            Status::Rejected => "rejected",
            Status::Cancelled => "cancelled",
            Status::Removed => "removed",
        }
    }
}

const ALL_STATUSES: &[Status] = &[
    Status::Offered,
    Status::OfferWithdrawn,
    Status::Accepted,
    Status::Active,
    Status::Rejected,
    Status::Cancelled,
    Status::Removed,
];

const WEI_PER_GRT: f64 = 1e18;

/// `type(uint64).max`, which the collector uses to mean "no end date".
const NO_END: &str = "18446744073709551615";

fn address(row: &Value, key: &str) -> Option<String> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

/// Decimal-string wei to GRT. Absent reads as zero, never as a missing number.
fn grt(value: Option<&Value>) -> f64 {
    let Some(value) = value else { return 0.0 };
    match value {
        Value::String(s) if !s.is_empty() => s.parse::<f64>().unwrap_or(0.0) / WEI_PER_GRT,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) / WEI_PER_GRT,
        _ => 0.0,
    }
}

/// A big-integer column arrives as `<name>_dec`; the raw column is the fallback.
fn big(row: &Value, name: &str) -> f64 {
    grt(row
        .get(format!("{name}_dec"))
        .filter(|v| !v.is_null())
        .or_else(|| row.get(name)))
}

fn number(row: &Value, key: &str) -> f64 {
    row.get(key)
        .and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// An agreement's end, or nothing where it has none.
///
/// The sentinel is compared **as a string**, before any numeric conversion, because the conversion
/// is precisely what destroys it: `u64::MAX` through an `f64` becomes 18446744073709552000, which
/// renders as a date in the year 584,542,046,090. A field whose whole job is to say "this never
/// expires" was instead saying something absurd, confidently. Found on Sepolia's real rows, where
/// 111 of 113 agreements carry it.
fn ends_at(row: &Value) -> Option<f64> {
    let raw = match row.get("endsAt")? {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if raw == NO_END {
        return None;
    }
    raw.parse::<f64>().ok().filter(|n| n.is_finite())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub stage: String,
    pub status: Status,
    pub agreement_id: String,
    pub block: f64,
    pub timestamp: f64,
    pub tx_hash: Option<String>,
    pub payer: Option<String>,
    pub service_provider: Option<String>,
    pub data_service: Option<String>,
    /// GRT moved by this event, for `collected` only.
    pub tokens: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agreement {
    pub id: String,
    pub payer: Option<String>,
    pub service_provider: Option<String>,
    pub data_service: Option<String>,
    pub status: Status,
    pub first_seen: f64,
    pub last_seen: f64,
    pub ends_at: Option<f64>,
    pub max_initial_tokens: Option<f64>,
    pub max_ongoing_tokens_per_second: Option<f64>,
    pub collections: u32,
    pub collected_grt: f64,
    pub data_service_cut_grt: f64,
    /// Enum straight from the contract, unlabelled because the meanings are not ours to invent.
    pub rejected_reason: Option<f64>,
    pub canceled_by: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    /// Whether this service could ask the nest at all.
    ///
    /// Always true here: reaching this code means the read succeeded, and a nest that cannot be
    /// asked is an error rather than a summary. It is on the wire because `DipsAgreements.tsx`
    /// and `DipsStatus.tsx` both open with `if (!d?.available) return null`, so a response
    /// without it renders as an empty panel with no error anywhere - the shape of the fault that
    /// put eight routes into production answering 200 on 7 September.
    pub available: bool,
    pub agreements: Vec<Agreement>,
    pub events: Vec<Event>,
    pub counts: serde_json::Map<String, Value>,
    pub total_collected_grt: f64,
    /// True when no lifecycle event of any kind exists yet, which on Arbitrum One is every day so
    /// far. Said explicitly rather than left to be inferred from a zero.
    pub empty: bool,
}

fn agreement_id(row: &Value) -> Option<String> {
    row.get("agreementId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn counts_of(agreements: &[Agreement]) -> serde_json::Map<String, Value> {
    let mut counts = serde_json::Map::new();
    for status in ALL_STATUSES {
        // Every key is present at zero. A missing key and a count of none are the same fact, and a
        // consumer that indexes into this should not have to know which it got.
        counts.insert(status.key().into(), Value::from(0));
    }
    for a in agreements {
        let entry = counts.entry(a.status.key()).or_insert(Value::from(0));
        *entry = Value::from(entry.as_u64().unwrap_or(0) + 1);
    }
    counts
}

/// Fold the per-stage rows into one agreement list and one ordered event stream.
pub fn build(rows: &BTreeMap<Stage, Vec<Value>>) -> Summary {
    let mut events: Vec<Event> = Vec::new();
    for stage in STAGES {
        for row in rows.get(stage).into_iter().flatten() {
            // An event we cannot attribute to an agreement is not an event we can show.
            let Some(agreement_id) = agreement_id(row) else {
                continue;
            };
            events.push(Event {
                stage: stage.key().to_string(),
                status: stage.status(),
                agreement_id,
                block: number(row, "block_number"),
                timestamp: number(row, "block_timestamp"),
                tx_hash: row
                    .get("tx_hash")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                payer: address(row, "payer"),
                service_provider: address(row, "serviceProvider")
                    .or_else(|| address(row, "provider")),
                data_service: address(row, "dataService"),
                tokens: (*stage == Stage::Collected).then(|| big(row, "tokens")),
            });
        }
    }
    events.sort_by(|a, b| {
        a.block
            .total_cmp(&b.block)
            .then_with(|| a.stage.cmp(&b.stage))
    });

    let mut by_id: BTreeMap<String, Agreement> = BTreeMap::new();
    let mut rank: BTreeMap<String, (u8, f64)> = BTreeMap::new();

    for stage in STAGES {
        for row in rows.get(stage).into_iter().flatten() {
            let Some(id) = agreement_id(row) else {
                continue;
            };
            let block = number(row, "block_number");
            let ts = number(row, "block_timestamp");

            let a = by_id.entry(id.clone()).or_insert_with(|| Agreement {
                id: id.clone(),
                payer: None,
                service_provider: None,
                data_service: None,
                status: stage.status(),
                first_seen: ts,
                last_seen: ts,
                ends_at: None,
                max_initial_tokens: None,
                max_ongoing_tokens_per_second: None,
                collections: 0,
                collected_grt: 0.0,
                data_service_cut_grt: 0.0,
                rejected_reason: None,
                canceled_by: None,
            });

            // First non-null wins: a later event that omits the payer must not erase one an
            // earlier event supplied.
            a.payer = a.payer.take().or_else(|| address(row, "payer"));
            a.service_provider = a
                .service_provider
                .take()
                .or_else(|| address(row, "serviceProvider").or_else(|| address(row, "provider")));
            a.data_service = a
                .data_service
                .take()
                .or_else(|| address(row, "dataService"));

            a.first_seen = if a.first_seen == 0.0 {
                ts
            } else {
                a.first_seen.min(ts)
            };
            a.last_seen = a.last_seen.max(ts);

            match stage {
                Stage::Accepted | Stage::Updated => {
                    a.ends_at = ends_at(row).or(a.ends_at);
                    a.max_initial_tokens = Some(big(row, "maxInitialTokens"));
                    a.max_ongoing_tokens_per_second = Some(big(row, "maxOngoingTokensPerSecond"));
                }
                Stage::Collected => {
                    a.collections += 1;
                    a.collected_grt += big(row, "tokens");
                    a.data_service_cut_grt += big(row, "dataServiceCut");
                }
                Stage::Rejected if row.get("reason").is_some_and(|v| !v.is_null()) => {
                    a.rejected_reason = Some(number(row, "reason"));
                }
                Stage::Canceled if row.get("canceledBy").is_some_and(|v| !v.is_null()) => {
                    a.canceled_by = Some(number(row, "canceledBy"));
                }
                _ => {}
            }

            // Furthest stage reached wins, so a routine collection cannot un-cancel an agreement.
            //
            // The incumbent computed this as `rank * 1e6 + min(block, 999_999)`, which on a chain
            // past block 500,000,000 clamps every block to the same value and makes the second
            // term inert. It cannot have changed an answer - the only equal-rank pairs are
            // accepted/updated and added/collected, and each pair shares a status - so this is the
            // same result with the tie-break that was intended rather than the one that compiled.
            let here = (stage.rank(), block);
            let best = rank.entry(id).or_insert((0, f64::NEG_INFINITY));
            if here >= *best {
                *best = here;
                a.status = stage.status();
            }
        }
    }

    let mut agreements: Vec<Agreement> = by_id.into_values().collect();
    agreements.sort_by(|x, y| y.last_seen.total_cmp(&x.last_seen));

    Summary {
        available: true,
        counts: counts_of(&agreements),
        total_collected_grt: agreements.iter().map(|a| a.collected_grt).sum(),
        empty: events.is_empty(),
        agreements,
        events,
    }
}

/// Narrow a summary to one service provider. The per-indexer portfolio is this, and no more.
pub fn for_indexer(summary: &Summary, indexer: &str) -> Summary {
    let target = indexer.to_lowercase();
    let agreements: Vec<Agreement> = summary
        .agreements
        .iter()
        .filter(|a| a.service_provider.as_deref() == Some(target.as_str()))
        .cloned()
        .collect();
    let ids: std::collections::BTreeSet<&str> = agreements.iter().map(|a| a.id.as_str()).collect();
    Summary {
        available: true,
        counts: counts_of(&agreements),
        total_collected_grt: agreements.iter().map(|a| a.collected_grt).sum(),
        // An indexer with no agreements is empty for that indexer, whatever the room holds.
        empty: agreements.is_empty(),
        events: summary
            .events
            .iter()
            .filter(|e| ids.contains(e.agreement_id.as_str()))
            .cloned()
            .collect(),
        agreements,
    }
}

/// The fold as `many_folded` wants it: one object keyed by stage, in and one summary out.
pub fn fold(combined: &Value) -> Value {
    let mut rows: BTreeMap<Stage, Vec<Value>> = BTreeMap::new();
    for stage in STAGES {
        if let Some(array) = combined.get(stage.key()).and_then(Value::as_array) {
            rows.insert(*stage, array.clone());
        }
    }
    serde_json::to_value(build(&rows)).unwrap_or_else(|_| Value::Null)
}

/// Narrow a already-folded summary to one service provider.
///
/// Applied after the cache rather than before it, so one fold serves every caller and an indexer
/// filter costs the nest nothing.
pub fn narrow(summary: &Value, indexer: &str) -> Value {
    match serde_json::from_value::<Summary>(summary.clone()) {
        Ok(full) => {
            serde_json::to_value(for_indexer(&full, indexer)).unwrap_or_else(|_| Value::Null)
        }
        // A summary this cannot read is one this module wrote, so returning it unfiltered would
        // hand one indexer somebody else's agreements. Empty is the safe answer.
        Err(_) => serde_json::to_value(for_indexer(&build(&BTreeMap::new()), indexer))
            .unwrap_or_else(|_| Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(pairs: Vec<(Stage, Value)>) -> BTreeMap<Stage, Vec<Value>> {
        let mut out: BTreeMap<Stage, Vec<Value>> = BTreeMap::new();
        for (stage, row) in pairs {
            out.entry(stage).or_default().push(row);
        }
        out
    }

    fn wei(grt: u64) -> String {
        format!("{grt}000000000000000000")
    }

    #[test]
    fn an_empty_nest_says_so_rather_than_returning_zeroes() {
        let summary = build(&BTreeMap::new());
        assert!(summary.empty);
        assert!(summary.agreements.is_empty());
        assert_eq!(summary.total_collected_grt, 0.0);
        // Every status key is present at zero, so a consumer never has to tell absent from none.
        assert_eq!(summary.counts.len(), 7);
        assert_eq!(summary.counts["active"], 0);
        assert_eq!(summary.counts["offer-withdrawn"], 0);
    }

    /// The one the rank exists for. A collection arriving after a cancellation must not resurrect
    /// the agreement, whatever block it landed in.
    #[test]
    fn a_collection_cannot_un_cancel_an_agreement() {
        let summary = build(&rows(vec![
            (
                Stage::Accepted,
                json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10}),
            ),
            (
                Stage::Canceled,
                json!({"agreementId": "a1", "block_number": 2, "block_timestamp": 20, "canceledBy": 1}),
            ),
            (
                Stage::Collected,
                json!({"agreementId": "a1", "block_number": 3, "block_timestamp": 30, "tokens_dec": wei(5)}),
            ),
        ]));
        let a = &summary.agreements[0];
        assert_eq!(
            a.status,
            Status::Cancelled,
            "cancelled outranks a later collection"
        );
        assert_eq!(a.collections, 1, "the collection is still counted");
        assert_eq!(a.collected_grt, 5.0);
        assert_eq!(a.canceled_by, Some(1.0));
    }

    /// `u64::MAX` means "no end date" and must be caught before it becomes a float, because
    /// becoming a float is what turns it into a date in the year 584 billion.
    #[test]
    fn the_no_end_sentinel_is_a_null_rather_than_a_date() {
        let summary = build(&rows(vec![(
            Stage::Accepted,
            json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10, "endsAt": NO_END}),
        )]));
        assert_eq!(summary.agreements[0].ends_at, None);

        let real = build(&rows(vec![(
            Stage::Accepted,
            json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10, "endsAt": "1790000000"}),
        )]));
        assert_eq!(real.agreements[0].ends_at, Some(1_790_000_000.0));
    }

    /// The decimal column wins, and the raw one stands in when it is absent. Reading the raw
    /// column when both exist would take whichever the nest happened to widen.
    #[test]
    fn a_big_integer_prefers_its_decimal_column() {
        let both = build(&rows(vec![(
            Stage::Collected,
            json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10,
                   "tokens_dec": wei(7), "tokens": wei(999)}),
        )]));
        assert_eq!(both.agreements[0].collected_grt, 7.0);

        let raw_only = build(&rows(vec![(
            Stage::Collected,
            json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10, "tokens": wei(3)}),
        )]));
        assert_eq!(raw_only.agreements[0].collected_grt, 3.0);
    }

    /// A later event that omits the payer must not erase one an earlier event supplied.
    #[test]
    fn identity_fields_keep_the_first_value_seen() {
        let summary = build(&rows(vec![
            (
                Stage::OfferStored,
                json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10, "payer": "0xPAYER", "serviceProvider": "0xABC"}),
            ),
            (
                Stage::Collected,
                json!({"agreementId": "a1", "block_number": 2, "block_timestamp": 20}),
            ),
        ]));
        let a = &summary.agreements[0];
        assert_eq!(a.payer.as_deref(), Some("0xpayer"), "and lowercased");
        assert_eq!(a.service_provider.as_deref(), Some("0xabc"));
    }

    /// `provider` is the other spelling the manager's events use for the same party.
    #[test]
    fn either_spelling_of_the_service_provider_is_read() {
        let summary = build(&rows(vec![(
            Stage::Added,
            json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10, "provider": "0xDEF"}),
        )]));
        assert_eq!(
            summary.agreements[0].service_provider.as_deref(),
            Some("0xdef")
        );
    }

    #[test]
    fn a_row_with_no_agreement_id_is_dropped_rather_than_shown() {
        let summary = build(&rows(vec![
            (
                Stage::Accepted,
                json!({"block_number": 1, "block_timestamp": 10}),
            ),
            (
                Stage::Accepted,
                json!({"agreementId": "", "block_number": 1, "block_timestamp": 10}),
            ),
        ]));
        assert!(summary.empty);
        assert!(summary.agreements.is_empty());
    }

    #[test]
    fn one_indexers_portfolio_is_its_own_agreements_and_their_events() {
        let summary = build(&rows(vec![
            (
                Stage::Added,
                json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10, "provider": "0xaaa"}),
            ),
            (
                Stage::Added,
                json!({"agreementId": "a2", "block_number": 2, "block_timestamp": 20, "provider": "0xbbb"}),
            ),
            (
                Stage::Collected,
                json!({"agreementId": "a2", "block_number": 3, "block_timestamp": 30, "tokens_dec": wei(4), "provider": "0xbbb"}),
            ),
        ]));
        assert_eq!(summary.agreements.len(), 2);
        assert_eq!(summary.total_collected_grt, 4.0);

        let mine = for_indexer(&summary, "0xAAA");
        assert_eq!(mine.agreements.len(), 1);
        assert_eq!(mine.agreements[0].id, "a1");
        assert_eq!(mine.events.len(), 1, "only events for my agreements");
        assert_eq!(mine.total_collected_grt, 0.0);
        assert!(!mine.empty);

        let nobody = for_indexer(&summary, "0xccc");
        assert!(
            nobody.empty,
            "no agreements for this indexer is empty for them"
        );
        assert_eq!(nobody.counts["active"], 0);
    }

    /// The response is destructured by a page, so the wire names are the contract.
    #[test]
    fn the_wire_names_are_the_incumbents() {
        let summary = build(&rows(vec![(
            Stage::Collected,
            json!({"agreementId": "a1", "block_number": 1, "block_timestamp": 10,
                   "tokens_dec": wei(1), "dataServiceCut_dec": wei(1), "serviceProvider": "0xa"}),
        )]));
        let json = serde_json::to_value(&summary).unwrap();
        for key in [
            "agreements",
            "events",
            "counts",
            "totalCollectedGrt",
            "empty",
        ] {
            assert!(json.get(key).is_some(), "Summary is missing {key}");
        }
        for key in [
            "id",
            "payer",
            "serviceProvider",
            "dataService",
            "status",
            "firstSeen",
            "lastSeen",
            "endsAt",
            "maxInitialTokens",
            "maxOngoingTokensPerSecond",
            "collections",
            "collectedGrt",
            "dataServiceCutGrt",
            "rejectedReason",
            "canceledBy",
        ] {
            assert!(
                json["agreements"][0].get(key).is_some(),
                "Agreement is missing {key}"
            );
        }
        for key in [
            "stage",
            "status",
            "agreementId",
            "block",
            "timestamp",
            "txHash",
            "tokens",
        ] {
            assert!(
                json["events"][0].get(key).is_some(),
                "Event is missing {key}"
            );
        }
        assert_eq!(json["events"][0]["stage"], "collected");
        assert_eq!(json["agreements"][0]["status"], "active");
    }

    /// The name a reasonable person would guess is the wrong one, so it is asserted rather than
    /// trusted: `RCACollected` is `r_c_a_collected`, not `rca_collected`.
    #[test]
    fn the_table_names_are_the_nests_and_not_a_guess() {
        assert_eq!(
            Stage::Collected.table(),
            "recurring_collector__r_c_a_collected"
        );
        assert_eq!(STAGES.len(), 9);
        // Every stage names a distinct table, or one of them is silently reading another's rows.
        let tables: std::collections::BTreeSet<&str> = STAGES.iter().map(|s| s.table()).collect();
        assert_eq!(tables.len(), STAGES.len());
    }
}
