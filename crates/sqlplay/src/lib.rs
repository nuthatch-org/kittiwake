//! The public SQL surface: which datasets a stranger may ask, and a first pass over their SQL.
//!
//! **Where the security actually is, and it is not here.** The nest opens DuckDB with external
//! access disabled, a directory restriction and a locked configuration, applies a function
//! allowlist as well as a denylist, strips comments before matching, and refuses unknown table
//! references. It was built to face the public. This screening is a cheap first pass so obvious
//! nonsense never crosses the network; where the two disagree, the nest wins.

use kittiwake_types::NestId;
use sqlparser::ast::Statement;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// One dataset a stranger may query. An explicit allowlist, not a passthrough: the origin fronts
/// several nests and some exist to serve a single panel, so proxying the lot because they share a
/// hostname is how a private dataset becomes public by accident.
#[derive(Debug, Clone, Copy)]
pub struct Dataset {
    pub id: &'static str,
    pub label: &'static str,
    pub nest: NestId,
    pub chain: &'static str,
    pub description: &'static str,
    /// A frozen archive rather than a live follower. The reader must be told, or a page showing an
    /// archive beside three live datasets invites someone to read old data as current.
    pub archival: bool,
    /// The query the playground offers first.
    pub sample: &'static str,
}

/// The default sample.
///
/// A sample that names a table is a sample that breaks the day that table is renamed, and the first
/// thing a visitor would see is an error on a page that works. `information_schema` cannot be
/// wrong, and listing the tables is the right first move in an unfamiliar dataset anyway.
pub const LIST_TABLES: &str =
    "SELECT table_name\nFROM information_schema.tables\nORDER BY table_name\nLIMIT 50";

pub const DATASETS: &[Dataset] = &[
    Dataset {
        id: "staking",
        label: "Horizon staking",
        nest: NestId::Alloc,
        chain: "Arbitrum One",
        description: "The delegation events from HorizonStaking, on the nest that indexes the whole protocol. The source behind the delegation feed on this dashboard.",
        archival: false,
        sample: "SELECT block_number, serviceProvider, delegator, tokens\nFROM staking__tokens_delegated\nORDER BY block_number DESC\nLIMIT 20",
    },
    Dataset {
        id: "dips",
        label: "DIPS",
        nest: NestId::Dips,
        chain: "Arbitrum One",
        description: "Direct Indexer Payments: the Issuance Allocator, Recurring Agreement Manager and Recurring Collector. Nothing else indexes these three contracts.",
        archival: false,
        sample: LIST_TABLES,
    },
    Dataset {
        id: "dips-sepolia",
        label: "DIPS (Arbitrum Sepolia)",
        nest: NestId::DipsSepolia,
        chain: "Arbitrum Sepolia",
        description: "The same three DIPS contracts on the testnet where the agreement lifecycle has actually been exercised.",
        archival: false,
        sample: "SELECT block_number, agreementId, tokens_dec\nFROM recurring_collector__r_c_a_collected\nORDER BY block_number DESC\nLIMIT 20",
    },
    Dataset {
        id: "gns",
        label: "GNS / developer activity",
        nest: NestId::Gns,
        chain: "Arbitrum One",
        description: "The GNS publication events behind the developer-activity chart.",
        archival: false,
        sample: LIST_TABLES,
    },
    Dataset {
        id: "legacy-flows",
        label: "Legacy GRT flows",
        nest: NestId::LegacyFlows,
        chain: "Arbitrum One",
        description: "Delegation across the Horizon and pre-Horizon staking contracts together. A frozen full-history archive, not a live follower: its data ends where the archive ends.",
        archival: true,
        sample: LIST_TABLES,
    },
];

pub fn find_dataset(id: &str) -> Option<&'static Dataset> {
    DATASETS.iter().find(|d| d.id == id)
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ScreenError {
    #[error("that is not valid SQL: {0}")]
    Unparseable(String),
    #[error("send one statement, not {0}")]
    NotOneStatement(usize),
    #[error("this surface is read-only; {0} is not a SELECT")]
    NotReadOnly(&'static str),
}

/// Accept exactly one read-only statement. A `SELECT`, with or without CTEs, and nothing else.
pub fn screen(sql: &str) -> Result<(), ScreenError> {
    let statements = Parser::parse_sql(&GenericDialect {}, sql)
        .map_err(|e| ScreenError::Unparseable(e.to_string()))?;

    if statements.len() != 1 {
        return Err(ScreenError::NotOneStatement(statements.len()));
    }

    match &statements[0] {
        Statement::Query(_) => Ok(()),
        other => Err(ScreenError::NotReadOnly(statement_kind(other))),
    }
}

fn statement_kind(s: &Statement) -> &'static str {
    match s {
        Statement::Insert(_) => "INSERT",
        Statement::Update { .. } => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Drop { .. } => "DROP",
        Statement::Truncate { .. } => "TRUNCATE",
        Statement::AlterTable { .. } => "ALTER",
        Statement::Copy { .. } => "COPY",
        Statement::Grant { .. } => "GRANT",
        Statement::Set(_) => "SET",
        _ => "this",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_select_is_allowed() {
        assert!(screen("SELECT id, tokens FROM staking__tokens_delegated LIMIT 10").is_ok());
    }

    #[test]
    fn a_cte_is_still_a_select() {
        assert!(
            screen("WITH recent AS (SELECT * FROM staking__tokens_delegated LIMIT 5) SELECT * FROM recent")
                .is_ok()
        );
    }

    #[test]
    fn writes_are_refused() {
        for sql in [
            "DELETE FROM staking__tokens_delegated",
            "UPDATE staking__tokens_delegated SET tokens = 0",
            "INSERT INTO staking__tokens_delegated VALUES (1)",
            "DROP TABLE staking__tokens_delegated",
        ] {
            assert!(
                matches!(screen(sql), Err(ScreenError::NotReadOnly(_))),
                "{sql} was allowed"
            );
        }
    }

    /// The classic: a harmless SELECT with something appended after the semicolon.
    #[test]
    fn a_second_statement_is_refused_even_when_the_first_is_innocent() {
        let err = screen("SELECT 1; DROP TABLE staking__tokens_delegated").unwrap_err();
        assert_eq!(err, ScreenError::NotOneStatement(2));
    }

    #[test]
    fn nonsense_is_refused_before_it_crosses_the_network() {
        assert!(matches!(
            screen("not sql at all"),
            Err(ScreenError::Unparseable(_))
        ));
    }

    #[test]
    fn every_advertised_dataset_resolves_to_a_nest() {
        assert_eq!(DATASETS.len(), 5);
        for d in DATASETS {
            assert!(find_dataset(d.id).is_some());
            assert_eq!(d.archival, d.nest.is_archival(), "{}", d.id);
        }
    }
}
