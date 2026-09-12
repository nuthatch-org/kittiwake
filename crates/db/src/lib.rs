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

/// How this service's runs are labelled in `cron_runs`.
///
/// Both systems write that table, and while both are running they write it under job names that are
/// otherwise identical - `refresh-chain-health` fires here and on the platform. Without a prefix,
/// "when did refresh-chain-health last succeed" answers with whichever system ran most recently and
/// gives no way to tell which, which is precisely the question a shadow run is asking.
///
/// With it, `refresh` and `kittiwake:refresh` sit side by side in the same table and can be compared
/// directly. Worth keeping after the platform crons are gone, too: a row that says where it came
/// from stays true, and a row that does not has to be dated to be understood.
pub const CRON_RUN_PREFIX: &str = "kittiwake:";

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
    sqlx::query!(
        "INSERT INTO cron_runs (step, started_at, duration_ms, rows_affected, success, error_message) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        format!("{CRON_RUN_PREFIX}{step}"),
        started_at,
        duration.as_millis() as i32,
        rows_affected as i32,
        success,
        error
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Where the two rolling APYs live.
///
/// Ours, not the platform's: `indexers` is lodestar's table and this service only reads it, so a
/// column there would be a schema change in another repository (#31). A side table joined on read
/// costs one join and stays reversible.
pub async fn ensure_rolling_apy_table(pool: &Pool) -> Result<(), DbError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS indexer_rolling_apy ( \
           indexer_address TEXT PRIMARY KEY, \
           rolling_apy_30d NUMERIC, \
           rolling_apy_90d NUMERIC, \
           computed_at TIMESTAMPTZ NOT NULL )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace every indexer's pair. Written by the refresh job in the same pass that writes the
/// directory row, so the two cannot drift by more than one run.
pub async fn write_rolling_apy(
    pool: &Pool,
    rows: &[(String, Option<f64>, Option<f64>)],
) -> Result<u64, DbError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query!(
        "INSERT INTO indexer_rolling_apy (indexer_address, rolling_apy_30d, rolling_apy_90d, computed_at) \
         SELECT * FROM UNNEST($1::text[], $2::float8[], $3::float8[]) AS t, LATERAL (SELECT NOW()) AS n(now) \
         ON CONFLICT (indexer_address) DO UPDATE SET \
         rolling_apy_30d = EXCLUDED.rolling_apy_30d, \
         rolling_apy_90d = EXCLUDED.rolling_apy_90d, \
         computed_at = NOW()",
        // Positional, and the nulls must be kept. Collapsing `None` out of either array with a
        // `filter_map` would shift every later row's APY onto the wrong indexer.
        &rows.iter().map(|r| r.0.clone()).collect::<Vec<String>>(),
        &rows.iter().map(|r| r.1).collect::<Vec<Option<f64>>>() as &[Option<f64>],
        &rows.iter().map(|r| r.2).collect::<Vec<Option<f64>>>() as &[Option<f64>]
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(done)
}

/// The indexer every directory read leaves out.
///
/// Defined here rather than beside the nest SQL because both readers need it and only one of them
/// had it: `/api/indexers` filtered this address while `/api/indexers-enriched` served it.
pub const INDEXERS_EXCLUDED: &str = "0xb43b2cccceada5292732a8c58ae134adefce09bb";

/// One row of the directory, as the refresh job left it.
///
/// Numerics are read as text on purpose: `NUMERIC` holds values an `f64` does not, and a balance
/// that is subtly wrong on the way out is no better than one that was wrong going in.
#[derive(Debug, Clone, Default, sqlx::FromRow, serde::Serialize)]
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
    pub query_fees_collected_grt: Option<String>,
    pub rewards_earned_grt: Option<String>,
    pub rolling_apy_30d: Option<String>,
    pub rolling_apy_90d: Option<String>,
    pub distinct_data_services: Option<i32>,
    pub reo_status: Option<String>,
    pub last_updated: Option<chrono::DateTime<chrono::Utc>>,
}

/// The directory read. Held as a constant so the exclusion below can be asserted without a
/// database: it was missing here for the whole of the cutover while `/api/indexers` had it.
///
/// `reward_cut` is stored in PPM and served as a percentage. The `CASE` is not only a
/// transition guard: the column genuinely holds both units, seven rows the platform wrote
/// and this service no longer updates being the PPM ones, and without it they reach the
/// dashboard as a cut of 549000%. It can go when #47 settles who owns the column.
pub const ENRICHED_INDEXERS_SQL: &str = "SELECT address, url, geo_hash, self_stake_grt::text, delegated_grt::text, \
         allocated_grt::text, \
         (CASE WHEN reward_cut > 100 THEN reward_cut / 10000.0 ELSE reward_cut END)::text \
           AS reward_cut, \
         effective_cut::text, delegator_apr::text, \
         delegation_capacity_pct::text, score::text, score_grade, allocation_count, \
         delegations_in_7d, undelegations_in_7d, net_flow_grt_7d::text, distinct_data_services, \
         reo_status, last_updated, query_fees_collected_grt::text, rewards_earned_grt::text, \
         a.rolling_apy_30d::text, a.rolling_apy_90d::text \
         FROM indexers i LEFT JOIN indexer_rolling_apy a ON a.indexer_address = i.address \
         WHERE i.address <> $2 \
         ORDER BY i.score DESC NULLS LAST, i.self_stake_grt DESC LIMIT $1";

/// The directory rows the refresh job writes, read back for `/api/indexers-enriched`.
pub async fn enriched_indexers(pool: &Pool, limit: i64) -> Result<Vec<EnrichedIndexer>, DbError> {
    let rows = sqlx::query_as::<_, EnrichedIndexer>(ENRICHED_INDEXERS_SQL)
        .bind(limit)
        .bind(INDEXERS_EXCLUDED)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// Dock: the existing Studio tables
// ---------------------------------------------------------------------------------------------

/// These are Lodestar's tables, not a new schema.  Keeping the row names in their wire form is
/// intentional: the frontend used the Postgres-shaped JSON before this service existed.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct StudioSubgraph {
    pub id: i32,
    pub owner_address: String,
    pub slug: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub deployment_id: Option<String>,
    pub network: Option<String>,
    pub published_subgraph_id: Option<String>,
    pub version_label: Option<String>,
    pub last_published_deployment_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct DeployKey {
    pub key_hash: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SyncBounty {
    pub id: i32,
    pub deployment_id: String,
    pub subgraph_id: Option<i32>,
    pub developer_address: String,
    pub amount_grt: String,
    pub message: Option<String>,
    pub status: String,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub chain_bounty_id: Option<String>,
    pub post_tx_hash: Option<String>,
}

/// The editable, owner-scoped fields of a Studio subgraph. `None` means leave the stored value
/// alone, matching the incumbent PATCH contract.
pub struct StudioSubgraphPatch<'a> {
    pub id: i32,
    pub owner: &'a str,
    pub display_name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub published_id: Option<&'a str>,
    pub version_label: Option<&'a str>,
    pub last_deployment: Option<&'a str>,
}

/// The values the developer supplies when opening a sync bounty. The database owns status and
/// timestamps, so neither can be smuggled in through this request.
pub struct NewBounty<'a> {
    pub deployment: &'a str,
    pub subgraph_id: Option<i32>,
    pub developer: &'a str,
    pub amount: &'a str,
    pub message: Option<&'a str>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub chain_id: Option<&'a str>,
    pub tx: Option<&'a str>,
}

pub async fn list_studio_subgraphs(
    pool: &Pool,
    owner: &str,
) -> Result<Vec<StudioSubgraph>, DbError> {
    Ok(sqlx::query_as(
        "SELECT * FROM studio_subgraphs WHERE owner_address = $1 ORDER BY created_at DESC",
    )
    .bind(owner.to_lowercase())
    .fetch_all(pool)
    .await?)
}

