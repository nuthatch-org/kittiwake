//! Lodestar's protocol figures, read a second time from the Graph Network subgraph on Arbitrum and
//! compared: the network totals, every indexer and every deployment with signal or stake.
//!
//! Ours come from nests decoding the contracts' events; theirs from graph-node running the network
//! subgraph's mappings, two code paths that share nothing but the chain. Each subgraph query is
//! pinned to the block our nest answered at, so any difference is a difference in what is counted,
//! not in when it was read. When the subgraph has not reached that block it is read at its head and
//! the answer says it is unpinned.

use std::collections::BTreeMap;

use serde::Serialize;

/// Relative gap put down to an unpinned read: the two sides were looking at different blocks.
pub const CLOSE: f64 = 0.001;
/// Differing entities listed per field. The counts are always whole.
const EXAMPLES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// GRT wei, compared as integers.
    Tokens,
    Count,
}

pub const FIELDS: &[(&str, Kind)] = &[
    ("totalTokensStaked", Kind::Tokens),
    ("totalDelegatedTokens", Kind::Tokens),
    ("totalTokensAllocated", Kind::Tokens),
    ("totalTokensSignalled", Kind::Tokens),
    ("totalIndexingRewards", Kind::Tokens),
    ("totalQueryFees", Kind::Tokens),
    ("totalSupply", Kind::Tokens),
    ("indexerCount", Kind::Count),
    ("stakedIndexersCount", Kind::Count),
    ("delegatorCount", Kind::Count),
    ("activeDelegatorCount", Kind::Count),
    ("curatorCount", Kind::Count),
    ("activeCuratorCount", Kind::Count),
    ("subgraphCount", Kind::Count),
    ("activeSubgraphCount", Kind::Count),
    ("currentEpoch", Kind::Count),
];

/// Differences that are understood, said beside the figure rather than counted against it.
pub fn note(field: &str) -> Option<&'static str> {
    match field {
        "indexerCount" => Some(
            "The subgraph also counts addresses that set a URL or delegation parameters and never \
             staked; ours counts addresses that have staked.",
        ),
        "stakedIndexersCount" => Some(
            "The subgraph's counter has drifted from its own indexers: count those with stake \
             above zero and it agrees with ours.",
        ),
        "totalSupply" => Some(
            "Ours is the token contract's own totalSupply(), sampled every few minutes rather than \
             at this block; the subgraph sums mints and burns.",
        ),
        "curatorCount" => Some(
            "The subgraph also counts an account that never signalled; ours counts accounts that \
             have.",
        ),
        "subgraphCount" | "activeSubgraphCount" => Some(
            "Lodestar misses the five subgraphs transferred from L1 and never published on L2: the \
             allocations nest does not index SubgraphReceivedFromL1.",
        ),
        _ => None,
    }
}

fn block_arg(block: Option<i64>) -> String {
    block.map_or(String::new(), |b| format!("block: {{ number: {b} }}"))
}

pub fn totals_query(block: Option<i64>) -> String {
    let fields: Vec<&str> = FIELDS.iter().map(|(f, _)| *f).collect();
    format!(
        "{{ _meta {{ deployment block {{ number }} hasIndexingErrors }} graphNetwork(id: \"1\", {}) {{ {} }} }}",
        block_arg(block),
        fields.join(" ")
    )
}

/// One kind of entity read from both sides.
pub struct Entity {
    pub name: &'static str,
    /// The subgraph's collection and filter.
    pub collection: &'static str,
    pub filter: &'static str,
    /// (our column, the subgraph's field).
    pub fields: &'static [(&'static str, &'static str)],
    /// A subgraph field that, when zero on an entity only the subgraph has, explains its absence
    /// from ours, with the reason.
    pub explains_absence: Option<(&'static str, &'static str)>,
}

/// Delegation cuts are left out on purpose: for indexers that never opened a Horizon provision the
/// nest keeps their last legacy cut and the subgraph resets it to the default, and both are defensible.
pub const INDEXERS: Entity = Entity {
    name: "indexers",
    collection: "indexers",
    filter: "",
    fields: &[
        ("staked_tokens", "stakedTokens"),
        ("locked_tokens", "lockedTokens"),
        ("delegated_tokens", "delegatedTokens"),
        ("allocated_tokens", "allocatedTokens"),
        ("rewards_earned", "rewardsEarned"),
        ("query_fees_collected", "queryFeesCollected"),
        ("delegator_shares", "delegatorShares"),
    ],
    explains_absence: Some((
        "stakedTokens",
        "addresses that set a URL or delegation parameters and never staked, which the subgraph \
         counts as indexers",
    )),
};

