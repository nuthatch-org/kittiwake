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
