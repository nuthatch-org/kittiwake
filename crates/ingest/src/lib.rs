//! The ingest jobs: nest to Postgres.
//!
//! Two disciplines run through all of them. **Wei never becomes a float.** Every token amount
//! arrives as a decimal string and is divided by 1e18 inside Postgres as `numeric`, because
//! `f64` cannot hold a GRT balance and the error is silent. And **a truncated answer is a
//! defect, not a page.** Each query is sized to fit under the nest's row cap, so `truncated`
//! means the assumption broke and the run fails rather than writing a partial snapshot that
//! reads as complete.

use kittiwake_db::Pool;
use kittiwake_nest::{Lane, NestGate, QuerySpec, SqlAnswer};
use kittiwake_types::NestId;

pub mod bounties;
pub mod graph_support;
pub mod live;
pub mod liveness;
pub mod names;
pub mod qos_days;
pub mod qos_score;
pub mod rav;
pub mod refresh;
pub mod snapshots;
pub mod state;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("nest: {0}")]
    Nest(#[from] kittiwake_nest::NestError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Refused(String),
}

impl From<kittiwake_db::DbError> for IngestError {
    fn from(e: kittiwake_db::DbError) -> Self {
        match e {
            kittiwake_db::DbError::Sqlx(e) => Self::Db(e),
        }
    }
}

/// One page of rows from a nest, with the truncation check every caller would otherwise forget.
pub(crate) async fn page(
    gate: &NestGate,
    nest: NestId,
    sql: String,
    lane: Lane,
    what: &str,
) -> Result<SqlAnswer, IngestError> {
    let mut spec = QuerySpec::cron(nest, sql);
    spec.lane = lane;
    let answer = gate.query(&spec).await?;
    if answer.provenance.truncated {
        return Err(IngestError::Refused(format!(
            "the nest truncated the {what} page: this query is sized to fit under the row cap, so \
             a truncated answer means that assumption broke. Writing it would leave a partial \
             snapshot that reads as complete."
        )));
    }
    Ok(answer)
}

