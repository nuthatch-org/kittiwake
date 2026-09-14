//! Indexer names, from the two places a name can be verified.
//!
//! Measured 2026-09-14 over the directory's 100 indexers: 52 have a primary ENS name that resolves
//! back to them, one has a Graph default name (also theirs on ENS), and the 53 account metadata
//! documents the network subgraph took `displayName` from carry none at all.

use std::collections::HashMap;

use futures::StreamExt;
use kittiwake_chain::Chain;
use kittiwake_db::Pool;
use kittiwake_nest::{Lane, NestGate};
use kittiwake_types::{ApiError, NestId};

use crate::{IngestError, Ingested, i, page, s};

/// Two mainnet reads per indexer against public endpoints, so politeness rather than throughput.
const CONCURRENCY: usize = 8;

/// The gns nest's newest `SetDefaultName` per account. One row today.
pub const DEFAULT_NAMES_SQL: &str =
    "SELECT account, name, name_system, name_identifier FROM account_default_names";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    pub default_name: Option<String>,
    pub ens_name: Option<String>,
}

impl Names {
    /// The Graph default name first: it is the one the account chose for this protocol.
    pub fn display(&self) -> Option<String> {
        self.default_name.clone().or_else(|| self.ens_name.clone())
    }
}

type Declared = (String, i64, String);

pub async fn names(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
    chain: &Chain,
) -> Result<Ingested, IngestError> {
    let addresses: Vec<String> = sqlx::query_scalar("SELECT address FROM indexers")
        .fetch_all(pool)
        .await?;

    let declared: HashMap<String, Declared> = page(
        gate,
        NestId::Gns,
        DEFAULT_NAMES_SQL.into(),
        lane,
        "default names",
    )
    .await?
    .rows
    .iter()
    .filter_map(|r| {
        let account = s(r, "account")?.to_lowercase();
        let declared = (
            s(r, "name")?,
            i(r, "name_system")?,
            s(r, "name_identifier")?,
        );
        Some((account, declared))
    })
    .collect();

    let looked_up: Vec<(String, Result<Names, ApiError>)> = futures::stream::iter(addresses)
        .map(|address| {
            let declared = declared.get(&address.to_lowercase()).cloned();
            async move {
                let names = lookup(chain, &address, declared).await;
                (address, names)
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    let (settled, failed) = settle(looked_up);
    if failed > 0 {
        tracing::warn!("{failed} indexer name lookups failed; their names are left as they were");
    }
    write(pool, &settled).await?;
    Ok(Ingested {
        rows: settled.len() as u64,
    })
}

async fn lookup(
    chain: &Chain,
    address: &str,
    declared: Option<Declared>,
) -> Result<Names, ApiError> {
    let ens_name = chain.ens_name(address).await?;
    let default_name = match declared {
        Some((name, system, identifier)) => {
            chain
                .verified_default_name(address, &name, system, &identifier)
                .await?
        }
        None => None,
    };
    Ok(Names {
        default_name,
        ens_name,
    })
}

/// A lookup that failed is dropped rather than written as "no name", which it is not.
fn settle(looked_up: Vec<(String, Result<Names, ApiError>)>) -> (Vec<(String, Names)>, usize) {
    let total = looked_up.len();
    let settled: Vec<(String, Names)> = looked_up
        .into_iter()
        .filter_map(|(a, r)| r.ok().map(|n| (a, n)))
        .collect();
    let failed = total - settled.len();
    (settled, failed)
}

async fn write(pool: &Pool, rows: &[(String, Names)]) -> Result<(), sqlx::Error> {
    if rows.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE indexers SET name = t.name, ens_name = t.ens_name \
         FROM UNNEST($1::text[], $2::text[], $3::text[]) AS t(address, name, ens_name) \
         WHERE indexers.address = t.address",
    )
    .bind(rows.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|(_, n)| n.display()).collect::<Vec<_>>())
    .bind(
        rows.iter()
            .map(|(_, n)| n.ens_name.clone())
            .collect::<Vec<_>>(),
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(default_name: Option<&str>, ens_name: Option<&str>) -> Names {
        Names {
            default_name: default_name.map(str::to_string),
            ens_name: ens_name.map(str::to_string),
        }
    }

    #[test]
    fn the_graph_default_name_is_shown_before_the_ens_name() {
        assert_eq!(
            names(Some("grassets-tech-2"), Some("grassets-tech-2.eth")).display(),
            Some("grassets-tech-2".into())
        );
        assert_eq!(
            names(None, Some("pinax.eth")).display(),
            Some("pinax.eth".into())
        );
        assert_eq!(names(None, None).display(), None);
    }

    /// Writing a failed lookup as null would strip a real name for as long as an RPC is down.
    #[test]
    fn a_failed_lookup_is_not_written_but_a_confirmed_absence_is() {
        let (settled, failed) = settle(vec![
            ("0xa".into(), Ok(names(None, Some("a.eth")))),
            ("0xb".into(), Err(ApiError::Unavailable("timed out".into()))),
            ("0xc".into(), Ok(names(None, None))),
        ]);
        assert_eq!(failed, 1);
        let written: Vec<&str> = settled.iter().map(|(a, _)| a.as_str()).collect();
        assert_eq!(written, ["0xa", "0xc"]);
    }
}