pub const DEPLOYMENTS: Entity = Entity {
    name: "deployments",
    collection: "subgraphDeployments",
    filter: "signalledTokens_gt: \"0\"",
    fields: &[
        ("signalled_tokens", "signalledTokens"),
        ("staked_tokens", "stakedTokens"),
        ("query_fees_amount", "queryFeesAmount"),
    ],
    explains_absence: None,
};

/// Every row, with every field cast to text so wei survives the trip.
pub fn nest_sql(e: &Entity) -> String {
    let cols: Vec<String> = e
        .fields
        .iter()
        .map(|(c, _)| format!("CAST({c} AS VARCHAR) AS {c}"))
        .collect();
    let (table, filter) = match e.name {
        "indexers" => ("lodestar_indexers", ""),
        _ => ("lodestar_deployments", " WHERE signalled_tokens > 0"),
    };
    format!(
        "SELECT LOWER(id) AS id, {} FROM {table}{filter}",
        cols.join(", ")
    )
}

/// One page of the subgraph's side, ordered by id and keyed past `after`.
pub fn page_query(e: &Entity, block: Option<i64>, after: &str, first: usize) -> String {
    let fields: Vec<&str> = e.fields.iter().map(|(_, f)| *f).collect();
    let sep = if e.filter.is_empty() { "" } else { ", " };
    format!(
        "{{ rows: {}(first: {first}, orderBy: id, {}, where: {{ id_gt: \"{after}\"{sep}{} }}) {{ id {} }} }}",
        e.collection,
        block_arg(block),
        e.filter,
        fields.join(" ")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Match,
    Close,
    Differs,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Field {
    pub field: &'static str,
    pub kind: Kind,
    /// As each side sent it: wei for tokens, so strings.
    pub ours: Option<String>,
    pub theirs: Option<String>,
    /// (theirs - ours) / the larger of the two.
    pub relative: Option<f64>,
    pub verdict: Verdict,
    pub note: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub agree: bool,
    /// Differing fields without a note.
    pub differing: usize,
    pub fields: Vec<Field>,
}

fn text(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `ours` and `theirs` are each a `graphNetwork` object. `pinned` says both were read at one block,
/// in which case only exact agreement counts.
pub fn compare(ours: &serde_json::Value, theirs: &serde_json::Value, pinned: bool) -> Totals {
    let fields: Vec<Field> = FIELDS
        .iter()
        .map(|(field, kind)| {
            let a = text(ours.get(*field));
            let b = text(theirs.get(*field));
            let parsed = a
                .as_deref()
                .and_then(|a| a.parse::<u128>().ok())
                .zip(b.as_deref().and_then(|b| b.parse::<u128>().ok()));
            let (relative, verdict) = match parsed {
                None => (None, Verdict::Missing),
                Some((x, y)) if x == y => (Some(0.0), Verdict::Match),
                Some((x, y)) => {
                    let rel = (y as f64 - x as f64) / (x.max(y) as f64);
                    let v = if !pinned && rel.abs() <= CLOSE {
                        Verdict::Close
                    } else {
                        Verdict::Differs
                    };
                    (Some(rel), v)
                }
            };
            Field {
                field,
                kind: *kind,
                ours: a,
                theirs: b,
                relative,
                verdict,
                note: note(field),
            }
        })
        .collect();
    let differing = fields
        .iter()
        .filter(|f| matches!(f.verdict, Verdict::Differs | Verdict::Missing) && f.note.is_none())
        .count();
    Totals {
        agree: differing == 0,
        differing,
        fields,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityExample {
    pub id: String,
    pub ours: Option<String>,
    pub theirs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityField {
    pub field: &'static str,
    pub differing: usize,
    pub examples: Vec<EntityExample>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityCheck {
    pub entity: &'static str,
    pub ours_block: Option<i64>,
    pub theirs_block: Option<i64>,
    pub pinned: bool,
    /// Present on both sides and compared field by field.
    pub compared: usize,
    pub only_ours: usize,
    pub only_theirs: usize,
    /// Only in the subgraph, for a reason already understood; not counted against `agree`.
    pub only_theirs_explained: usize,
    pub only_theirs_reason: Option<&'static str>,
    pub only_ours_examples: Vec<String>,
    pub only_theirs_examples: Vec<String>,
    pub fields: Vec<EntityField>,
    pub agree: bool,
}

/// `ours` are nest rows keyed `id` with our column names; `theirs` subgraph rows with its field names.
pub fn compare_entities(
    e: &Entity,
    ours: &[serde_json::Value],
    theirs: &[serde_json::Value],
    ours_block: Option<i64>,
    theirs_block: Option<i64>,
    pinned: bool,
) -> EntityCheck {
    let key = |v: &serde_json::Value| v.get("id").and_then(|i| i.as_str()).map(str::to_lowercase);
    let a: BTreeMap<String, &serde_json::Value> =
        ours.iter().filter_map(|v| Some((key(v)?, v))).collect();
    let b: BTreeMap<String, &serde_json::Value> =
        theirs.iter().filter_map(|v| Some((key(v)?, v))).collect();
    let only_ours: Vec<String> = a.keys().filter(|k| !b.contains_key(*k)).cloned().collect();
    let explained = |v: &serde_json::Value| {
        e.explains_absence.is_some_and(|(field, _)| {
            text(v.get(field)).and_then(|t| t.parse::<u128>().ok()) == Some(0)
        })
    };
    let mut only_theirs = Vec::new();
    let mut theirs_explained = 0;
    for (k, v) in &b {
        if a.contains_key(k) {
            continue;
        }
        if explained(v) {
            theirs_explained += 1;
        } else {
            only_theirs.push(k.clone());
        }
    }

    let fields: Vec<EntityField> = e
        .fields
        .iter()
        .map(|(col, field)| {
            let mut differing = 0;
            let mut examples = Vec::new();
            for (id, x) in &a {
                let Some(y) = b.get(id) else { continue };
                let ours = text(x.get(*col));
                let theirs = text(y.get(*field));
                let same = match (ours.as_deref(), theirs.as_deref()) {
                    (Some(p), Some(q)) => {
                        matches!((p.parse::<u128>(), q.parse::<u128>()), (Ok(x), Ok(y)) if x == y)
                    }
                    _ => false,
                };
                if !same {
                    differing += 1;
                    if examples.len() < EXAMPLES {
                        examples.push(EntityExample {
                            id: id.clone(),
                            ours,
                            theirs,
                        });
                    }
                }
            }
            EntityField {
                field,
                differing,
                examples,
            }
        })
        .collect();
    let agree =
        only_ours.is_empty() && only_theirs.is_empty() && fields.iter().all(|f| f.differing == 0);
    EntityCheck {
        entity: e.name,
        ours_block,
        theirs_block,
        pinned,
        compared: a.keys().filter(|k| b.contains_key(*k)).count(),
        only_ours: only_ours.len(),
        only_theirs: only_theirs.len(),
        only_theirs_explained: theirs_explained,
        only_theirs_reason: e.explains_absence.map(|(_, why)| why),
        only_ours_examples: only_ours.into_iter().take(EXAMPLES).collect(),
        only_theirs_examples: only_theirs.into_iter().take(EXAMPLES).collect(),
        fields,
        agree,
    }
}

/// The subgraph's indexers with stake above zero, the count its own counter should hold.
pub fn staked_indexers(theirs: &[serde_json::Value]) -> usize {
    theirs
        .iter()
        .filter(|v| {
            text(v.get("stakedTokens"))
                .and_then(|s| s.parse::<u128>().ok())
                .is_some_and(|s| s > 0)
        })
        .count()
}

/// graph-node's refusal of a block it has not reached yet.
pub fn not_yet_indexed(reason: &str) -> bool {
    reason.contains("has only indexed up to block")
        || reason.contains("not yet indexed")
        || reason.contains("only indexed up to")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn verdict(c: &Totals, f: &str) -> Verdict {
        c.fields.iter().find(|x| x.field == f).unwrap().verdict
    }

    #[test]
    fn unpinned_totals_allow_a_small_gap_and_pinned_ones_do_not() {
        let ours =
            json!({ "totalSupply": "3153018775839206686296721173", "totalTokensStaked": "5" });
        let theirs =
            json!({ "totalSupply": "3153018798580039949231925009", "totalTokensStaked": "5" });
        assert_eq!(
            verdict(&compare(&ours, &theirs, false), "totalSupply"),
            Verdict::Close
        );
        assert_eq!(
            verdict(&compare(&ours, &theirs, true), "totalSupply"),
            Verdict::Differs
        );
        assert_eq!(
            verdict(&compare(&ours, &theirs, true), "totalTokensStaked"),
            Verdict::Match
        );
    }

    #[test]
    fn explained_differences_are_noted_and_not_counted() {
        let all = |n: i64| -> serde_json::Value {
            serde_json::Value::Object(
                FIELDS
                    .iter()
                    .map(|(f, _)| (f.to_string(), json!(n)))
                    .collect(),
            )
        };
        let ours = all(5);
        let mut theirs = all(5);
        theirs["indexerCount"] = json!(6);
        let c = compare(&ours, &theirs, true);
        assert!(c.agree);
        let f = c.fields.iter().find(|x| x.field == "indexerCount").unwrap();
        assert_eq!(f.verdict, Verdict::Differs);
        assert!(f.note.is_some());

        theirs["totalTokensSignalled"] = json!(9);
        let c = compare(&ours, &theirs, true);
        assert!(!c.agree);
        assert_eq!(c.differing, 1);
    }

    #[test]
    fn entities_are_matched_by_id_and_compared_field_by_field() {
        let ours = vec![
            json!({"id": "0xA", "signalled_tokens": "10", "staked_tokens": "1", "query_fees_amount": "0"}),
            json!({"id": "0xb", "signalled_tokens": "20", "staked_tokens": "2", "query_fees_amount": "0"}),
            json!({"id": "0xc", "signalled_tokens": "30", "staked_tokens": "3", "query_fees_amount": "0"}),
        ];
        let theirs = vec![
            json!({"id": "0xa", "signalledTokens": "10", "stakedTokens": "1", "queryFeesAmount": "0"}),
            json!({"id": "0xb", "signalledTokens": "21", "stakedTokens": "2", "queryFeesAmount": "0"}),
            json!({"id": "0xd", "signalledTokens": "40", "stakedTokens": "4", "queryFeesAmount": "0"}),
        ];
        let c = compare_entities(&DEPLOYMENTS, &ours, &theirs, Some(7), Some(7), true);
        assert_eq!((c.compared, c.only_ours, c.only_theirs), (2, 1, 1));
        assert_eq!(c.only_ours_examples, vec!["0xc"]);
        let sig = c
            .fields
            .iter()
            .find(|f| f.field == "signalledTokens")
            .unwrap();
        assert_eq!(sig.differing, 1);
        assert_eq!(sig.examples[0].id, "0xb");
        assert!(!c.agree);
    }

    #[test]
    fn never_staked_indexers_only_in_the_subgraph_are_explained() {
        let ours = vec![json!({"id": "0xa", "staked_tokens": "1"})];
        let theirs = vec![
            json!({"id": "0xa", "stakedTokens": "1"}),
            json!({"id": "0xb", "stakedTokens": "0"}),
            json!({"id": "0xc", "stakedTokens": "7"}),
        ];
        let c = compare_entities(&INDEXERS, &ours, &theirs, None, None, false);
        assert_eq!((c.only_theirs, c.only_theirs_explained), (1, 1));
        assert_eq!(c.only_theirs_examples, vec!["0xc"]);
        assert!(!c.agree);
    }

    #[test]
    fn a_field_missing_on_one_side_differs() {
        let ours = vec![json!({"id": "0xa", "signalled_tokens": "10"})];
        let theirs = vec![json!({"id": "0xa", "signalledTokens": "10"})];
        let c = compare_entities(&DEPLOYMENTS, &ours, &theirs, None, None, false);
        let staked = c.fields.iter().find(|f| f.field == "stakedTokens").unwrap();
        assert_eq!(staked.differing, 1);
    }

    #[test]
    fn queries_are_pinned_and_paged() {
        let q = page_query(&DEPLOYMENTS, Some(42), "0xff", 1000);
        assert!(q.contains("block: { number: 42 }"));
        assert!(q.contains("id_gt: \"0xff\", signalledTokens_gt: \"0\""));
        assert!(q.contains("signalledTokens stakedTokens queryFeesAmount"));
        let q = page_query(&INDEXERS, None, "", 1000);
        assert!(q.contains("where: { id_gt: \"\" }"));
        assert!(!q.contains("block:"));
        assert!(totals_query(Some(9)).contains("graphNetwork(id: \"1\", block: { number: 9 })"));
        assert!(
            nest_sql(&DEPLOYMENTS)
                .ends_with("FROM lodestar_deployments WHERE signalled_tokens > 0")
        );
    }

    #[test]
    fn staked_indexers_are_counted_from_the_entities() {
        let rows = vec![
            json!({"id": "0x1", "stakedTokens": "0"}),
            json!({"id": "0x2", "stakedTokens": "5"}),
        ];
        assert_eq!(staked_indexers(&rows), 1);
        assert!(not_yet_indexed(
            "subgraph QmX has only indexed up to block number 5"
        ));
    }
}
