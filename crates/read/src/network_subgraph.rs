//! The protocol totals on `/api/network-stats`, read a second time from the Graph Network subgraph
//! on Arbitrum, and compared field by field.
//!
//! Ours come from nests decoding the contracts' events; theirs from graph-node running the network
//! subgraph's mappings. The two are read a few blocks apart, so a small gap in a figure that moves
//! every block is expected; a large one means one side counts something the other does not.

use serde::Serialize;

/// Relative gap still put down to the two reads being at different blocks.
pub const CLOSE: f64 = 0.001;

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

pub fn query() -> String {
    let fields: Vec<&str> = FIELDS.iter().map(|(f, _)| *f).collect();
    format!(
        "{{ _meta {{ deployment block {{ number }} hasIndexingErrors }} graphNetwork(id: \"1\") {{ {} }} }}",
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
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub agree: bool,
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

/// `ours` and `theirs` are each a `graphNetwork` object.
pub fn compare(ours: &serde_json::Value, theirs: &serde_json::Value) -> Check {
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
                    let v = if rel.abs() <= CLOSE {
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
            }
        })
        .collect();
    let differing = fields
        .iter()
        .filter(|f| matches!(f.verdict, Verdict::Differs | Verdict::Missing))
        .count();
    Check {
        agree: differing == 0,
        differing,
        fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn verdict(c: &Check, f: &str) -> Verdict {
        c.fields.iter().find(|x| x.field == f).unwrap().verdict
    }

    #[test]
    fn exact_close_and_different_fields_are_told_apart() {
        let ours = json!({
            "totalTokensStaked": "712122404020421878971928527",
            "totalTokensSignalled": "10188821652095880783375969",
            "totalSupply": "3153018775839206686296721173",
            "indexerCount": 185,
        });
        let theirs = json!({
            "totalTokensStaked": "712122404020421878971928527",
            "totalTokensSignalled": "11694772357573117852969243",
            "totalSupply": "3153018798580039949231925009",
            "indexerCount": 190,
        });
        let c = compare(&ours, &theirs);
        assert_eq!(verdict(&c, "totalTokensStaked"), Verdict::Match);
        assert_eq!(verdict(&c, "totalSupply"), Verdict::Close);
        assert_eq!(verdict(&c, "totalTokensSignalled"), Verdict::Differs);
        assert_eq!(verdict(&c, "indexerCount"), Verdict::Differs);
        assert_eq!(verdict(&c, "currentEpoch"), Verdict::Missing);
        assert!(!c.agree);
        let sig = c
            .fields
            .iter()
            .find(|x| x.field == "totalTokensSignalled")
            .unwrap();
        assert!((sig.relative.unwrap() - 0.128771).abs() < 1e-5);
    }

    #[test]
    fn identical_totals_agree() {
        let all: serde_json::Map<_, _> = FIELDS
            .iter()
            .map(|(f, _)| (f.to_string(), json!("42")))
            .collect();
        let v = serde_json::Value::Object(all);
        let c = compare(&v, &v);
        assert!(c.agree);
        assert_eq!(c.differing, 0);
    }

    #[test]
    fn the_query_asks_for_every_field() {
        let q = query();
        assert!(FIELDS.iter().all(|(f, _)| q.contains(f)));
        assert!(q.starts_with("{ _meta"));
    }
}