pub async fn studio_subgraph_by_slug(
    pool: &Pool,
    slug: &str,
) -> Result<Option<StudioSubgraph>, DbError> {
    Ok(
        sqlx::query_as("SELECT * FROM studio_subgraphs WHERE slug = $1")
            .bind(slug)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn create_studio_subgraph(
    pool: &Pool,
    owner: &str,
    slug: &str,
    display_name: Option<&str>,
) -> Result<StudioSubgraph, DbError> {
    Ok(sqlx::query_as("INSERT INTO studio_subgraphs (owner_address, slug, display_name) VALUES ($1, $2, $3) RETURNING *")
        .bind(owner.to_lowercase()).bind(slug).bind(display_name).fetch_one(pool).await?)
}

pub async fn update_studio_deployment(
    pool: &Pool,
    slug: &str,
    deployment: &str,
    network: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query("UPDATE studio_subgraphs SET deployment_id = $1, network = $2, updated_at = NOW() WHERE slug = $3")
        .bind(deployment).bind(network).bind(slug).execute(pool).await?;
    Ok(())
}

pub async fn patch_studio_subgraph(
    pool: &Pool,
    patch: StudioSubgraphPatch<'_>,
) -> Result<(), DbError> {
    sqlx::query("UPDATE studio_subgraphs SET display_name = COALESCE($1, display_name), description = COALESCE($2, description), published_subgraph_id = COALESCE($3, published_subgraph_id), version_label = COALESCE($4, version_label), last_published_deployment_id = COALESCE($5, last_published_deployment_id), updated_at = NOW() WHERE id = $6 AND owner_address = $7")
        .bind(patch.display_name).bind(patch.description).bind(patch.published_id).bind(patch.version_label).bind(patch.last_deployment).bind(patch.id).bind(patch.owner.to_lowercase()).execute(pool).await?;
    Ok(())
}

pub async fn delete_studio_subgraph(pool: &Pool, id: i32, owner: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM studio_subgraphs WHERE id = $1 AND owner_address = $2")
        .bind(id)
        .bind(owner.to_lowercase())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn deploy_key(pool: &Pool, owner: &str) -> Result<Option<DeployKey>, DbError> {
    Ok(sqlx::query_as("SELECT key_hash, created_at, last_used_at FROM studio_deploy_keys WHERE owner_address = $1").bind(owner.to_lowercase()).fetch_optional(pool).await?)
}

pub async fn replace_deploy_key(pool: &Pool, owner: &str, hash: &str) -> Result<(), DbError> {
    sqlx::query("INSERT INTO studio_deploy_keys (owner_address, key_hash) VALUES ($1, $2) ON CONFLICT (owner_address) DO UPDATE SET key_hash = EXCLUDED.key_hash, created_at = NOW(), last_used_at = NULL")
        .bind(owner.to_lowercase()).bind(hash).execute(pool).await?;
    Ok(())
}

pub async fn deploy_key_owner(pool: &Pool, hash: &str) -> Result<Option<String>, DbError> {
    let owner: Option<(String,)> =
        sqlx::query_as("SELECT owner_address FROM studio_deploy_keys WHERE key_hash = $1")
            .bind(hash)
            .fetch_optional(pool)
            .await?;
    if owner.is_some() {
        sqlx::query("UPDATE studio_deploy_keys SET last_used_at = NOW() WHERE key_hash = $1")
            .bind(hash)
            .execute(pool)
            .await?;
    }
    Ok(owner.map(|r| r.0))
}

pub async fn list_bounties(
    pool: &Pool,
    deployment: Option<&str>,
) -> Result<Vec<SyncBounty>, DbError> {
    let sql = if deployment.is_some() {
        "SELECT * FROM sync_bounties WHERE deployment_id = $1 ORDER BY created_at DESC"
    } else {
        "SELECT * FROM sync_bounties WHERE status IN ('open', 'claimed', 'expired') ORDER BY CASE status WHEN 'open' THEN 0 WHEN 'claimed' THEN 1 ELSE 2 END, amount_grt::numeric DESC, created_at DESC LIMIT 100"
    };
    let mut q = sqlx::query_as(sql);
    if let Some(d) = deployment {
        q = q.bind(d);
    };
    Ok(q.fetch_all(pool).await?)
}

pub async fn create_bounty(pool: &Pool, bounty: NewBounty<'_>) -> Result<SyncBounty, DbError> {
    Ok(sqlx::query_as("INSERT INTO sync_bounties (deployment_id, subgraph_id, developer_address, amount_grt, message, expires_at, chain_bounty_id, post_tx_hash) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING *")
        .bind(bounty.deployment).bind(bounty.subgraph_id).bind(bounty.developer.to_lowercase()).bind(bounty.amount).bind(bounty.message).bind(bounty.expires_at).bind(bounty.chain_id).bind(bounty.tx).fetch_one(pool).await?)
}

pub async fn bounty(pool: &Pool, id: i32) -> Result<Option<SyncBounty>, DbError> {
    Ok(sqlx::query_as("SELECT * FROM sync_bounties WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
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
    // Where a live job's finished answer is kept, so a restart does not blank a panel.
    //
    // The jobs write into this process's memory, which is what makes them fast and what removes
    // Redis from the request path. The cost is that memory does not survive a deploy, and a job on
    // a thirty-minute period leaves the route with nothing to say for up to thirty minutes. The
    // service this replaces did not have that hole, because its Redis outlived its functions. This
    // table is that durability, without putting a network hop back into the read path: the store
    // still answers every request, and this is consulted only when the store is empty.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS live_answers ( \
           key TEXT PRIMARY KEY, \
           value JSONB NOT NULL, \
           computed_at TIMESTAMPTZ NOT NULL )",
    )
    .execute(pool)
    .await?;
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
    let rows = sqlx::query!(
        "SELECT network, head, head_first_seen_at, last_checked_at FROM chain_head_records"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.network,
                r.head,
                r.head_first_seen_at.timestamp_millis(),
                r.last_checked_at.timestamp_millis(),
            )
        })
        .collect())
}

pub async fn upsert_chain_head_records(pool: &Pool, rows: &[ChainHeadRow]) -> Result<u64, DbError> {
    let mut written = 0;
    for (network, head, first_seen_ms, checked_ms) in rows {
        let first_seen = chrono::DateTime::from_timestamp_millis(*first_seen_ms)
            .unwrap_or_else(chrono::Utc::now);
        let checked =
            chrono::DateTime::from_timestamp_millis(*checked_ms).unwrap_or_else(chrono::Utc::now);
        written += sqlx::query!(
            "INSERT INTO chain_head_records (network, head, head_first_seen_at, last_checked_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (network) DO UPDATE SET head = EXCLUDED.head, \
               head_first_seen_at = EXCLUDED.head_first_seen_at, \
               last_checked_at = EXCLUDED.last_checked_at",
            network,
            head,
            first_seen,
            checked
        )
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(written)
}

/// The chain set each indexer was last seen serving.
pub async fn indexer_chains(pool: &Pool) -> Result<Vec<(String, Vec<String>)>, DbError> {
    let rows = sqlx::query!("SELECT address, chains FROM indexer_chains")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| (r.address, r.chains)).collect())
}

pub async fn upsert_indexer_chains(
    pool: &Pool,
    rows: &[(String, Vec<String>)],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<u64, DbError> {
    let mut written = 0;
    for (address, chains) in rows {
        written += sqlx::query!(
            "INSERT INTO indexer_chains (address, chains, captured_at) VALUES ($1, $2, $3) \
             ON CONFLICT (address) DO UPDATE SET chains = EXCLUDED.chains, \
               captured_at = EXCLUDED.captured_at",
            address,
            chains,
            now
        )
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(written)
}

/// Keep a live job's answer, so a restart has something to serve until the job runs again.
pub async fn put_live_answer(
    pool: &Pool,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO live_answers (key, value, computed_at) VALUES ($1, $2, NOW()) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, computed_at = NOW()",
        key,
        value
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The last answer a live job kept, and when it was computed.
pub async fn get_live_answer(
    pool: &Pool,
    key: &str,
) -> Result<Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)>, DbError> {
    let row = sqlx::query!(
        "SELECT value, computed_at FROM live_answers WHERE key = $1",
        key
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.value, r.computed_at)))
}

