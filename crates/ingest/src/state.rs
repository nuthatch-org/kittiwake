//! The ingest cursors, in `ingestion_state`.
//!
//! `updated_at` moves on every run whether or not anything was ingested, which is what lets the
//! health check tell "running and idle" apart from "stuck": a job with nothing to do still says so.

use kittiwake_db::Pool;

/// How this service's cursors are namespaced in `ingestion_state`.
///
/// The platform's crons use the bare keys - `epochs`, `delegation_events`, `allocations`, `rav`.
/// Sharing them would be wrong twice over.
///
/// **It is unsafe.** Two writers advancing one cursor each resume from where the *other* stopped.
/// The upserts are idempotent so the tables still converge, but that depends on both
/// implementations agreeing exactly about cursor semantics, and ours does not have to: a
/// disagreement means one advances past rows neither has written, which is silent data loss in the
/// dashboard's own history.
///
/// **And it destroys the comparison.** A shadow run exists to answer "would this service write what
/// the platform writes", and with one cursor there is no way to tell which system produced a row.
///
/// Separate cursors mean both walk independently, both write the same rows, the tables converge,
/// and the two can be compared. At cutover the platform's crons stop and ours are already current.
const PREFIX: &str = "kittiwake:";

fn key(k: &str) -> String {
    format!("{PREFIX}{k}")
}

#[derive(Debug, Default, Clone, sqlx::FromRow)]
pub struct State {
    pub last_epoch: Option<i32>,
    pub last_block: Option<i32>,
    pub last_id: Option<String>,
    /// A cursor that is a **time** rather than a block, in unix seconds.
    ///
    /// `last_block` is `int4` and the RAV ingest was storing a unix timestamp in it, which works
    /// until 19 January 2038 and then does something worse than stop: `max_timestamp as i32` wraps
    /// negative in Rust, `since` clamps to zero, and every delta run silently becomes a full
    /// backfill. A job that looks busy and re-reads everything for ever. #4
    pub last_timestamp: Option<i64>,
}

pub async fn get(pool: &Pool, key: &str) -> Result<State, sqlx::Error> {
    let row = sqlx::query_as!(
        State,
        "SELECT last_epoch, last_block, last_id, last_timestamp FROM ingestion_state WHERE key = $1",
        self::key(key)
    )
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
    sqlx::query!(
        "INSERT INTO ingestion_state (key, last_epoch, last_block, last_id, updated_at) \
         VALUES ($1, $2, $3, $4, NOW()) \
         ON CONFLICT (key) DO UPDATE SET \
         last_epoch = COALESCE(EXCLUDED.last_epoch, ingestion_state.last_epoch), \
         last_block = COALESCE(EXCLUDED.last_block, ingestion_state.last_block), \
         last_id = COALESCE(EXCLUDED.last_id, ingestion_state.last_id), \
         updated_at = NOW()",
        self::key(key),
        last_epoch,
        last_block,
        last_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Move a cursor that is a time rather than a block.
///
/// Its own column, and a `bigint`, because a timestamp in a column named `last_block` is how #4
/// happened: nothing was wrong with the value, only with where it was kept, and nobody looks at a
/// column whose name already explains it.
pub async fn set_timestamp(pool: &Pool, key: &str, seconds: i64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO ingestion_state (key, last_timestamp, updated_at) VALUES ($1, $2, NOW()) \
         ON CONFLICT (key) DO UPDATE SET last_timestamp = EXCLUDED.last_timestamp, updated_at = NOW()",
        self::key(key),
        seconds
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Say "I ran and there was nothing to do", without moving any cursor.
pub async fn touch(pool: &Pool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO ingestion_state (key, updated_at) VALUES ($1, NOW()) \
         ON CONFLICT (key) DO UPDATE SET updated_at = NOW()",
        self::key(key)
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned, because the whole safety of running both systems at once rests on these keys not
    /// colliding. A change here silently re-shares the cursors.
    /// The failure #4 describes, stated in arithmetic so nobody has to take it on trust. An `i32`
    /// cannot hold a unix timestamp past January 2038, and Rust's `as` truncates rather than
    /// saturating, so the cursor does not stall - it goes *backwards*, past zero, and every delta
    /// run becomes a full backfill.
    #[test]
    fn a_timestamp_past_2038_does_not_fit_and_wraps_rather_than_stalling() {
        let jan_2038: i64 = 2_147_483_648; // i32::MAX + 1, 19 January 2038
        assert!(jan_2038 > i32::MAX as i64);
        assert!((jan_2038 as i32) < 0, "it wraps, it does not clamp");
        // Which is why the cursor moved to its own bigint column.
        let s = State {
            last_timestamp: Some(jan_2038),
            ..Default::default()
        };
        assert_eq!(s.last_timestamp, Some(jan_2038));
    }

    #[test]
    fn our_cursors_are_namespaced_away_from_the_platforms() {
        assert_eq!(key("epochs"), "kittiwake:epochs");
        assert_eq!(key("rav"), "kittiwake:rav");
        assert_ne!(key("delegation_events"), "delegation_events");
    }
}