pub(crate) fn s(v: &serde_json::Value, k: &str) -> Option<String> {
    match v.get(k)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn i(v: &serde_json::Value, k: &str) -> Option<i64> {
    match v.get(k)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub struct Ingested {
    pub rows: u64,
}

// ---------------------------------------------------------------------------------------------
// Epochs
// ---------------------------------------------------------------------------------------------

/// Walks forward from the last epoch ingested. Query fee rebates are zero under Horizon: the
/// concept does not exist there, and writing null would read as "unknown" rather than "none".
///
/// The L1 range is asked for first. A nest from before its `l1_blocks` re-index refuses that, and
/// the page is read without it. Once the nest has it, epochs already stored without one are read
/// again from the first of them, so the range fills in behind the swap rather than only ahead of it.
pub async fn epochs(pool: &Pool, gate: &NestGate, lane: Lane) -> Result<Ingested, IngestError> {
    let cursor = state::get(pool, "epochs").await?.last_epoch.unwrap_or(0) as i64;
    let refill: Option<i32> =
        sqlx::query_scalar("SELECT MIN(id) FROM epochs WHERE start_l1_block IS NULL AND id <= $1")
            .bind(cursor as i32)
            .fetch_one(pool)
            .await?;
    let from = refill.map_or(cursor, |id| i64::from(id) - 1).min(cursor);
    let answer = match page(
        gate,
        NestId::Alloc,
        epochs_page_sql(from, true),
        lane,
        "epochs",
    )
    .await
    {
        Err(IngestError::Nest(kittiwake_nest::NestError::Http { reason, .. }))
            if kittiwake_read::sql::lacks_epoch_l1(&reason) =>
        {
            page(
                gate,
                NestId::Alloc,
                epochs_page_sql(cursor, false),
                lane,
                "epochs",
            )
            .await?
        }
        other => other?,
    };
    if answer.rows.is_empty() {
        state::touch(pool, "epochs").await?;
        return Ok(Ingested { rows: 0 });
    }

    let (mut ids, mut starts, mut ends) = (vec![], vec![], vec![]);
    let (mut l1_starts, mut l1_ends) = (vec![], vec![]);
    let (mut stake, mut signalled, mut rewards) = (vec![], vec![], vec![]);
    let (mut ind_rewards, mut del_rewards) = (vec![], vec![]);
    let (mut fees, mut curator_fees, mut taxed) = (vec![], vec![], vec![]);

    for r in &answer.rows {
        ids.push(i(r, "id").unwrap_or(0) as i32);
        starts.push(i(r, "start_block").unwrap_or(0) as i32);
        ends.push(i(r, "end_block").unwrap_or(0) as i32);
        l1_starts.push(i(r, "start_l1_block").map(|b| b as i32));
        l1_ends.push(i(r, "end_l1_block").map(|b| b as i32));
        stake.push(s(r, "stake_deposited").unwrap_or_else(|| "0".into()));
        signalled.push(s(r, "signalled_tokens").unwrap_or_else(|| "0".into()));
        rewards.push(s(r, "total_rewards").unwrap_or_else(|| "0".into()));
        ind_rewards.push(s(r, "total_indexer_rewards").unwrap_or_else(|| "0".into()));
        del_rewards.push(s(r, "total_delegator_rewards").unwrap_or_else(|| "0".into()));
        fees.push(s(r, "query_fees_collected").unwrap_or_else(|| "0".into()));
        curator_fees.push(s(r, "curator_query_fees").unwrap_or_else(|| "0".into()));
        taxed.push(s(r, "taxed_query_fees").unwrap_or_else(|| "0".into()));
    }
    // A refill page sits behind the cursor and must not move it back.
    let (highest, highest_block) = if ids.iter().copied().max().unwrap_or(0) as i64 >= cursor {
        (
            ids.iter().copied().max().unwrap_or(0),
            starts.iter().copied().max().unwrap_or(0),
        )
    } else {
        let st = state::get(pool, "epochs").await?;
        (st.last_epoch.unwrap_or(0), st.last_block.unwrap_or(0))
    };

    // Deliberately not `query!`. Every amount here is wei carried as a decimal string and cast to
    // `numeric` by Postgres, which keeps all of it; a wei figure does not fit an `f64` and two
    // rows differing in the last few digits are exactly the pair somebody is trying to tell apart.
    // The macro would want the `bigdecimal` feature or `float8` parameters, and the second rounds
    // the money to buy a compile-time check. #2.
    sqlx::query(
        "INSERT INTO epochs (id, start_block, end_block, stake_deposited, signalled_tokens, \
         total_rewards, total_indexer_rewards, total_delegator_rewards, total_query_fees, \
         query_fees_collected, curator_query_fees, query_fee_rebates, taxed_query_fees, \
         start_l1_block, end_l1_block) \
         SELECT id, start_block, end_block, stake / 1e18, signalled / 1e18, rewards / 1e18, \
         ind / 1e18, del / 1e18, (fees + cur) / 1e18, fees / 1e18, cur / 1e18, 0, taxed / 1e18, \
         l1_start, l1_end \
         FROM UNNEST($1::int[], $2::int[], $3::int[], $4::numeric[], $5::numeric[], $6::numeric[], \
         $7::numeric[], $8::numeric[], $9::numeric[], $10::numeric[], $11::numeric[], $12::int[], $13::int[]) \
         AS t(id, start_block, end_block, stake, signalled, rewards, ind, del, fees, cur, taxed, l1_start, l1_end) \
         ON CONFLICT (id) DO UPDATE SET \
         start_block = EXCLUDED.start_block, \
         end_block = EXCLUDED.end_block, stake_deposited = EXCLUDED.stake_deposited, \
         signalled_tokens = EXCLUDED.signalled_tokens, total_rewards = EXCLUDED.total_rewards, \
         total_indexer_rewards = EXCLUDED.total_indexer_rewards, \
         total_delegator_rewards = EXCLUDED.total_delegator_rewards, \
         total_query_fees = EXCLUDED.total_query_fees, query_fees_collected = EXCLUDED.query_fees_collected, \
         curator_query_fees = EXCLUDED.curator_query_fees, query_fee_rebates = EXCLUDED.query_fee_rebates, \
         taxed_query_fees = EXCLUDED.taxed_query_fees, \
         start_l1_block = COALESCE(EXCLUDED.start_l1_block, epochs.start_l1_block), \
         end_l1_block = COALESCE(EXCLUDED.end_l1_block, epochs.end_l1_block)",
    )
    .bind(&ids)
    .bind(&starts)
    .bind(&ends)
    .bind(&stake)
    .bind(&signalled)
    .bind(&rewards)
    .bind(&ind_rewards)
    .bind(&del_rewards)
    .bind(&fees)
    .bind(&curator_fees)
    .bind(&taxed)
    .bind(&l1_starts)
    .bind(&l1_ends)
    .execute(pool)
    .await?;

    state::set(pool, "epochs", Some(highest), Some(highest_block), None).await?;
    Ok(Ingested {
        rows: answer.rows.len() as u64,
    })
}

/// One page of `lodestar_epochs` after epoch `after`, with the L1 range or without it.
fn epochs_page_sql(after: i64, l1: bool) -> String {
    format!(
        "SELECT id, start_block, end_block, {}CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(stake_deposited AS VARCHAR) AS stake_deposited, CAST(total_rewards AS VARCHAR) AS total_rewards, \
         CAST(total_indexer_rewards AS VARCHAR) AS total_indexer_rewards, \
         CAST(total_delegator_rewards AS VARCHAR) AS total_delegator_rewards, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, \
         CAST(curator_query_fees AS VARCHAR) AS curator_query_fees, \
         CAST(taxed_query_fees AS VARCHAR) AS taxed_query_fees \
         FROM lodestar_epochs WHERE id > {after} ORDER BY id ASC LIMIT 500",
        if l1 {
            "start_l1_block, end_l1_block, "
        } else {
            ""
        }
    )
}

// ---------------------------------------------------------------------------------------------
// Delegation events
// ---------------------------------------------------------------------------------------------

const DELEGATION_PAGE: usize = 1000;

/// Walks forward by timestamp.
///
/// The cursor subtlety, kept from the original because it is easy to get wrong: a full page that
/// ends part-way through a second would drop the rest of that second on the next pass. So a full
/// page rewinds one second and relies on the primary key to dedupe, while a short page - which by
/// definition reached the end - takes the last timestamp as it stands.
pub async fn delegation_events(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
) -> Result<Ingested, IngestError> {
    let mut cursor = state::get(pool, "delegation_events")
        .await?
        .last_block
        .unwrap_or(0) as i64;
    let sql = format!(
        "SELECT id, event_type, indexer, delegator, CAST(tokens AS VARCHAR) AS tokens, timestamp \
         FROM lodestar_delegations WHERE timestamp > {cursor} ORDER BY timestamp ASC, id ASC LIMIT {DELEGATION_PAGE}"
    );
    let answer = page(gate, NestId::Alloc, sql, lane, "delegation events").await?;
    if answer.rows.is_empty() {
        state::touch(pool, "delegation_events").await?;
        return Ok(Ingested { rows: 0 });
    }

    let (mut ids, mut kinds, mut delegators, mut indexers, mut tokens, mut times) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    for r in &answer.rows {
        let Some(id) = s(r, "id") else { continue };
        ids.push(id);
        kinds.push(s(r, "event_type").unwrap_or_default());
        delegators.push(s(r, "delegator").unwrap_or_default().to_lowercase());
        indexers.push(s(r, "indexer").unwrap_or_default().to_lowercase());
        tokens.push(s(r, "tokens").unwrap_or_else(|| "0".into()));
        times.push(i(r, "timestamp").unwrap_or(0));
    }

    // Deliberately not `query!`. Every amount here is wei carried as a decimal string and cast to
    // `numeric` by Postgres, which keeps all of it; a wei figure does not fit an `f64` and two
    // rows differing in the last few digits are exactly the pair somebody is trying to tell apart.
    // The macro would want the `bigdecimal` feature or `float8` parameters, and the second rounds
    // the money to buy a compile-time check. #2.
    sqlx::query(
        "INSERT INTO delegation_events (id, event_type, delegator, indexer, tokens_grt, timestamp) \
         SELECT id, kind, delegator, indexer, tokens / 1e18, to_timestamp(ts) \
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::numeric[], $6::bigint[]) \
         AS t(id, kind, delegator, indexer, tokens, ts) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&ids)
    .bind(&kinds)
    .bind(&delegators)
    .bind(&indexers)
    .bind(&tokens)
    .bind(&times)
    .execute(pool)
    .await?;

    let last = times.iter().copied().max().unwrap_or(cursor);
    cursor = if answer.rows.len() < DELEGATION_PAGE {
        last
    } else {
        (cursor + 1).max(last - 1)
    };
    state::set(pool, "delegation_events", None, Some(cursor as i32), None).await?;
    Ok(Ingested {
        rows: ids.len() as u64,
    })
}

// ---------------------------------------------------------------------------------------------
// Allocations
// ---------------------------------------------------------------------------------------------

const ALLOCATION_PAGE: usize = 2000;

/// Walks forward by allocation id.
///
/// Scope, deliberately: the nest indexes SubgraphService from its deployment, so it carries Horizon
/// allocations only. The legacy rows already in Postgres are closed, immutable, and left alone.
pub async fn allocations(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
) -> Result<Ingested, IngestError> {
    let cursor = state::get(pool, "allocations")
        .await?
        .last_id
        .unwrap_or_default();
    let where_clause = if cursor.is_empty() {
        String::new()
    } else {
        // The cursor is an id we produced and stored; quoting it is still the wrong shape, so it is
        // checked to be hex before it goes anywhere near a statement.
        if !cursor.starts_with("0x") || !cursor[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(IngestError::Refused(format!(
                "stored allocation cursor is not an id: {cursor:?}"
            )));
        }
        format!("WHERE id > '{cursor}' ")
    };
    let sql = format!(
        "SELECT id, indexer, subgraph_deployment, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, created_at_epoch, closed_at_epoch, \
         created_at, closed_at, poi, CAST(indexing_rewards AS VARCHAR) AS indexing_rewards, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, status \
         FROM lodestar_allocations {where_clause}ORDER BY id ASC LIMIT {ALLOCATION_PAGE}"
    );
    let answer = page(gate, NestId::Alloc, sql, lane, "allocations").await?;
    if answer.rows.is_empty() {
        state::touch(pool, "allocations").await?;
        return Ok(Ingested { rows: 0 });
    }

    let mut ids = vec![];
    let (mut indexers, mut deployments, mut allocated, mut signal) =
        (vec![], vec![], vec![], vec![]);
    let (mut created_epoch, mut closed_epoch) = (vec![], vec![]);
    let (mut created_at, mut closed_at) = (vec![], vec![]);
    let (mut pois, mut rewards, mut fees, mut statuses) = (vec![], vec![], vec![], vec![]);

    for r in &answer.rows {
        let Some(id) = s(r, "id") else { continue };
        ids.push(id);
        indexers.push(s(r, "indexer").unwrap_or_default().to_lowercase());
        deployments.push(s(r, "subgraph_deployment").unwrap_or_default());
        allocated.push(s(r, "allocated_tokens").unwrap_or_else(|| "0".into()));
        signal.push(s(r, "signalled_tokens").unwrap_or_else(|| "0".into()));
        created_epoch.push(i(r, "created_at_epoch").unwrap_or(0) as i32);
        closed_epoch.push(i(r, "closed_at_epoch").unwrap_or(0) as i32);
        created_at.push(i(r, "created_at").unwrap_or(0));
        closed_at.push(i(r, "closed_at").unwrap_or(0));
        pois.push(s(r, "poi").unwrap_or_default());
        rewards.push(s(r, "indexing_rewards").unwrap_or_else(|| "0".into()));
        fees.push(s(r, "query_fees_collected").unwrap_or_else(|| "0".into()));
        statuses.push(s(r, "status").unwrap_or_default());
    }

    // Deliberately not `query!`. Every amount here is wei carried as a decimal string and cast to
    // `numeric` by Postgres, which keeps all of it; a wei figure does not fit an `f64` and two
    // rows differing in the last few digits are exactly the pair somebody is trying to tell apart.
    // The macro would want the `bigdecimal` feature or `float8` parameters, and the second rounds
    // the money to buy a compile-time check. #2.
    sqlx::query(
        "INSERT INTO allocations (id, indexer_address, deployment_id, allocated_tokens_grt, signal_at_open, \
         created_epoch, closed_epoch, created_at, closed_at, poi, indexing_rewards_grt, query_fees_grt, status) \
         SELECT id, indexer, deployment, allocated / 1e18, signal / 1e18, created_epoch, \
         NULLIF(closed_epoch, 0), to_timestamp(created_at), \
         CASE WHEN closed_at = 0 THEN NULL ELSE to_timestamp(closed_at) END, \
         NULLIF(poi, ''), rewards / 1e18, fees / 1e18, status \
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::numeric[], $5::numeric[], $6::int[], $7::int[], \
         $8::bigint[], $9::bigint[], $10::text[], $11::numeric[], $12::numeric[], $13::text[]) \
         AS t(id, indexer, deployment, allocated, signal, created_epoch, closed_epoch, created_at, closed_at, \
         poi, rewards, fees, status) \
         ON CONFLICT (id) DO UPDATE SET \
         allocated_tokens_grt = EXCLUDED.allocated_tokens_grt, closed_epoch = EXCLUDED.closed_epoch, \
         closed_at = EXCLUDED.closed_at, poi = EXCLUDED.poi, \
         indexing_rewards_grt = EXCLUDED.indexing_rewards_grt, query_fees_grt = EXCLUDED.query_fees_grt, \
         status = EXCLUDED.status",
    )
    .bind(&ids)
    .bind(&indexers)
    .bind(&deployments)
    .bind(&allocated)
    .bind(&signal)
    .bind(&created_epoch)
    .bind(&closed_epoch)
    .bind(&created_at)
    .bind(&closed_at)
    .bind(&pois)
    .bind(&rewards)
    .bind(&fees)
    .bind(&statuses)
    .execute(pool)
    .await?;

    let last = ids.last().cloned().unwrap_or_default();
    // A short page means the walk reached the end; start again from the beginning next time so
    // reopened and re-closed allocations are picked up.
    let next = if answer.rows.len() < ALLOCATION_PAGE {
        String::new()
    } else {
        last
    };
    state::set(pool, "allocations", None, None, Some(&next)).await?;
    Ok(Ingested {
        rows: ids.len() as u64,
    })
}

// ---------------------------------------------------------------------------------------------
// Disputes
// ---------------------------------------------------------------------------------------------

/// A full snapshot rather than a cursor walk: there are single figures of these, not a history.
///
/// **An accepted dispute fails the run.** The nest cannot compute the burn until `StakeSlashed` is
/// indexed, and writing a zero burn would be a wrong number that looks like a right one. No dispute
/// has ever been accepted, so this has never fired; when it does, it should be noticed.
pub async fn disputes(pool: &Pool, gate: &NestGate, lane: Lane) -> Result<Ingested, IngestError> {
    let sql = "SELECT d.id, d.kind, d.indexer, d.fisherman, d.allocation_id, \
               a.subgraph_deployment, d.status, d.created_at, d.resolved_at \
               FROM lodestar_disputes d LEFT JOIN lodestar_allocations a ON a.id = d.allocation_id \
               ORDER BY d.created_at"
        .to_string();
    let answer = page(gate, NestId::Alloc, sql, lane, "disputes").await?;

    let accepted: Vec<String> = answer
        .rows
        .iter()
        .filter(|r| {
            s(r, "status")
                .unwrap_or_default()
                .eq_ignore_ascii_case("accepted")
        })
        .filter_map(|r| s(r, "id"))
        .collect();
    if !accepted.is_empty() {
        return Err(IngestError::Refused(format!(
            "{} dispute(s) are accepted and the nest cannot yet compute the burn: {}. Writing a \
             zero burn would be a wrong number wearing the shape of a right one.",
            accepted.len(),
            accepted.join(", ")
        )));
    }

    if answer.rows.is_empty() {
        state::touch(pool, "disputes").await?;
        return Ok(Ingested { rows: 0 });
    }

    let (mut ids, mut kinds, mut indexers, mut fishermen) = (vec![], vec![], vec![], vec![]);
    let (mut allocs, mut deployments, mut statuses) = (vec![], vec![], vec![]);
    let (mut created, mut closed) = (vec![], vec![]);
    for r in &answer.rows {
        let Some(id) = s(r, "id") else { continue };
        ids.push(id);
        kinds.push(s(r, "kind").unwrap_or_default());
        indexers.push(s(r, "indexer").unwrap_or_default().to_lowercase());
        fishermen.push(s(r, "fisherman").unwrap_or_default().to_lowercase());
        allocs.push(s(r, "allocation_id").unwrap_or_default());
        deployments.push(s(r, "subgraph_deployment").unwrap_or_default());
        statuses.push(s(r, "status").unwrap_or_default());
        created.push(i(r, "created_at").unwrap_or(0));
        closed.push(i(r, "resolved_at").unwrap_or(0));
    }

    // Deliberately not `query!`. Every amount here is wei carried as a decimal string and cast to
    // `numeric` by Postgres, which keeps all of it; a wei figure does not fit an `f64` and two
    // rows differing in the last few digits are exactly the pair somebody is trying to tell apart.
    // The macro would want the `bigdecimal` feature or `float8` parameters, and the second rounds
    // the money to buy a compile-time check. #2.
    sqlx::query(
        "INSERT INTO disputes (id, dispute_type, indexer_address, fisherman, allocation_id, deployment_id, \
         status, tokens_slashed_grt, tokens_burned_grt, created_at, closed_at) \
         SELECT id, kind, indexer, NULLIF(fisherman, ''), NULLIF(alloc, ''), NULLIF(deployment, ''), \
         status, 0, 0, to_timestamp(created), \
         CASE WHEN closed = 0 THEN NULL ELSE to_timestamp(closed) END \
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], \
         $8::bigint[], $9::bigint[]) \
         AS t(id, kind, indexer, fisherman, alloc, deployment, status, created, closed) \
         ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status, closed_at = EXCLUDED.closed_at",
    )
    .bind(&ids)
    .bind(&kinds)
    .bind(&indexers)
    .bind(&fishermen)
    .bind(&allocs)
    .bind(&deployments)
    .bind(&statuses)
    .bind(&created)
    .bind(&closed)
    .execute(pool)
    .await?;

    state::touch(pool, "disputes").await?;
    Ok(Ingested {
        rows: ids.len() as u64,
    })
}

// ---------------------------------------------------------------------------------------------
// Network snapshot
// ---------------------------------------------------------------------------------------------

/// One row per run, which is what the staking trend chart is drawn from.
pub async fn network_snapshot(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
    grt_price_usd: Option<f64>,
    network_tvl_usd: Option<f64>,
) -> Result<Ingested, IngestError> {
    let answer = page(
        gate,
        NestId::Alloc,
        kittiwake_read::sql::network_sql().to_string(),
        lane,
        "network totals",
    )
    .await?;
    let Some(n) = answer.rows.first() else {
        return Err(IngestError::Refused(
            "the network view returned no rows".into(),
        ));
    };

    // Deliberately not `query!`. The five totals are wei carried as decimal strings and cast to
    // `numeric` by Postgres, which keeps every digit; a wei figure does not fit an `f64`. The
    // macro would want the `bigdecimal` feature or a `float8` parameter, and the second rounds the
    // protocol's own totals to buy a compile-time check. #2.
    sqlx::query(
        "INSERT INTO network_snapshots (total_staked, total_delegated, total_signalled, total_allocated, \
         total_supply_grt, indexer_count, active_indexer_count, delegator_count, active_delegator_count, \
         curator_count, active_curator_count, subgraph_count, active_subgraph_count, current_epoch, \
         grt_price_usd, network_tvl_usd) \
         VALUES ($1::numeric / 1e18, $2::numeric / 1e18, $3::numeric / 1e18, $4::numeric / 1e18, \
         $5::numeric / 1e18, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
    )
    .bind(s(n, "total_tokens_staked").unwrap_or_else(|| "0".into()))
    .bind(s(n, "total_delegated_tokens").unwrap_or_else(|| "0".into()))
    .bind(s(n, "total_tokens_signalled").unwrap_or_else(|| "0".into()))
    .bind(s(n, "total_tokens_allocated").unwrap_or_else(|| "0".into()))
    .bind(s(n, "total_supply").unwrap_or_else(|| "0".into()))
    .bind(i(n, "indexer_count").unwrap_or(0) as i32)
    .bind(i(n, "staked_indexers_count").unwrap_or(0) as i32)
    .bind(i(n, "delegator_count").unwrap_or(0) as i32)
    .bind(i(n, "active_delegator_count").unwrap_or(0) as i32)
    .bind(i(n, "curator_count").unwrap_or(0) as i32)
    .bind(i(n, "active_curator_count").unwrap_or(0) as i32)
    .bind(i(n, "subgraph_count").unwrap_or(0) as i32)
    .bind(i(n, "active_subgraph_count").unwrap_or(0) as i32)
    .bind(i(n, "current_epoch").unwrap_or(0) as i32)
    .bind(grt_price_usd)
    .bind(network_tvl_usd)
    .execute(pool)
    .await?;

    Ok(Ingested { rows: 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The legacy page is the statement the gate set records as `ingest.epochs`, byte for byte, so a
    /// nest from before the L1 range is asked exactly what it was asked before.
    #[test]
    fn the_legacy_epochs_page_is_unchanged_and_the_l1_page_only_adds_the_range() {
        let legacy = "SELECT id, start_block, end_block, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(stake_deposited AS VARCHAR) AS stake_deposited, CAST(total_rewards AS VARCHAR) AS total_rewards, \
         CAST(total_indexer_rewards AS VARCHAR) AS total_indexer_rewards, \
         CAST(total_delegator_rewards AS VARCHAR) AS total_delegator_rewards, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, \
         CAST(curator_query_fees AS VARCHAR) AS curator_query_fees, \
         CAST(taxed_query_fees AS VARCHAR) AS taxed_query_fees \
         FROM lodestar_epochs WHERE id > 0 ORDER BY id ASC LIMIT 500";
        assert_eq!(epochs_page_sql(0, false), legacy);
        assert_eq!(
            epochs_page_sql(0, true),
            legacy.replacen(
                "end_block, ",
                "end_block, start_l1_block, end_l1_block, ",
                1
            )
        );
        assert!(kittiwake_read::sql::lacks_epoch_l1(
            "plan error: Binder Error: Referenced column \"start_l1_block\" not found"
        ));
    }

    #[test]
    fn numbers_arrive_as_strings_or_numbers_and_both_are_read() {
        let row = serde_json::json!({"a": "123", "b": 456, "c": null});
        assert_eq!(s(&row, "a").as_deref(), Some("123"));
        assert_eq!(s(&row, "b").as_deref(), Some("456"));
        assert_eq!(s(&row, "c"), None);
        assert_eq!(i(&row, "a"), Some(123));
        assert_eq!(i(&row, "b"), Some(456));
    }

    /// Wei must never round-trip through a float. This is the value that proves it: it does not
    /// survive `f64`, and Postgres `numeric` holds it exactly.
    #[test]
    fn a_full_precision_wei_amount_is_kept_as_text() {
        let wei = "77100063717931471486199";
        let row = serde_json::json!({"tokens": wei});
        assert_eq!(s(&row, "tokens").as_deref(), Some(wei));
        assert_ne!(
            format!("{:.0}", wei.parse::<f64>().unwrap()),
            wei,
            "if this ever passes, f64 gained precision and the comment above needs rewriting"
        );
    }
}