/// Each indexer's delegation exchange rate, one point per day, newest snapshot per day.
///
/// `DISTINCT ON` with that ordering takes the **last** snapshot of each day rather than the first,
/// which is what a day's closing rate means. Several snapshots a day is the normal case.
pub async fn delegation_exchange_rates(
    pool: &Pool,
    indexers: &[String],
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<(String, chrono::NaiveDate, f64)>, DbError> {
    if indexers.is_empty() {
        return Ok(Vec::new());
    }
    // The three `!` overrides assert non-null on columns Postgres can only describe as nullable.
    // `indexer_address` is `NOT NULL` in the DDL but sits under a `DISTINCT ON`, and the other two
    // are expressions; the `IS NOT NULL` in the `WHERE` is what makes the rate claim true. An
    // assertion the query itself does not enforce would be a decode panic in production, so the
    // filter and the override belong together.
    let rows = sqlx::query!(
        "SELECT DISTINCT ON (indexer_address, snapshot_at::date) \
           indexer_address AS \"indexer_address!\", \
           snapshot_at::date AS \"snapshot_date!\", \
           delegation_exchange_rate::float8 AS \"rate!\" \
         FROM indexer_snapshots \
         WHERE indexer_address = ANY($1) AND snapshot_at >= $2 \
           AND delegation_exchange_rate IS NOT NULL \
         ORDER BY indexer_address, snapshot_at::date, snapshot_at DESC",
        indexers,
        since
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.indexer_address, r.snapshot_date, r.rate))
        .collect())
}

