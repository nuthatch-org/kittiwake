//! Lodestar's own Postgres: the tables the crons fill and the request-time routes read back.
//!
//! The schema is the existing one, unchanged. That is what makes the migration reversible: with no
//! data migration, rolling back is a matter of pointing the frontend at the old handler again.

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Pool = PgPool;

/// Open the pool. Conservative by default: this box also runs Postgres itself and Grafana reads it
/// directly, so the API taking a large share of connections would be taking them from the database
/// it depends on.
pub async fn connect(url: &str, max_connections: u32) -> Result<Pool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(600))
        .test_before_acquire(true)
        .connect(url)
        .await?;
    Ok(pool)
}

pub async fn ping(pool: &Pool) -> Result<(), DbError> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Append one run to `cron_runs`. The table already exists and Grafana reads it, so the columns
/// are taken as given rather than redesigned.
pub async fn record_cron_run(
    pool: &Pool,
    step: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    duration: Duration,
    success: bool,
    rows_affected: i64,
    error: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO cron_runs (step, started_at, duration_ms, rows_affected, success, error_message) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(step)
    .bind(started_at)
    .bind(duration.as_millis() as i32)
    .bind(rows_affected as i32)
    .bind(success)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// One row of the directory, as the refresh job left it.
///
/// Numerics are read as text on purpose: `NUMERIC` holds values an `f64` does not, and a balance
/// that is subtly wrong on the way out is no better than one that was wrong going in.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichedIndexer {
    pub address: String,
    pub url: Option<String>,
    pub geo_hash: Option<String>,
    pub self_stake_grt: Option<String>,
    pub delegated_grt: Option<String>,
    pub allocated_grt: Option<String>,
    pub reward_cut: Option<String>,
    pub effective_cut: Option<String>,
    pub delegator_apr: Option<String>,
    pub delegation_capacity_pct: Option<String>,
    pub score: Option<String>,
    pub score_grade: Option<String>,
    pub allocation_count: Option<i32>,
    pub delegations_in_7d: Option<i32>,
    pub undelegations_in_7d: Option<i32>,
    pub net_flow_grt_7d: Option<String>,
    pub distinct_data_services: Option<i32>,
    pub reo_status: Option<String>,
    pub last_updated: Option<chrono::DateTime<chrono::Utc>>,
}

/// The directory rows the refresh job writes, read back for `/api/indexers-enriched`.
pub async fn enriched_indexers(pool: &Pool, limit: i64) -> Result<Vec<EnrichedIndexer>, DbError> {
    let rows = sqlx::query_as::<_, EnrichedIndexer>(
        "SELECT address, url, geo_hash, self_stake_grt::text, delegated_grt::text, \
         allocated_grt::text, reward_cut::text, effective_cut::text, delegator_apr::text, \
         delegation_capacity_pct::text, score::text, score_grade, allocation_count, \
         delegations_in_7d, undelegations_in_7d, net_flow_grt_7d::text, distinct_data_services, \
         reo_status, last_updated \
         FROM indexers ORDER BY score DESC NULLS LAST, self_stake_grt DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// Chain health: head history and per-indexer chain sets
// ---------------------------------------------------------------------------------------------

/// Create the two tables the chain-health job keeps its history in.
///
/// The rest of the schema is the platform's, untouched, which is what keeps the migration
/// reversible. These two are additive and nothing else reads or writes them, so they cost that
/// property nothing: the old handler does not know they exist and does not care.
///
/// They exist as tables rather than as process memory because the history is the signal. A chain
/// that has been dead for three days must still read as dead after a restart; keeping the stall
/// clock in memory would reset it on every deploy and hand back a clean bill of health to exactly
/// the chain that had earned the alarm.
pub async fn ensure_chain_health_tables(pool: &Pool) -> Result<(), DbError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chain_head_records ( \
           network TEXT PRIMARY KEY, \
           head BIGINT NOT NULL, \
           head_first_seen_at TIMESTAMPTZ NOT NULL, \
           last_checked_at TIMESTAMPTZ NOT NULL )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS indexer_chains ( \
           address TEXT PRIMARY KEY, \
           chains TEXT[] NOT NULL, \
           captured_at TIMESTAMPTZ NOT NULL )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `(network, head, head_first_seen_at_ms, last_checked_at_ms)`.
pub type ChainHeadRow = (String, i64, i64, i64);

pub async fn chain_head_records(pool: &Pool) -> Result<Vec<ChainHeadRow>, DbError> {
    let rows: Vec<(
        String,
        i64,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT network, head, head_first_seen_at, last_checked_at FROM chain_head_records",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(n, h, f, l)| (n, h, f.timestamp_millis(), l.timestamp_millis()))
        .collect())
}

pub async fn upsert_chain_head_records(pool: &Pool, rows: &[ChainHeadRow]) -> Result<u64, DbError> {
    let mut written = 0;
    for (network, head, first_seen_ms, checked_ms) in rows {
        let first_seen = chrono::DateTime::from_timestamp_millis(*first_seen_ms)
            .unwrap_or_else(chrono::Utc::now);
        let checked =
            chrono::DateTime::from_timestamp_millis(*checked_ms).unwrap_or_else(chrono::Utc::now);
        written += sqlx::query(
            "INSERT INTO chain_head_records (network, head, head_first_seen_at, last_checked_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (network) DO UPDATE SET head = EXCLUDED.head, \
               head_first_seen_at = EXCLUDED.head_first_seen_at, \
               last_checked_at = EXCLUDED.last_checked_at",
        )
        .bind(network)
        .bind(head)
        .bind(first_seen)
        .bind(checked)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(written)
}

/// The chain set each indexer was last seen serving.
pub async fn indexer_chains(pool: &Pool) -> Result<Vec<(String, Vec<String>)>, DbError> {
    let rows: Vec<(String, Vec<String>)> =
        sqlx::query_as("SELECT address, chains FROM indexer_chains")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

pub async fn upsert_indexer_chains(
    pool: &Pool,
    rows: &[(String, Vec<String>)],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<u64, DbError> {
    let mut written = 0;
    for (address, chains) in rows {
        written += sqlx::query(
            "INSERT INTO indexer_chains (address, chains, captured_at) VALUES ($1, $2, $3) \
             ON CONFLICT (address) DO UPDATE SET chains = EXCLUDED.chains, \
               captured_at = EXCLUDED.captured_at",
        )
        .bind(address)
        .bind(chains)
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(written)
}
