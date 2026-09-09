//! The named tier: a caller sends a **name and typed arguments, never SQL**.
//!
//! `/api/sql/query` is the exploring tier, and nuthatch's own RFC-0034 is blunt about why that is
//! not the whole story: arbitrary SQL "is the product for a local developer and a liability for a
//! public endpoint", because the node's guards - timeout, row cap, concurrency - are
//! **self-protection, not a security boundary**. They bound the damage of one query and say nothing
//! about which questions the surface is willing to answer at all.
//!
//! This is where we say which. It lives here rather than in the nests' mount config because those
//! are single-nest processes serving live dashboard panels, and a product feature is a poor reason
//! to reconfigure four of them. The nest's guards still apply underneath; this narrows the set of
//! questions before one is ever asked.
//!
//! ## Why `int` and `address` and nothing else
//!
//! Both have a **total validating parse** into a form with no escaping hazard: an int becomes
//! decimal digits, an address becomes `0x` and forty hex characters and cannot contain a quote.
//! Each renders into SQL safely by construction rather than by careful escaping.
//!
//! `text` is deliberately absent, and the reasoning is nuthatch's, adopted wholesale. Free text
//! needs escaping; escaping has to be right in every dialect and every context - string literal,
//! identifier, LIKE pattern - and "we escaped it carefully" is how this class of bug ships.
//! Matching caller-supplied SQL against a pattern is the shape of every SQL-filter bypass ever
//! written. This is not that: the caller never sends text that reaches the query.
//!
//! ## Every query is pinned
//!
//! Each takes a `before_block` and bounds itself by it. Not decoration: it is what makes an answer
//! reproducible, and therefore what makes it worth attesting to. The nest answers from sealed
//! history plus a moving tip, so an unpinned answer cannot be reproduced by anyone, including
//! whoever took it. A named, pinned query is the unit a signed receipt is useful over, because two
//! parties can then agree on what the question *was* rather than only on what came back. #19

/// The only two argument types. See the module note on why there is no `text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    Int,
    Address,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct QueryParam {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub kind: ParamType,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct NamedQuery {
    pub name: &'static str,
    pub dataset: &'static str,
    pub description: &'static str,
    pub params: &'static [QueryParam],
    /// `{param}` placeholders, substituted only with validated values.
    pub sql: &'static str,
}

/// Every named query bounds itself by this, so every answer is reproducible.
const PIN: QueryParam = QueryParam {
    name: "before_block",
    kind: ParamType::Int,
    description: "Only consider blocks at or below this one. Required: it is what makes the answer reproducible.",
};

const INDEXER: QueryParam = QueryParam {
    name: "indexer",
    kind: ParamType::Address,
    description: "The service provider receiving the delegation.",
};

pub static NAMED_QUERIES: &[NamedQuery] = &[
    NamedQuery {
        name: "delegations_to_indexer",
        dataset: "staking",
        description: "Every delegation to one indexer, newest first, up to a block.",
        params: &[INDEXER, PIN],
        sql: "SELECT block_number, block_timestamp, delegator, tokens, shares, tx_hash\n\
              FROM staking__tokens_delegated\n\
              WHERE serviceProvider = '{indexer}' AND block_number <= {before_block}\n\
              ORDER BY block_number DESC\n\
              LIMIT 500",
    },
    NamedQuery {
        name: "delegator_activity",
        dataset: "staking",
        description: "One delegator's delegations and undelegations together, up to a block. The union is the point: neither table answers \"what did they do\" on its own.",
        params: &[
            QueryParam {
                name: "delegator",
                kind: ParamType::Address,
                description: "The delegator to trace.",
            },
            PIN,
        ],
        sql: "SELECT block_number, block_timestamp, 'delegated' AS action, serviceProvider, tokens\n\
              FROM staking__tokens_delegated\n\
              WHERE delegator = '{delegator}' AND block_number <= {before_block}\n\
              UNION ALL\n\
              SELECT block_number, block_timestamp, 'undelegated' AS action, serviceProvider, tokens\n\
              FROM staking__tokens_undelegated\n\
              WHERE delegator = '{delegator}' AND block_number <= {before_block}\n\
              ORDER BY block_number DESC\n\
              LIMIT 500",
    },
    NamedQuery {
        name: "net_delegation_to_indexer",
        dataset: "staking",
        description: "Tokens delegated minus tokens undelegated for one indexer, as of a block. Returned as text, because these are uint256 and a float would quietly change the number.",
        params: &[
            QueryParam {
                name: "indexer",
                kind: ParamType::Address,
                description: "The service provider.",
            },
            PIN,
        ],
        sql: "SELECT\n  \
              CAST(SUM(CAST(tokens AS HUGEINT)) AS VARCHAR) AS delegated,\n  \
              CAST(COUNT(*) AS VARCHAR) AS events\n\
              FROM staking__tokens_delegated\n\
              WHERE serviceProvider = '{indexer}' AND block_number <= {before_block}",
    },
    NamedQuery {
        name: "issuance_rate_changes",
        dataset: "dips",
        description: "Every change to the Issuance Allocator's per-block rate, up to a block. This is the number GIP-0088 turns on.",
        params: &[PIN],
        sql: "SELECT block_number, block_timestamp, oldIssuancePerBlock, newIssuancePerBlock, tx_hash\n\
              FROM issuance_allocator__issuance_per_block_updated\n\
              WHERE block_number <= {before_block}\n\
              ORDER BY block_number DESC\n\
              LIMIT 200",
    },
    NamedQuery {
        name: "dips_agreements",
        dataset: "dips",
        description: "Indexing agreements added to the Recurring Agreement Manager, up to a block. Empty today, which is itself the answer: DIPS is armed and unfunded.",
        params: &[PIN],
        sql: "SELECT block_number, block_timestamp, agreementId, collector, dataService, provider\n\
              FROM recurring_agreement_manager__agreement_added\n\
              WHERE block_number <= {before_block}\n\
              ORDER BY block_number DESC\n\
              LIMIT 200",
    },
];

pub fn find_named_query(name: &str) -> Option<&'static NamedQuery> {
    NAMED_QUERIES.iter().find(|q| q.name == name)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("missing argument `{name}` ({kind})")]
    Missing { name: String, kind: &'static str },
    #[error("`{0}` must be a whole number")]
    NotAnInt(String),
    #[error("`{0}` must be a 0x-prefixed 20-byte address")]
    NotAnAddress(String),
    #[error("unknown argument `{0}`")]
    Unknown(String),
}

