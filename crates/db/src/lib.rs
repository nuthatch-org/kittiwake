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

    const SCHEMA: &str = include_str!("../../../db/schema.sql");
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
        "unnest", "lateral", "select", "insert", "update", "delete", "set", "from", "where", "t",
        "n", "e", "f", "o", "a", "c", "i", "d",
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
            let window = &rest[..rest.len().min(4000)];
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
    Ok(())
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