/// One entry in an indexer's "why did my APR change" trail, from Lodestar's own tables.
///
/// Two sources with nothing in common but a timestamp: delegations in and out, and the cuts the
/// indexer set. Both move a delegator's return and neither explains the other, which is the point
/// of showing them on one timeline.
/// Nullability mirrors lodestar's `scripts/schema.sql`, which owns these tables: a tuple that
/// asserts more than the DDL does fails at decode time, in production, as #24 did.
///
/// `(event_type, delegator, tokens_grt, timestamp)`
///
/// `delegator` was `Option<String>` until the compile-time check contradicted it: the DDL has
/// `delegator TEXT NOT NULL`, so the tuple was claiming a nullability the table does not have. It
/// decoded fine - an `Option` accepts a non-null - which is exactly why nothing found it until
/// something read the schema. The other three are genuinely nullable and stay so.
pub type DelegationEventRow = (
    Option<String>,
    String,
    Option<f64>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// `(param_name, old_value, new_value, created_at)`
pub type ParameterChangeRow = (
    String,
    Option<f64>,
    Option<f64>,
    Option<chrono::DateTime<chrono::Utc>>,
);

pub struct ProvenanceRows {
    pub delegations: Vec<DelegationEventRow>,
    pub params: Vec<ParameterChangeRow>,
}

pub async fn provenance_rows(pool: &Pool, indexer: &str) -> Result<ProvenanceRows, DbError> {
    let delegations: Vec<DelegationEventRow> = sqlx::query!(
        "SELECT event_type, delegator, tokens_grt::float8 AS tokens_grt, timestamp \
         FROM delegation_events WHERE indexer = $1 ORDER BY timestamp DESC LIMIT 30",
        indexer
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.event_type, r.delegator, r.tokens_grt, r.timestamp))
    .collect();

    // `DISTINCT ON` over the whole tuple, because the same change can be recorded in several
    // epochs and the trail should show it once.
    let params: Vec<ParameterChangeRow> = sqlx::query!(
        "SELECT DISTINCT ON (param_name, old_value, new_value, epoch) \
           param_name AS \"param_name!\", old_value::float8 AS old_value, \
           new_value::float8 AS new_value, created_at \
         FROM parameter_changes WHERE indexer_address = $1 \
         ORDER BY param_name, old_value, new_value, epoch, created_at DESC",
        indexer
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.param_name, r.old_value, r.new_value, r.created_at))
    .collect();

    Ok(ProvenanceRows {
        delegations,
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prefix is the only thing telling two systems' rows apart in a shared table, so it is
    /// pinned rather than left to whoever edits the constant next.
    #[test]
    fn a_recorded_run_is_labelled_with_its_system() {
        assert_eq!(CRON_RUN_PREFIX, "kittiwake:");
        assert_eq!(
            format!("{CRON_RUN_PREFIX}refresh-chain-health"),
            "kittiwake:refresh-chain-health"
        );
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    /// `/api/indexers` filtered this address and `/api/indexers-enriched` did not, so the enriched
    /// directory served an indexer the dashboard deliberately hides. Both readers take the
    /// constant from here now; this pins the clause that was missing.
    #[test]
    fn the_directory_read_excludes_the_blocked_indexer() {
        assert!(
            ENRICHED_INDEXERS_SQL.contains("WHERE i.address <> $2"),
            "{ENRICHED_INDEXERS_SQL}"
        );
        assert_eq!(INDEXERS_EXCLUDED.len(), 42);
        assert!(INDEXERS_EXCLUDED.starts_with("0x"));
    }

    /// The column holds PPM and the wire carries a percentage, and for now it holds both units at
    /// once: seven rows the platform wrote and this service no longer updates are still in PPM.
    /// Without the guard they reach the dashboard as a cut of 549000%.
    #[test]
    fn a_cut_reaches_the_wire_as_a_percentage_whichever_unit_it_is_stored_in() {
        assert!(
            ENRICHED_INDEXERS_SQL.contains("CASE WHEN reward_cut > 100"),
            "{ENRICHED_INDEXERS_SQL}"
        );
        assert!(ENRICHED_INDEXERS_SQL.contains("reward_cut / 10000.0"));
        // Named, or the row arrives as an unlabelled column and serde drops it.
        assert!(ENRICHED_INDEXERS_SQL.contains("AS reward_cut"));
    }

    /// The names lodestar destructures off the wire. A rename here is a breaking change there and
    /// nothing else in this repository would notice: `enriched-normalise.ts` reads these exact
    /// keys, and the cutover already proved that a silent rename renders a table of dashes.
    #[test]
    fn the_row_serialises_under_the_names_the_frontend_reads() {
        let v = serde_json::to_value(EnrichedIndexer::default()).unwrap();
        for key in [
            "address",
            "selfStakeGrt",
            "netFlowGrt7d",
            "queryFeesCollectedGrt",
            "rewardsEarnedGrt",
            "rollingApy30d",
            "rollingApy90d",
        ] {
            assert!(v.get(key).is_some(), "{key} is not on the wire: {v}");
        }
    }

    /// The refresh job writes these and this read did not ask for them, which is the whole of why
    /// Fees rendered as a dash on every row of the directory.
    #[test]
    fn the_columns_the_refresh_job_writes_are_actually_read_back() {
        for column in [
            "query_fees_collected_grt",
            "rewards_earned_grt",
            "net_flow_grt_7d",
            "delegation_capacity_pct",
            "rolling_apy_30d",
            "rolling_apy_90d",
        ] {
            assert!(
                ENRICHED_INDEXERS_SQL.contains(column),
                "{column} is written by the refresh job and never selected"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sync bounties
// ---------------------------------------------------------------------------------------------

/// `(id, chain_bounty_id, status, claimed_by)` for every bounty worth asking the chain about.
pub type ReconcilableBounty = (i32, String, String, Option<String>);

/// The bounties whose cached status could still be wrong.
///
/// `cancelled` and `expired` are terminal on-chain and are left out: nothing moves them back, so
/// asking about them every ten minutes would be RPC calls spent to confirm what cannot change.
/// `claimed` stays in because the winner recorded against it may be the studio session's address
/// rather than the indexer's.
pub async fn reconcilable_bounties(pool: &Pool) -> Result<Vec<ReconcilableBounty>, DbError> {
    let rows = sqlx::query_as(
        "SELECT id, chain_bounty_id, status, claimed_by FROM sync_bounties \
         WHERE status IN ('open', 'claimed') AND chain_bounty_id IS NOT NULL \
         ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Bring one cached bounty into line with the chain.
///
/// `claimed_at` is set only on the transition into `claimed` and cleared otherwise, which is what
/// the handler being replaced did. It is our own observation time rather than the block's: the
/// contract records no claim timestamp, and inventing one from the block would be a more precise
/// lie than the honest "this is when we noticed".
pub async fn update_bounty_status(
    pool: &Pool,
    id: i32,
    status: &str,
    claimed_by: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE sync_bounties SET status = $2, claimed_by = $3, \
         claimed_at = CASE WHEN $2 = 'claimed' THEN NOW() ELSE NULL END WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(claimed_by)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
/// Every indexer holding an allocation, and how much.
///
/// The quality score does not come from here: it is Foghorn's, and joining them is the caller's
/// job precisely so an indexer missing from Foghorn stays missing rather than becoming a zero.
pub async fn allocating_indexers(pool: &Pool) -> Result<Vec<(String, f64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT address, allocated_grt::float8 FROM indexers WHERE allocated_grt > 0",
    )
    .fetch_all(pool)
    .await?)
}

// ---------------------------------------------------------------------------------------------
// The long tail: disputes, the parameter trail, and indexer revenue
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct IndexerDispute {
    pub id: String,
    pub dispute_type: Option<String>,
    pub fisherman: Option<String>,
    pub allocation_id: Option<String>,
    pub deployment_id: Option<String>,
    pub status: Option<String>,
    /// Held as text for the same reason every other amount is: these are `numeric` in the table and
    /// an `f64` would round the ones somebody is trying to tell apart.
    pub tokens_slashed_grt: Option<String>,
    pub tokens_burned_grt: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn indexer_disputes(pool: &Pool, address: &str) -> Result<Vec<IndexerDispute>, DbError> {
    Ok(sqlx::query_as(
        "SELECT id, dispute_type, fisherman, allocation_id, deployment_id, status, \
         tokens_slashed_grt::text, tokens_burned_grt::text, created_at, closed_at \
         FROM disputes WHERE indexer_address = $1 \
         ORDER BY created_at DESC NULLS LAST LIMIT 100",
    )
    .bind(address.to_lowercase())
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ParameterChange {
    pub param_name: String,
    pub old_value: Option<f64>,
    pub new_value: f64,
    pub epoch: Option<i32>,
    pub detected_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The parameter trail, deduplicated.
///
/// `DISTINCT ON` over the change itself rather than over the row: the refresh job records what it
/// observed each pass, so one cut change seen across forty passes is forty rows and one event.
pub async fn parameter_history(
    pool: &Pool,
    address: &str,
) -> Result<Vec<ParameterChange>, DbError> {
    Ok(sqlx::query_as(
        "SELECT param_name, old_value::float8, new_value::float8, epoch, detected_at FROM ( \
           SELECT DISTINCT ON (param_name, old_value, new_value, epoch) \
             param_name, old_value, new_value, epoch, created_at AS detected_at \
           FROM parameter_changes \
           WHERE indexer_address = $1 AND old_value IS DISTINCT FROM new_value \
           ORDER BY param_name, old_value, new_value, epoch, created_at DESC \
         ) deduped ORDER BY detected_at DESC LIMIT 100",
    )
    .bind(address.to_lowercase())
    .fetch_all(pool)
    .await?)
}

/// One day's total, as the revenue fold wants it.
///
/// Cast to `float8` in the statement rather than carried as text, and this is the one place that is
/// right: the column is GRT rather than wei, the figure is charted rather than compared, and the
/// incumbent's shape is a JSON number. A string here would be a parity break.
pub async fn revenue_daily(
    pool: &Pool,
    address: &str,
    window_days: i32,
) -> Result<(Vec<(String, f64)>, Vec<(String, f64)>), DbError> {
    let address = address.to_lowercase();
    let rav: Vec<(String, f64)> = sqlx::query_as(
        "SELECT to_char(date_trunc('day', collected_at), 'YYYY-MM-DD'), \
         COALESCE(sum(tokens_grt), 0)::float8 \
         FROM rav_redemptions \
         WHERE indexer_address = $1 AND collected_at >= now() - ($2::int * INTERVAL '1 day') \
         GROUP BY 1 ORDER BY 1",
    )
    .bind(&address)
    .bind(window_days)
    .fetch_all(pool)
    .await?;
    let rewards: Vec<(String, f64)> = sqlx::query_as(
        "SELECT to_char(date_trunc('day', closed_at), 'YYYY-MM-DD'), \
         COALESCE(sum(indexing_rewards_grt), 0)::float8 \
         FROM allocations \
         WHERE indexer_address = $1 AND closed_at IS NOT NULL \
         AND closed_at >= now() - ($2::int * INTERVAL '1 day') \
         GROUP BY 1 ORDER BY 1",
    )
    .bind(&address)
    .bind(window_days)
    .fetch_all(pool)
    .await?;
    Ok((rav, rewards))
}

pub async fn revenue_by_deployment(
    pool: &Pool,
    address: &str,
    window_days: i32,
) -> Result<(Vec<(Option<String>, f64)>, Vec<(Option<String>, f64)>), DbError> {
    let address = address.to_lowercase();
    let rav: Vec<(Option<String>, f64)> = sqlx::query_as(
        "SELECT deployment_id, COALESCE(sum(tokens_grt), 0)::float8 \
         FROM rav_redemptions \
         WHERE indexer_address = $1 AND collected_at >= now() - ($2::int * INTERVAL '1 day') \
         GROUP BY deployment_id",
    )
    .bind(&address)
    .bind(window_days)
    .fetch_all(pool)
    .await?;
    let rewards: Vec<(Option<String>, f64)> = sqlx::query_as(
        "SELECT deployment_id, COALESCE(sum(indexing_rewards_grt), 0)::float8 \
         FROM allocations \
         WHERE indexer_address = $1 AND closed_at IS NOT NULL \
         AND closed_at >= now() - ($2::int * INTERVAL '1 day') \
         GROUP BY deployment_id",
    )
    .bind(&address)
    .bind(window_days)
    .fetch_all(pool)
    .await?;
    Ok((rav, rewards))
}

/// Every table this service's own statements name, worked out once.
///
/// `schema_gaps` checks against this rather than against everything `db/schema.sql` declares. That
/// file describes the whole database, including tables only the platform touches - `community_votes`
/// from a feature that has been deleted, `indexer_scores` - and reporting
/// those as gaps asks an operator to grant privileges nothing here will ever use. A check that
/// lists things nobody intends to fix teaches people to skip reading it.
fn tables_we_use() -> &'static std::collections::HashSet<String> {
    static USED: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    USED.get_or_init(|| SOURCES.iter().flat_map(|(_, src)| tables_in(src)).collect())
}

const SOURCES: &[(&str, &str)] = &[
    ("crates/db/src/lib.rs", include_str!("lib.rs")),
    (
        "crates/ingest/src/lib.rs",
        include_str!("../../ingest/src/lib.rs"),
    ),
    (
        "crates/ingest/src/rav.rs",
        include_str!("../../ingest/src/rav.rs"),
    ),
    (
        "crates/ingest/src/refresh.rs",
        include_str!("../../ingest/src/refresh.rs"),
    ),
    (
        "crates/ingest/src/state.rs",
        include_str!("../../ingest/src/state.rs"),
    ),
    (
        "crates/ingest/src/live.rs",
        include_str!("../../ingest/src/live.rs"),
    ),
    (
        "crates/ext/src/lib.rs",
        include_str!("../../ext/src/lib.rs"),
    ),
    (
        "crates/read/src/search.rs",
        include_str!("../../read/src/search.rs"),
    ),
];
/// Words that follow `FROM` and are not Postgres tables: SQL's own, and the short aliases
/// these statements use.
const NOT_A_TABLE: &[&str] = &[
    "unnest", "lateral", "select", "insert", "update", "delete", "set", "from", "where", "t", "n",
    "e", "f", "o", "a", "c", "i", "d",
];
/// A nest table rather than a Postgres one. The nest owns its own schema and this file does
/// not describe it: `lodestar_*` are the mounted views, and `__` is nuthatch's separator
/// between a contract and its event.
fn is_nest_table(t: &str) -> bool {
    t.starts_with("lodestar_")
        || t.contains("__")
        || t == "subgraph_current"
        || t == "deployment_subgraphs"
}
fn tables_in(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    // This module is excluded from its own scan: its prose contains the words it looks for,
    // and a check that trips over its own explanation is no use to anybody. Only this one -
    // truncating at the first `#[cfg(test)]` would cut off real statements that happen to sit
    // below an earlier test module, which is how the first version of this managed to find
    // nothing in half the file.
    let mut rest = match src.find("mod schema_coverage {") {
        Some(i) => &src[..i],
        None => src,
    };
    while let Some(i) = rest.find("sqlx::query") {
        rest = &rest[i + "sqlx::query".len()..];
        // Back the window off to a word boundary. Cutting mid-identifier invents a table: a
        // window ending at `UPDATE scu` reports `scu` as missing from the schema, which is a
        // failure about this function rather than about the database.
        let cut = rest.len().min(4000);
        let window = &rest[..cut];
        let window = match window.rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            Some(i) if cut == 4000 => &window[..=i],
            _ => window,
        };
        // The statement, reassembled from the literals in this call. Only literals that look
        // like SQL, so a doc comment sitting inside the window cannot contribute a "table".
        let mut sql = String::new();
        for (n, part) in window.split('"').enumerate() {
            if n % 2 == 1 {
                let upper = part.to_uppercase();
                if ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "FROM"]
                    .iter()
                    .any(|k| upper.contains(k))
                {
                    sql.push(' ');
                    sql.push_str(part);
                }
            }
        }
        let words: Vec<String> = sql
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect();
        for (n, w) in words.iter().enumerate() {
            // `a IS DISTINCT FROM b` is an operator, not a table reference, and the `b` is a
            // column. Reading it as a table reported `new_value` as missing from the schema.
            if n > 0 && words[n - 1] == "distinct" {
                continue;
            }
            if matches!(w.as_str(), "from" | "into" | "update" | "join")
                && let Some(next) = words.get(n + 1)
                && !NOT_A_TABLE.contains(&next.as_str())
                && !is_nest_table(next)
            {
                out.push(next.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod schema_coverage {
    //! Every Postgres table this service names must exist in `db/schema.sql`.
    //!
    //! `ipfs_metadata` was missing from that file for the whole of the day the schema moved here
    //! (#31), and nothing noticed. It was missed because it had never been in the file the move
    //! copied - it lives in lodestar's `migrations/` directory - and the compile-time query check
    //! could not catch it, because every statement touching that table is a runtime `sqlx::query`
    //! rather than a macro. Two absences agreeing is not a confirmation.
    //!
    //! So this reads the source, pulls the table names out of every `sqlx::query` literal, and
    //! checks each against the schema. It covers exactly the statements the macro cannot.

    use super::{SCHEMA_SQL as SCHEMA, SOURCES, tables_in};

    #[test]
    fn every_table_a_statement_names_is_in_the_schema() {
        let declared: Vec<String> = SCHEMA
            .split("CREATE TABLE IF NOT EXISTS ")
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .map(|s| s.trim_end_matches('(').to_lowercase())
            .collect();
        assert!(
            declared.len() > 15,
            "the schema did not parse: {declared:?}"
        );

        // If the extraction stops finding statements, every assertion below passes vacuously and
        // the check quietly becomes decoration.
        let found: usize = SOURCES.iter().map(|(_, src)| tables_in(src).len()).sum();
        assert!(
            found > 20,
            "only {found} tables extracted; the scan is broken"
        );

        let mut missing = Vec::new();
        for (path, src) in SOURCES {
            for t in tables_in(src) {
                if !declared.contains(&t) {
                    missing.push(format!("{t}  ({path})"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these tables are used and not declared in db/schema.sql:\n  {}\n\n\
             Add them, or the next person to build a database from that file gets one that cannot \
             answer.",
            missing.join("\n  ")
        );
    }

    /// If the extraction breaks, the assertion above passes vacuously.
    #[test]
    fn the_extraction_finds_the_tables_we_know_are_there() {
        let found = tables_in(SOURCES[0].1);
        for expected in ["cron_runs", "indexer_rolling_apy", "sync_bounties"] {
            assert!(
                found.contains(&expected.to_string()),
                "did not find {expected} in {found:?}"
            );
        }
    }
}
// ---------------------------------------------------------------------------------------------
// graph-support: the community triage archive

/// One issue from nightswatchhq/graph-support, as the mirror job last saw it.
///
/// `labels` is the flat list of names rather than anything parsed. The taxonomy in that repo's
/// `TRIAGE.md` is not exhaustive - `area/governance` and `kind/question` are both in use and in
/// neither published list - so a mirror that understood the labels would drop the ones it had not
/// been told about. The dashboard groups them; this stores them.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSupportIssue {
    pub number: i32,
    pub title: String,
    /// The opening post, as Markdown. Empty for an issue filed with no body at all.
    pub body: String,
    pub url: String,
    pub state: String,
    pub labels: Vec<String>,
    pub comments: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Create the mirror table.
///
/// Additive and ours alone: nothing the platform runs reads or writes it, which is what lets it
/// live here rather than in the schema the migration must be able to roll back to.
pub async fn ensure_graph_support_table(pool: &Pool) -> Result<(), DbError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS graph_support_issues ( \
           number      INTEGER PRIMARY KEY, \
           title       TEXT NOT NULL, \
           url         TEXT NOT NULL, \
           state       TEXT NOT NULL, \
           labels      TEXT[] NOT NULL DEFAULT '{}', \
           comments    INTEGER NOT NULL DEFAULT 0, \
           created_at  TIMESTAMPTZ NOT NULL, \
           updated_at  TIMESTAMPTZ NOT NULL, \
           ingested_at TIMESTAMPTZ NOT NULL DEFAULT NOW() \
         )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_graph_support_updated \
         ON graph_support_issues(updated_at DESC)",
    )
    .execute(pool)
    .await?;

    // Added after the table shipped, so it arrives as an alter rather than in the CREATE above.
    // A default of empty string rather than null: an issue filed with no body is a real thing and
    // reads as empty, where null would invite a downstream `unwrap` on the difference.
    sqlx::query(
        "ALTER TABLE graph_support_issues ADD COLUMN IF NOT EXISTS body TEXT NOT NULL DEFAULT ''",
    )
    .execute(pool)
    .await?;

    // The thread under each issue. The write-ups this archive exists for are frequently in the
    // comments rather than the opening post: the reporter describes a symptom and the mechanism
    // gets worked out below it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS graph_support_comments ( \
           id           BIGINT PRIMARY KEY, \
           issue_number INTEGER NOT NULL, \
           author       TEXT NOT NULL, \
           body         TEXT NOT NULL, \
           url          TEXT NOT NULL, \
           created_at   TIMESTAMPTZ NOT NULL, \
           updated_at   TIMESTAMPTZ NOT NULL \
         )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_graph_support_comments_issue \
         ON graph_support_comments(issue_number, created_at)",
    )
    .execute(pool)
    .await?;

    // The issue forms, so the dashboard's own compose form is generated from the repository's
    // templates rather than from a copy of them that goes stale the first time somebody edits one.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS graph_support_templates ( \
           file       TEXT PRIMARY KEY, \
           yaml       TEXT NOT NULL, \
           fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW() \
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Every issue template, by filename.
pub async fn graph_support_templates(pool: &Pool) -> Result<Vec<GraphSupportTemplate>, DbError> {
    let rows = sqlx::query_as::<_, GraphSupportTemplate>(
        "SELECT file, yaml, fetched_at FROM graph_support_templates ORDER BY file",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One issue template, stored as the YAML the repository holds.
///
/// Verbatim on purpose. GitHub's issue-form schema is GitHub's, and a mirror that parsed it would
/// have to be taught every field type they add. This stores bytes; the dashboard interprets them.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSupportTemplate {
    /// The filename, which is also what GitHub's `template` query parameter takes.
    pub file: String,
    pub yaml: String,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// One comment on an issue.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSupportComment {
    pub id: i64,
    pub issue_number: i32,
    pub author: String,
    pub body: String,
    pub url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The whole archive, newest activity first.
///
/// Unpaginated on purpose. It is thirty-three rows and grows by a handful a month; a limit here
/// would be a truncation the page could not tell from the archive being that size.
pub async fn graph_support_issues(pool: &Pool) -> Result<Vec<GraphSupportIssue>, DbError> {
    // `body` is deliberately not selected here and comes back empty. The index renders titles and
    // labels; carrying thirty-three bodies of seven to ten thousand characters each would be most
    // of a megabyte on a page that shows none of it.
    let rows = sqlx::query_as::<_, GraphSupportIssue>(
        "SELECT number, title, '' AS body, url, state, labels, comments, created_at, updated_at \
         FROM graph_support_issues ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One issue with its body, for the page dedicated to it.
pub async fn graph_support_issue(
    pool: &Pool,
    number: i32,
) -> Result<Option<GraphSupportIssue>, DbError> {
    let row = sqlx::query_as::<_, GraphSupportIssue>(
        "SELECT number, title, body, url, state, labels, comments, created_at, updated_at \
         FROM graph_support_issues WHERE number = $1",
    )
    .bind(number)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The thread under one issue, oldest first, which is the order it was worked out in.
pub async fn graph_support_thread(
    pool: &Pool,
    number: i32,
) -> Result<Vec<GraphSupportComment>, DbError> {
    let rows = sqlx::query_as::<_, GraphSupportComment>(
        "SELECT id, issue_number, author, body, url, created_at, updated_at \
         FROM graph_support_comments WHERE issue_number = $1 ORDER BY created_at",
    )
    .bind(number)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// When the mirror last ran, whether or not it wrote anything.
///
/// Read from the rows rather than from `ingestion_state` so the answer describes the data being
/// served. A cursor that moved while the write failed would report freshness the table does not
/// have.
pub async fn graph_support_ingested_at(
    pool: &Pool,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, DbError> {
    let row: Option<(chrono::DateTime<chrono::Utc>,)> =
        sqlx::query_as("SELECT MAX(ingested_at) FROM graph_support_issues")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0))
}

// ── Saying what is wrong, without being asked twice ──────────────────────────

/// The schema this binary was built against, which is the declaration `schema_gaps` checks.
const SCHEMA_SQL: &str = include_str!("../../../db/schema.sql");

/// Every `(table, column)` `db/schema.sql` declares.
///
/// Covers the two shapes that file uses: a column list inside `CREATE TABLE`, and a later
/// `ALTER TABLE ... ADD COLUMN`. Table-level constraints in a `CREATE TABLE` body are not columns
/// and are skipped by keyword; anything else on its own line is taken as a column name, which is
/// true of this file and would be worth revisiting if it stopped being.
fn declared_columns(sql: &str) -> Vec<(String, String)> {
    const CONSTRAINTS: &[&str] = &[
        "primary",
        "unique",
        "foreign",
        "check",
        "constraint",
        "exclude",
        "like",
    ];

    let mut out = Vec::new();
    let mut current: Option<String> = None;

    for raw in sql.lines() {
        let line = raw.split("--").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();

        if let Some(rest) = lower.strip_prefix("create table if not exists ") {
            current = rest
                .split_whitespace()
                .next()
                .map(|t| t.trim_matches('(').to_string());
            continue;
        }

        if let Some(rest) = lower.strip_prefix("alter table ") {
            // `ALTER TABLE x ADD COLUMN IF NOT EXISTS y TYPE;` and nothing else here declares one.
            let mut words = rest.split_whitespace();
            let table = words.next().unwrap_or("").to_string();
            let tail: Vec<&str> = words.collect();
            if let Some(i) = tail.iter().position(|w| *w == "column") {
                let after: Vec<&str> = tail[i + 1..]
                    .iter()
                    .copied()
                    .skip_while(|w| matches!(*w, "if" | "not" | "exists"))
                    .collect();
                if let Some(col) = after.first() {
                    out.push((table, col.trim_matches(&[',', ';'][..]).to_string()));
                }
            }
            continue;
        }

        let Some(table) = current.clone() else {
            continue;
        };

        if line.starts_with(')') {
            current = None;
            continue;
        }

        let Some(word) = lower.split_whitespace().next() else {
            continue;
        };
        if CONSTRAINTS.contains(&word) {
            continue;
        }
        out.push((table, word.trim_matches(&[',', '('][..]).to_string()));
    }

    out
}

/// A job whose recent runs have all failed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailingJob {
    /// The step name as `cron_runs` holds it, prefix included.
    pub step: String,
    /// How many consecutive failures, counting back from the most recent run.
    pub consecutive_failures: i64,
    /// When it last succeeded, if it ever has within the window looked at.
    pub last_success: Option<chrono::DateTime<chrono::Utc>>,
    /// The most recent error, which is usually the same one repeated.
    pub last_error: Option<String>,
}

/// Jobs that have failed every run for at least `min_failures`, most broken first.
///
/// On 2026-09-10 five ingest jobs failed every attempt from 09:01 to 15:37 on a column the live
/// database did not have. Every one of those failures was written to this table as it happened, and
/// the only thing that eventually noticed was a staleness threshold six and a half hours later.
/// The rows were always here; nothing read them.
///
/// Consecutive rather than a rate, because a rate hides the difference between a job that fails one
/// run in three and a job that has not worked since breakfast. Skipped runs are not failures and
/// are excluded: an overlapping run says the previous one is slow, not broken.
pub async fn failing_jobs(pool: &Pool, min_failures: i64) -> Result<Vec<FailingJob>, DbError> {
    // The window bounds the scan; a job silent for longer than this is a staleness question rather
    // than a failure question, and the ingestion ages already answer that one.
    let rows = sqlx::query_as::<_, (String, i64, Option<chrono::DateTime<chrono::Utc>>, Option<String>)>(
        "WITH recent AS ( \
           SELECT step, started_at, success, error_message, \
                  ROW_NUMBER() OVER (PARTITION BY step ORDER BY started_at DESC) AS n \
             FROM cron_runs \
            WHERE started_at > NOW() - INTERVAL '24 hours' \
              AND error_message IS DISTINCT FROM 'previous run still in flight' \
         ), \
         since_success AS ( \
           SELECT step, MIN(n) AS first_ok FROM recent WHERE success GROUP BY step \
         ) \
         SELECT r.step, \
                COALESCE(s.first_ok - 1, MAX(r.n)) AS consecutive_failures, \
                (SELECT MAX(started_at) FROM recent x WHERE x.step = r.step AND x.success) AS last_success, \
                (SELECT error_message FROM recent y WHERE y.step = r.step AND y.n = 1) AS last_error \
           FROM recent r LEFT JOIN since_success s ON s.step = r.step \
          GROUP BY r.step, s.first_ok \
         HAVING COALESCE(s.first_ok - 1, MAX(r.n)) >= $1 \
          ORDER BY consecutive_failures DESC",
    )
    .bind(min_failures)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(step, consecutive_failures, last_success, last_error)| FailingJob {
                step,
                consecutive_failures,
                last_success,
                last_error,
            },
        )
        .collect())
}

/// One probe round of `/api/indexing-status/{hash}`, as it goes into the store.
#[derive(Debug, Clone)]
pub struct ServabilityRound<'a> {
    pub deployment_hash: &'a str,
    pub probed_at: chrono::DateTime<chrono::Utc>,
    pub serving_operators: i32,
    pub serving_indexers: i32,
    pub gateway_verdict: Option<&'a str>,
    /// The verdict with `probes` inside it, which is what tells an edge block from a timeout after
    /// the fact. Passed as a value, never as a pre-serialised string: a string handed to a `jsonb`
    /// parameter lands as a JSON *string* inside the column and `verdict_json->'probes'` reads
    /// null on every row. lodestar#61 shipped exactly that.
    pub verdict_json: serde_json::Value,
}

/// The short history a round summary is read back as.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundSummary {
    pub probed_at: chrono::DateTime<chrono::Utc>,
    pub serving_operators: i64,
    pub serving_indexers: i64,
    pub gateway_verdict: Option<String>,
}

/// Append one round.
///
/// "Effectively dead" is derived from the last K rounds and never from one, so this table is what
/// makes that verdict possible at all. Without it the persistence rule sees a single round and
/// renders `rechecking` for ever, which is safe and useless.
pub async fn record_servability_round(
    pool: &Pool,
    round: ServabilityRound<'_>,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO servability_rounds \
           (deployment_hash, probed_at, serving_operator_count, serving_indexer_count, \
            gateway_verdict, verdict_json) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(round.deployment_hash)
    .bind(round.probed_at)
    .bind(round.serving_operators)
    .bind(round.serving_indexers)
    .bind(round.gateway_verdict)
    .bind(round.verdict_json)
    .execute(pool)
    .await?;
    Ok(())
}

/// The newest `limit` rounds for a deployment, **oldest first**.
///
/// The query takes them newest-first because that is what the index is for; the reversal is here
/// rather than left to the caller, because `apply_persistence` reads a history in time order and
/// handing it a reversed one silently inverts "recovering" and "going dead".
pub async fn recent_servability_rounds(
    pool: &Pool,
    deployment_hash: &str,
    limit: i64,
) -> Result<Vec<RoundSummary>, DbError> {
    let rows = sqlx::query_as::<_, (chrono::DateTime<chrono::Utc>, i32, i32, Option<String>)>(
        "SELECT probed_at, serving_operator_count, serving_indexer_count, gateway_verdict \
           FROM servability_rounds \
          WHERE deployment_hash = $1 \
          ORDER BY probed_at DESC \
          LIMIT $2",
    )
    .bind(deployment_hash)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .rev()
        .map(
            |(probed_at, operators, indexers, gateway_verdict)| RoundSummary {
                probed_at,
                serving_operators: operators as i64,
                serving_indexers: indexers as i64,
                gateway_verdict,
            },
        )
        .collect())
}

/// A job that is scheduled and has stopped firing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SilentJob {
    pub step: String,
    /// How often it is supposed to fire.
    pub period_minutes: i64,
    /// Since its last run, or since this process started when it has never run at all.
    pub silent_for_minutes: i64,
    /// The window it overran.
    pub allowed_minutes: i64,
    /// True when `cron_runs` has no row for it under any process.
    pub never_ran: bool,
}

/// Three missed ticks, and never less than this.
///
/// A one-minute warmer that has not run for three minutes is not news; it is a tick landing late
/// under load, and a check that fires on those is one people learn to scroll past. Twenty minutes
/// is short enough to catch a stopped job within a working morning and long enough that nothing
/// healthy trips it.
const SILENCE_FLOOR_MINUTES: i64 = 20;

fn silence_window_minutes(period_minutes: i64) -> i64 {
    (period_minutes * 3).max(SILENCE_FLOOR_MINUTES)
}

/// The whole judgement, with the database left out of it so it can be tested.
///
/// `quiet_minutes` is `None` when `cron_runs` has no row for the step under any process, and the
/// silence is then measured from start-up: after a restart, or against a fresh database, every job
/// has legitimately never run and judging it on that would report the entire schedule as stopped
/// the moment the service came up.
fn judge_silence(
    name: &str,
    period_secs: i64,
    quiet_minutes: Option<i64>,
    uptime_minutes: i64,
) -> Option<SilentJob> {
    // A sub-minute period would floor to zero and make every window the floor anyway; one minute
    // is the finest cadence anything here runs at.
    let period_minutes = (period_secs / 60).max(1);
    let allowed = silence_window_minutes(period_minutes);
    let (silent_for, never_ran) = match quiet_minutes {
        Some(q) => (q, false),
        None => (uptime_minutes, true),
    };
    (silent_for > allowed).then(|| SilentJob {
        step: format!("{CRON_RUN_PREFIX}{name}"),
        period_minutes,
        silent_for_minutes: silent_for,
        allowed_minutes: allowed,
        never_ran,
    })
}

/// Scheduled jobs that have not run for far longer than their own period.
///
/// `failing_jobs` sees a job that runs and errors. It cannot see one that has stopped running,
/// because a job that fires no longer writes no rows to argue with, and the two failures need
/// looking for in opposite directions. Until now the second was caught in the other repository, by
/// a hand-written table of thresholds describing this scheduler from a distance. The period comes
/// from the running scheduler instead, so there is nothing left to keep in step.
///
/// `uptime` covers the start-up case honestly: after a restart, or against a fresh database, every
/// job has legitimately never run, and a job is only judged once this process has been up long
/// enough for it to have fired.
pub async fn silent_jobs(
    pool: &Pool,
    jobs: &[(String, i64)],
    uptime: Duration,
) -> Result<Vec<SilentJob>, DbError> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }

    let steps: Vec<String> = jobs
        .iter()
        .map(|(name, _)| format!("{CRON_RUN_PREFIX}{name}"))
        .collect();

    // Every run ever, not a recent window: a job silent for a week has nothing inside one.
    let rows = sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT step,                 (EXTRACT(EPOCH FROM (NOW() - MAX(started_at))) / 60)::bigint AS quiet_minutes            FROM cron_runs           WHERE step = ANY($1)           GROUP BY step",
    )
    .bind(&steps)
    .fetch_all(pool)
    .await?;

    let last_run: std::collections::HashMap<String, i64> = rows
        .into_iter()
        .filter_map(|(step, quiet)| quiet.map(|q| (step, q)))
        .collect();

    let uptime_minutes = (uptime.as_secs() / 60) as i64;
    let mut out = Vec::new();

    for ((name, period_secs), step) in jobs.iter().zip(steps.iter()) {
        if let Some(j) = judge_silence(
            name,
            *period_secs,
            last_run.get(step).copied(),
            uptime_minutes,
        ) {
            out.push(j);
        }
    }

    out.sort_by_key(|j| std::cmp::Reverse(j.silent_for_minutes));
    Ok(out)
}

#[cfg(test)]
mod silence_tests {
    use super::*;

    /// The case this exists for: a job whose last run is far older than its own cadence.
    ///
    /// On 2026-09-10 five ingest jobs stopped producing anything for six and a half hours and the
    /// only thing that eventually noticed was a threshold living in another repository. That
    /// repository is now frontend-only, so the judgement has to be here or nowhere.
    #[test]
    fn a_job_that_stopped_firing_is_reported() {
        let j = judge_silence("ingest-epochs", 10 * 60, Some(390), 1000)
            .expect("390 minutes of silence from a ten-minute job is a stopped job");
        assert_eq!(j.step, "kittiwake:ingest-epochs");
        assert_eq!(j.allowed_minutes, 30);
        assert_eq!(j.silent_for_minutes, 390);
        assert!(!j.never_ran);
    }

    /// A late tick is not a stopped job, and a check that cannot tell them apart gets ignored.
    #[test]
    fn a_single_late_tick_is_not_reported() {
        assert!(judge_silence("ingest-epochs", 10 * 60, Some(11), 1000).is_none());
        assert!(judge_silence("ingest-epochs", 10 * 60, Some(29), 1000).is_none());
    }

    /// The floor. Three minutes of quiet from a one-minute warmer is load, not a fault.
    #[test]
    fn a_fast_job_gets_the_floor_rather_than_three_of_its_own_ticks() {
        assert_eq!(silence_window_minutes(1), SILENCE_FLOOR_MINUTES);
        assert_eq!(silence_window_minutes(2), SILENCE_FLOOR_MINUTES);
        assert!(judge_silence("warm", 60, Some(5), 1000).is_none());
        assert!(judge_silence("warm", 60, Some(45), 1000).is_some());
    }

    /// Start-up. Every job has legitimately never run, and reporting the whole schedule as stopped
    /// thirty seconds after a deploy is how a health endpoint gets muted.
    #[test]
    fn a_job_that_has_never_run_is_judged_on_uptime_not_on_absence() {
        assert!(judge_silence("ingest-disputes", 360 * 60, None, 2).is_none());
        let j = judge_silence("ingest-epochs", 10 * 60, None, 400)
            .expect("up for six hours and never once fired is a stopped job");
        assert!(j.never_ran);
        assert_eq!(j.silent_for_minutes, 400);
    }

    /// The recorded step carries the prefix; the scheduler's own name does not. Comparing the two
    /// directly would silently match nothing and report every job as never having run.
    #[test]
    fn the_step_name_is_the_recorded_one() {
        let j = judge_silence("refresh", 5 * 60, Some(999), 1000).unwrap();
        assert_eq!(j.step, format!("{CRON_RUN_PREFIX}refresh"));
    }

    /// Every cadence the scheduler actually runs, against the windows the other repository used
    /// before this took the judgement over. Pinned so a period change has to be looked at.
    #[test]
    fn the_live_cadences_all_get_a_sane_window() {
        for (period_minutes, want) in [
            (1, 20),     // warm
            (2, 20),     // ingest-horizon-activity
            (5, 20),     // refresh, snapshot-network
            (10, 30),    // ingest-epochs, warm-ipfs
            (15, 45),    // ingest-delegations
            (30, 90),    // refresh-chain-health
            (60, 180),   // ingest-allocations, ingest-rav
            (360, 1080), // ingest-disputes
            (720, 2160), // ingest-graph-support
        ] {
            assert_eq!(
                silence_window_minutes(period_minutes),
                want,
                "window for a {period_minutes}-minute job"
            );
        }
    }
}

/// A table or column `db/schema.sql` declares and the live database does not have.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaGap {
    pub table: String,
    /// `None` when the whole table is the problem.
    pub column: Option<String>,
    /// What is actually wrong, which is not always what it looks like.
    pub kind: GapKind,
}

/// Why a declared table or column is not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapKind {
    /// The database does not have it. A migration is owed.
    Missing,
    /// It exists and this role cannot see it. A GRANT is owed, and no schema change is.
    NoPrivilege,
}

/// What `db/schema.sql` declares that this database does not have.
///
/// The static `schema_coverage` test already checks that every table the code names appears in that
/// file. This is the other half, and it is the half that failed: the file said `ingestion_state` had
/// a `last_timestamp` column, the code read it, and the database it was pointed at did not have it.
/// Two of the three agreeing is not a confirmation.
///
/// Checked rather than applied. Applying would need DDL rights this service does not have and
/// should not want, and a binary that quietly reshapes a database it merely reads from is a worse
/// idea than one that refuses to pretend.
pub async fn schema_gaps(pool: &Pool) -> Result<Vec<SchemaGap>, DbError> {
    // `information_schema` shows only what the calling role has a privilege on, so a table this
    // role cannot touch is indistinguishable there from one that does not exist. The first
    // deployment of this check reported eleven tables as missing that were all present and all
    // ungranted: an operator's problem announced as a migration's. `pg_class` does not filter, so
    // asking both tells them apart.
    let visible = sqlx::query_as::<_, (String, String)>(
        "SELECT table_name, column_name FROM information_schema.columns WHERE table_schema = 'public'",
    )
    .fetch_all(pool)
    .await?;

    let existing = sqlx::query_as::<_, (String,)>(
        "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'v', 'm')",
    )
    .fetch_all(pool)
    .await?;

    let have: std::collections::HashSet<(String, String)> = visible.into_iter().collect();
    let visible_tables: std::collections::HashSet<String> =
        have.iter().map(|(t, _)| t.clone()).collect();
    let existing_tables: std::collections::HashSet<String> =
        existing.into_iter().map(|(t,)| t).collect();

    let used = tables_we_use();

    Ok(declared_columns(SCHEMA_SQL)
        .into_iter()
        .filter(|(table, _)| used.contains(table))
        .filter_map(|(table, column)| {
            if !visible_tables.contains(&table) {
                let kind = if existing_tables.contains(&table) {
                    GapKind::NoPrivilege
                } else {
                    GapKind::Missing
                };
                Some(SchemaGap {
                    table,
                    column: None,
                    kind,
                })
            } else if !have.contains(&(table.clone(), column.clone())) {
                // A column invisible on a visible table would be a column-level grant, which
                // nothing here uses, so an absence at this point really is one.
                Some(SchemaGap {
                    table,
                    column: Some(column),
                    kind: GapKind::Missing,
                })
            } else {
                None
            }
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// One trade click or protocol visit, as `/api/analytics/clickthrough` receives it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Clickthrough {
    pub event_type: String,
    pub token_address: Option<String>,
    pub token_symbol: Option<String>,
    pub protocol_slug: Option<String>,
    pub venue: String,
    pub pool_address: Option<String>,
    pub chain: Option<String>,
    pub destination_url: Option<String>,
    pub wallet: Option<String>,
    pub session_id: String,
}

/// The two the table's own CHECK constraint allows. Rejected here so a bad payload is a 400 rather
/// than a constraint violation reported as a 500.
pub const CLICKTHROUGH_EVENT_TYPES: &[&str] = &["trade_click", "protocol_visit"];

/// Append one clickthrough.
///
/// `wallet` is stored only when it looks like an address, and `session_id` is truncated, both as
/// the incumbent did: this is a public unauthenticated write, and the only thing standing between
/// it and arbitrary strings in a shared table is what this function refuses.
pub async fn record_clickthrough(pool: &Pool, e: &Clickthrough) -> Result<(), DbError> {
    let wallet = e.wallet.as_deref().filter(|w| {
        w.len() == 42 && w.starts_with("0x") && w[2..].chars().all(|c| c.is_ascii_hexdigit())
    });

    sqlx::query(
        "INSERT INTO clickthrough_events ( \
           event_type, token_address, token_symbol, protocol_slug, \
           venue, pool_address, chain, destination_url, wallet, session_id \
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(&e.event_type)
    .bind(&e.token_address)
    .bind(&e.token_symbol)
    .bind(&e.protocol_slug)
    .bind(&e.venue)
    .bind(&e.pool_address)
    .bind(e.chain.as_deref().unwrap_or("mainnet"))
    .bind(&e.destination_url)
    .bind(wallet.map(|w| w.to_ascii_lowercase()))
    .bind(e.session_id.chars().take(64).collect::<String>())
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod schema_declaration {
    //! What `db/schema.sql` says it declares, checked against the file itself.
    //!
    //! `schema_gaps` compares this against a live database and cannot be unit tested without one.
    //! What can be tested is the half that reads the file, and the case worth pinning is the one
    //! that broke: `ingestion_state.last_timestamp` is declared by an `ALTER TABLE ... ADD COLUMN`
    //! rather than in the `CREATE TABLE` body, which is exactly the shape a naive parser misses.

    use super::{SCHEMA_SQL, declared_columns};

    #[test]
    fn reads_columns_from_a_create_table_body() {
        let declared = declared_columns(SCHEMA_SQL);
        assert!(declared.contains(&("cron_runs".into(), "step".into())));
        assert!(declared.contains(&("cron_runs".into(), "success".into())));
        assert!(declared.contains(&("cron_runs".into(), "error_message".into())));
    }

    #[test]
    fn reads_a_column_added_by_a_later_alter() {
        let declared = declared_columns(SCHEMA_SQL);
        assert!(
            declared.contains(&("ingestion_state".into(), "last_timestamp".into())),
            "the column five ingest jobs died on for six and a half hours",
        );
    }

    #[test]
    fn does_not_mistake_a_table_constraint_for_a_column() {
        let declared = declared_columns(
            "CREATE TABLE IF NOT EXISTS t (\n  a TEXT,\n  PRIMARY KEY (a),\n  UNIQUE (a)\n);\n",
        );
        assert_eq!(declared, vec![("t".to_string(), "a".to_string())]);
    }

    #[test]
    fn stops_at_the_end_of_a_table() {
        // A statement after the closing paren must not be read as another column.
        let declared = declared_columns(
            "CREATE TABLE IF NOT EXISTS t (\n  a TEXT\n);\nCREATE INDEX IF NOT EXISTS i ON t (a);\n",
        );
        assert_eq!(declared, vec![("t".to_string(), "a".to_string())]);
    }

    #[test]
    fn finds_every_table_the_file_creates() {
        let declared = declared_columns(SCHEMA_SQL);
        let tables: std::collections::BTreeSet<&str> =
            declared.iter().map(|(t, _)| t.as_str()).collect();
        let created = SCHEMA_SQL.matches("CREATE TABLE IF NOT EXISTS ").count();
        assert_eq!(
            tables.len(),
            created,
            "every created table should contribute at least one column: {tables:?}",
        );
    }
}

#[cfg(test)]
mod gaps_are_about_us {
    //! `schema_gaps` asks about the tables this service uses, and no others.
    //!
    //! Its first deployment reported five tables nobody here touches - `community_votes` from a
    //! deleted feature, `servability_rounds`, `indexer_scores`, `delegations`,
    //! `roadmap_community_updates` - because `db/schema.sql` describes the whole database rather
    //! than this service's share of it. Asking an operator to grant privileges on tables nothing
    //! will ever read is how a health check stops being read.

    use super::{SCHEMA_SQL, declared_columns, tables_we_use};

    #[test]
    fn includes_tables_our_statements_name() {
        let used = tables_we_use();
        for t in ["ingestion_state", "cron_runs", "sync_bounties"] {
            assert!(
                used.contains(t),
                "{t} is named by this service and should be checked"
            );
        }
    }

    #[test]
    fn excludes_tables_only_the_platform_touches() {
        let used = tables_we_use();
        for t in ["community_votes", "roadmap_community_updates"] {
            assert!(!used.contains(t), "{t} is nothing to do with this service");
        }
    }

    #[test]
    fn every_table_we_use_is_declared_in_the_schema() {
        // The other direction, and the one that would bite: a table we name and the file does not
        // declare is invisible to the gap check entirely, because the check walks the file.
        let declared: std::collections::HashSet<String> = declared_columns(SCHEMA_SQL)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let missing: Vec<&String> = tables_we_use()
            .iter()
            .filter(|t| !declared.contains(*t) && !super::is_nest_table(t))
            .collect();
        assert!(
            missing.is_empty(),
            "named by this service, absent from db/schema.sql: {missing:?}"
        );
    }
}