/// Validate the arguments and render the statement.
///
/// Each type has a total parse into a lexically safe form, so the rendered SQL cannot contain
/// anything the caller wrote. An argument that does not parse is **refused**; nothing is coerced,
/// trimmed into shape, or escaped.
pub fn render(
    query: &NamedQuery,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, RenderError> {
    let mut values: Vec<(&str, String)> = Vec::with_capacity(query.params.len());

    for p in query.params {
        let raw = args.get(p.name).filter(|v| !v.is_null());
        // A JSON number and a JSON string are both accepted, because both are what a caller sends
        // in practice; what they contain is then judged identically. Anything else is missing.
        let as_text = match raw {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        if as_text.is_empty() {
            return Err(RenderError::Missing {
                name: p.name.to_string(),
                kind: match p.kind {
                    ParamType::Int => "int",
                    ParamType::Address => "address",
                },
            });
        }

        let value = match p.kind {
            // Digits only. No sign, no exponent, no separators, no leading plus: a block number is
            // a count. Anything else is refused rather than coerced.
            ParamType::Int => {
                let ok = (1..=20).contains(&as_text.len())
                    && as_text.bytes().all(|b| b.is_ascii_digit());
                if !ok {
                    return Err(RenderError::NotAnInt(p.name.to_string()));
                }
                as_text
            }
            // Exactly `0x` and forty hex. Cannot contain a quote, a space or a comment marker, so
            // rendering it inside a string literal is safe by construction.
            ParamType::Address => {
                let hex = as_text.strip_prefix("0x").unwrap_or("");
                if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(RenderError::NotAnAddress(p.name.to_string()));
                }
                as_text.to_lowercase()
            }
        };
        values.push((p.name, value));
    }

    // An unknown argument is a refusal, not something to ignore. Silently dropping it means a
    // caller who misspelt `indexer` gets a confident answer to a different question.
    for given in args.keys() {
        if !query.params.iter().any(|p| p.name == given) {
            return Err(RenderError::Unknown(given.clone()));
        }
    }

    let mut sql = query.sql.to_string();
    for (name, value) in &values {
        sql = sql.replace(&format!("{{{name}}}"), value);
    }
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    const ADDR: &str = "0x090F7382F9EA85C733Cd501f4D87F16CB5B83ed3";

    /// The property the whole tier rests on: nothing the caller wrote reaches the statement except
    /// through a total parse. A quote, a comment marker or a semicolon in any argument is refused
    /// rather than escaped, so there is no escaping to get wrong.
    #[test]
    fn nothing_a_caller_writes_can_reach_the_sql() {
        let q = find_named_query("delegations_to_indexer").unwrap();
        for hostile in [
            "0x' OR '1'='1",
            "0x090F7382F9EA85C733Cd501f4D87F16CB5B83ed3'; DROP TABLE x --",
            "0x090f7382f9ea85c733cd501f4d87f16cb5b83ed3 UNION SELECT 1",
            "0x090f7382f9ea85c733cd501f4d87f16cb5b83ed",
            "090f7382f9ea85c733cd501f4d87f16cb5b83ed3",
        ] {
            let out = render(
                q,
                &args(&[
                    ("indexer", serde_json::json!(hostile)),
                    ("before_block", serde_json::json!(1)),
                ]),
            );
            assert_eq!(
                out,
                Err(RenderError::NotAnAddress("indexer".into())),
                "{hostile}"
            );
        }
        for hostile in ["1; DROP TABLE x", "-1", "1e9", "1_000", "+1", " 1 2"] {
            let out = render(
                find_named_query("issuance_rate_changes").unwrap(),
                &args(&[("before_block", serde_json::json!(hostile))]),
            );
            assert_eq!(
                out,
                Err(RenderError::NotAnInt("before_block".into())),
                "{hostile}"
            );
        }
    }

    #[test]
    fn a_valid_call_renders_every_placeholder_and_lowercases_the_address() {
        let q = find_named_query("delegator_activity").unwrap();
        let sql = render(
            q,
            &args(&[
                ("delegator", serde_json::json!(ADDR)),
                ("before_block", serde_json::json!(497_000_000u64)),
            ]),
        )
        .expect("valid");
        assert!(!sql.contains('{'), "a placeholder survived: {sql}");
        assert_eq!(
            sql.matches(&ADDR.to_lowercase()).count(),
            2,
            "both halves of the union"
        );
        assert!(
            !sql.contains(ADDR),
            "the checksummed spelling must not reach the statement"
        );
        assert_eq!(sql.matches("497000000").count(), 2);
    }

    /// A number and a string are both what callers send; they are judged identically.
    #[test]
    fn a_block_number_may_arrive_as_a_number_or_as_a_string() {
        let q = find_named_query("issuance_rate_changes").unwrap();
        let a = render(
            q,
            &args(&[("before_block", serde_json::json!(400_000_000u64))]),
        );
        let b = render(
            q,
            &args(&[("before_block", serde_json::json!("400000000"))]),
        );
        assert_eq!(a, b);
        assert!(a.unwrap().contains("<= 400000000"));
    }

    /// Silently dropping a misspelt argument means a confident answer to a different question.
    #[test]
    fn a_misspelt_argument_is_refused_rather_than_ignored() {
        let q = find_named_query("delegations_to_indexer").unwrap();
        let out = render(
            q,
            &args(&[
                ("indexer", serde_json::json!(ADDR)),
                ("before_block", serde_json::json!(1)),
                ("indexr", serde_json::json!(ADDR)),
            ]),
        );
        assert_eq!(out, Err(RenderError::Unknown("indexr".into())));
    }

    #[test]
    fn a_missing_argument_names_itself_and_its_type() {
        let q = find_named_query("delegations_to_indexer").unwrap();
        assert_eq!(
            render(q, &args(&[("before_block", serde_json::json!(1))])),
            Err(RenderError::Missing {
                name: "indexer".into(),
                kind: "address"
            })
        );
        assert_eq!(
            render(q, &args(&[("indexer", serde_json::json!(ADDR))])),
            Err(RenderError::Missing {
                name: "before_block".into(),
                kind: "int"
            })
        );
    }

    /// Every declared query must be pinned, and every dataset it names must be one this service
    /// actually exposes. A query naming a dataset nobody serves is our bug, and it should fail the
    /// build rather than a caller's request.
    #[test]
    fn every_query_is_pinned_and_targets_a_dataset_we_serve() {
        assert!(!NAMED_QUERIES.is_empty());
        for q in NAMED_QUERIES {
            assert!(
                q.params.iter().any(|p| p.name == "before_block"),
                "{} is not pinned",
                q.name
            );
            assert!(
                crate::find_dataset(q.dataset).is_some(),
                "{} names the unknown dataset {}",
                q.name,
                q.dataset
            );
            // Every placeholder in the statement must be a declared parameter, or rendering leaves
            // a `{name}` in the SQL and the nest gets a syntax error instead of a question.
            for token in q.sql.split('{').skip(1) {
                let name = token.split('}').next().unwrap_or_default();
                assert!(
                    q.params.iter().any(|p| p.name == name),
                    "{} has an undeclared placeholder {{{name}}}",
                    q.name
                );
            }
        }
    }

    /// Names are how callers address these, so a duplicate would make one of them unreachable.
    #[test]
    fn the_names_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for q in NAMED_QUERIES {
            assert!(seen.insert(q.name), "duplicate name {}", q.name);
        }
    }
}
