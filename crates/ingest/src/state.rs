//! The ingest cursors, in `ingestion_state`.
//!
//! `updated_at` moves on every run whether or not anything was ingested, which is what lets the
//! health check tell "running and idle" apart from "stuck": a job with nothing to do still says so.

use kittiwake_db::Pool;

#[derive(Debug, Default, Clone, sqlx::FromRow)]
pub struct State {
    pub last_epoch: Option<i32>,
    pub last_block: Option<i32>,
    pub last_id: Option<String>,
}

pub async fn get(pool: &Pool, key: &str) -> Result<State, sqlx::Error> {
    let row: Option<State> = sqlx::query_as(
        "SELECT last_epoch, last_block, last_id FROM ingestion_state WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or_default())
}

/// Move the cursor. A `None` leaves that column as it was, so a job that advances only one of the
/// three cannot blank the others by omission.
pub async fn set(
    pool: &Pool,
    key: &str,
    last_epoch: Option<i32>,
    last_block: Option<i32>,
    last_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ingestion_state (key, last_epoch, last_block, last_id, updated_at) \
         VALUES ($1, $2, $3, $4, NOW()) \
         ON CONFLICT (key) DO UPDATE SET \
         last_epoch = COALESCE(EXCLUDED.last_epoch, ingestion_state.last_epoch), \
         last_block = COALESCE(EXCLUDED.last_block, ingestion_state.last_block), \
         last_id = COALESCE(EXCLUDED.last_id, ingestion_state.last_id), \
         updated_at = NOW()",
    )
    .bind(key)
    .bind(last_epoch)
    .bind(last_block)
    .bind(last_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Say "I ran and there was nothing to do", without moving any cursor.
pub async fn touch(pool: &Pool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ingestion_state (key, updated_at) VALUES ($1, NOW()) \
         ON CONFLICT (key) DO UPDATE SET updated_at = NOW()",
    )
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}
