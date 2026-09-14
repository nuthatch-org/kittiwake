//! First materialisation slice: publish a complete price snapshot atomically.

use chrono::{DateTime, Utc};
use dune_client::Snapshot;
use sqlx::PgPool;
use std::collections::BTreeSet;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid price snapshot: {0}")]
    Invalid(&'static str),
    #[error("price materialisation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("dune client error: {0}")]
    Client(#[from] dune_client::Error),
    #[error("dune ledger error: {0}")]
    Ledger(#[from] dune_ledger::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshnessReport {
    pub query_id: String,
    pub last_execution_id: Option<String>,
    pub last_executed_at: Option<DateTime<Utc>>,
    pub is_fresh: bool,
    pub age_seconds: Option<i64>,
}

pub const MIGRATION: &str = include_str!("../migrations/001_price_pilot.sql");

/// Does not migrate or execute Dune queries. Call against a separately migrated database.
pub async fn materialize(pool: &PgPool, snapshot: &Snapshot) -> Result<bool, Error> {
    let executed_at = validate(snapshot)?;
    let id = &snapshot.first_page.execution_id;
    let mut tx = pool.begin().await?;
    // Serialise this feed only, including first publication when no pointer exists yet.
    sqlx::query("SELECT pg_advisory_xact_lock(8699345)")
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query("INSERT INTO dune_executions(execution_id, query_id, executed_at, row_count) VALUES ($1, 'grt_price_daily', $2, $3) ON CONFLICT DO NOTHING")
        .bind(id).bind(executed_at).bind(snapshot.rows.len() as i64).execute(&mut *tx).await?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    for (index, row) in snapshot.rows.iter().enumerate() {
        sqlx::query("INSERT INTO dune_grt_price_daily(execution_id, executed_at, row_index, payload) VALUES ($1, $2, $3, $4)")
            .bind(id).bind(executed_at).bind(index as i32).bind(row).execute(&mut *tx).await?;
    }
    // Out-of-order completions may be retained for audit but must never replace newer data.
    sqlx::query("INSERT INTO dune_publications(query_id, execution_id, executed_at) VALUES ('grt_price_daily', $1, $2) ON CONFLICT (query_id) DO UPDATE SET execution_id = EXCLUDED.execution_id, executed_at = EXCLUDED.executed_at WHERE dune_publications.executed_at < EXCLUDED.executed_at")
        .bind(id).bind(executed_at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

fn validate(snapshot: &Snapshot) -> Result<DateTime<Utc>, Error> {
    if snapshot.first_page.query_id != 8699345
        || snapshot.rows.is_empty()
        || snapshot.rows.len() != snapshot.first_page.result.metadata.total_row_count
        || snapshot.rows.len() > 31
        || snapshot.first_page.state != "QUERY_STATE_COMPLETED"
    {
        return Err(Error::Invalid("identity, completion or row count"));
    }
    let executed_at = DateTime::parse_from_rfc3339(
        snapshot
            .first_page
            .execution_ended_at
            .as_deref()
            .ok_or(Error::Invalid("missing execution timestamp"))?,
    )
    .map_err(|_| Error::Invalid("execution timestamp"))?
    .with_timezone(&Utc);
    let mut days = BTreeSet::new();
    for row in &snapshot.rows {
        let day = row
            .get("day")
            .and_then(|v| v.as_str())
            .ok_or(Error::Invalid("day"))?;
        let day = chrono::NaiveDateTime::parse_from_str(day, "%Y-%m-%d %H:%M:%S%.f UTC")
            .map_err(|_| Error::Invalid("day timestamp"))?
            .and_utc();
        if day > executed_at || !days.insert(day) {
            return Err(Error::Invalid("future or duplicate day"));
        }
        let number = |name| {
            row.get(name)
                .and_then(|v| v.as_f64())
                .filter(|v| v.is_finite() && *v > 0.)
                .ok_or(Error::Invalid("price"))
        };
        let (open, high, low, close) = (
            number("open")?,
            number("high")?,
            number("low")?,
            number("close")?,
        );
        if low > open || low > close || high < open || high < close || low > high {
            return Err(Error::Invalid("OHLC bounds"));
        }
        if row
            .get("samples")
            .and_then(|v| v.as_u64())
            .is_none_or(|n| n == 0 || n > 1440)
        {
            return Err(Error::Invalid("sample count"));
        }
    }
    Ok(executed_at)
}

pub async fn last_publication(
    pool: &PgPool,
    query_id: &str,
) -> Result<Option<(String, DateTime<Utc>)>, Error> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT execution_id, executed_at FROM dune_publications WHERE query_id = $1",
    )
    .bind(query_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn prune_old_executions(
    pool: &PgPool,
    query_id: &str,
    keep_last_n: usize,
) -> Result<u64, Error> {
    let mut tx = pool.begin().await?;
    let to_delete: Vec<String> = sqlx::query_scalar(
        "SELECT execution_id FROM dune_executions \
         WHERE query_id = $1 \
           AND execution_id NOT IN (SELECT execution_id FROM dune_publications WHERE query_id = $1) \
           AND execution_id NOT IN ( \
               SELECT execution_id FROM dune_executions \
               WHERE query_id = $1 \
               ORDER BY executed_at DESC \
               LIMIT $2 \
           )",
    )
    .bind(query_id)
    .bind(keep_last_n as i64)
    .fetch_all(&mut *tx)
    .await?;

    if to_delete.is_empty() {
        tx.rollback().await?;
        return Ok(0);
    }

    if query_id == "grt_price_daily" {
        for id in &to_delete {
            sqlx::query("DELETE FROM dune_grt_price_daily WHERE execution_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }

    let mut deleted = 0u64;
    for id in &to_delete {
        let res = sqlx::query("DELETE FROM dune_executions WHERE execution_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        deleted += res.rows_affected();
    }
    tx.commit().await?;
    Ok(deleted)
}

pub async fn check_freshness(
    pool: &PgPool,
    query_id: &str,
    freshness_sla_seconds: u64,
) -> Result<FreshnessReport, Error> {
    let pub_opt = last_publication(pool, query_id).await?;
    let now = Utc::now();
    let (last_execution_id, last_executed_at, is_fresh, age_seconds) = match pub_opt {
        Some((id, exec_at)) => {
            let age = (now - exec_at).num_seconds();
            let fresh = age >= 0 && (age as u64) <= freshness_sla_seconds;
            (Some(id), Some(exec_at), fresh, Some(age))
        }
        None => (None, None, false, None),
    };
    Ok(FreshnessReport {
        query_id: query_id.to_string(),
        last_execution_id,
        last_executed_at,
        is_fresh,
        age_seconds,
    })
}

pub async fn poll_and_materialize(
    client: &dune_client::DuneClient,
    pool: &PgPool,
    ledger: Option<&dune_ledger::Ledger>,
    account: &str,
    query: &dune_registry::Query,
) -> Result<bool, Error> {
    let snapshot = client
        .snapshot(query.dune_query_id, 10, query.result_limit)
        .await?;
    let exec_id = snapshot.first_page.execution_id.clone();
    let changed = materialize(pool, &snapshot).await?;
    if changed {
        if let Some(l) = ledger {
            let cost = query.execution_estimate_microcredits as i64;
            let _ = l
                .record_observed(
                    account,
                    &query.workstream,
                    query.dune_query_id,
                    "poller",
                    Some(&exec_id),
                    cost,
                )
                .await;
        }
        let _ = prune_old_executions(pool, &query.id, 7).await;
    }
    Ok(changed)
}
