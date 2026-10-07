//! The SQL this service issues, kept in one place so two callers cannot drift into asking the same
//! question two different ways. Ported from `src/lib/nest-queries.ts`.
//!
//! Interpolation discipline: every value reaching a query is either a clamped integer or an
//! address that has passed [`is_address`]. Nothing else is ever formatted into a statement.

/// Lower-case hex address, exactly twenty bytes. Anything else never reaches the nest.
pub fn is_address(s: &str) -> bool {
    s.len() == 42
        && s.starts_with("0x")
        && s[2..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn clamp(v: i64, lo: i64, hi: i64) -> i64 {
    v.max(lo).min(hi)
}

/// Our own indexer, excluded from the public directory so the dashboard does not advertise its
/// operator to its readers.
pub use kittiwake_db::INDEXERS_EXCLUDED;

/// The columns a caller may sort the directory by, and the column each maps to. An allowlist
/// rather than a passthrough, because this value lands in an ORDER BY.
pub fn indexers_order_column(order_by: &str) -> &'static str {
    match order_by {
        "delegatedTokens" => "delegated_tokens",
        "allocatedTokens" => "allocated_tokens",
        "allocationCount" => "allocation_count",
        "rewardsEarned" => "rewards_earned",
        "queryFeesCollected" => "query_fees_collected",
        "createdAt" => "created_at",
        _ => "staked_tokens",
    }
}

pub fn indexers_sql(first: i64, skip: i64, order_by: &str, descending: bool) -> String {
    let col = indexers_order_column(order_by);
    let dir = if descending { "DESC" } else { "ASC" };
    let first = clamp(first, 1, 1000);
    let skip = clamp(skip, 0, 100_000);
    format!(
        "SELECT id, CAST(staked_tokens AS VARCHAR) AS staked_tokens, CAST(locked_tokens AS VARCHAR) AS locked_tokens, \
         CAST(delegated_tokens AS VARCHAR) AS delegated_tokens, CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, \
         allocation_count, indexing_reward_cut, query_fee_cut, last_delegation_parameter_update, \
         CAST(rewards_earned AS VARCHAR) AS rewards_earned, CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, \
         CAST(delegator_shares AS VARCHAR) AS delegator_shares, url, geohash, created_at \
         FROM lodestar_indexers i WHERE i.staked_tokens > 0 AND i.id <> '{INDEXERS_EXCLUDED}' \
         ORDER BY i.{col} {dir}, i.id ASC LIMIT {first} OFFSET {skip}"
    )
}

pub fn network_sql() -> &'static str {
    "SELECT CAST(total_tokens_staked AS VARCHAR) AS total_tokens_staked, CAST(total_delegated_tokens AS VARCHAR) AS total_delegated_tokens, \
     CAST(total_tokens_signalled AS VARCHAR) AS total_tokens_signalled, CAST(total_tokens_allocated AS VARCHAR) AS total_tokens_allocated, \
     CAST(total_indexing_rewards AS VARCHAR) AS total_indexing_rewards, CAST(total_query_fees AS VARCHAR) AS total_query_fees, \
     CAST(total_supply AS VARCHAR) AS total_supply, CAST(issuance_per_block AS VARCHAR) AS issuance_per_block, \
     CAST(bridge_minted AS VARCHAR) AS bridge_minted, CAST(bridge_burned AS VARCHAR) AS bridge_burned, \
     indexer_count, staked_indexers_count, delegator_count, active_delegator_count, curator_count, active_curator_count, \
     subgraph_count, active_subgraph_count, current_epoch FROM lodestar_network"
}

pub fn grt_supply_sql() -> &'static str {
    "SELECT gross_burned, bridge_burned, burned, gross_minted, bridge_minted, issued FROM grt_supply"
}

pub fn grt_supply_mainnet_sql() -> &'static str {
    "SELECT burned, minted FROM grt_supply"
}

pub fn network_params_sql() -> &'static str {
    "SELECT delegation_ratio, curation_tax_percentage, protocol_payment_cut, max_thawing_period_seconds, \
     epoch_length, last_length_update_epoch, last_length_update_block, \
     CAST(total_curation_tax AS VARCHAR) AS total_curation_tax, CAST(total_protocol_tax AS VARCHAR) AS total_protocol_tax \
     FROM lodestar_network_params"
}

pub fn epochs_sql(first: i64) -> String {
    let first = clamp(first, 1, 1000);
    format!(
        "SELECT id, start_block, end_block, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(stake_deposited AS VARCHAR) AS stake_deposited, CAST(total_rewards AS VARCHAR) AS total_rewards, \
         CAST(total_indexer_rewards AS VARCHAR) AS total_indexer_rewards, CAST(total_delegator_rewards AS VARCHAR) AS total_delegator_rewards, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, CAST(curator_query_fees AS VARCHAR) AS curator_query_fees, \
         CAST(taxed_query_fees AS VARCHAR) AS taxed_query_fees \
         FROM lodestar_epochs ORDER BY id DESC LIMIT {first}"
    )
}

/// [`epochs_sql`] with the exact L1 range EpochManager counts in. A nest from before its `l1_blocks`
/// re-index has no such columns and refuses this; [`lacks_epoch_l1`] recognises that refusal.
pub fn epochs_l1_sql(first: i64) -> String {
    epochs_sql(first).replacen(
        "start_block, end_block,",
        "start_block, end_block, start_l1_block, end_l1_block,",
        1,
    )
}

/// The nest's refusal of a statement naming the epoch L1 columns it does not have yet.
pub fn lacks_epoch_l1(reason: &str) -> bool {
    reason.contains("start_l1_block") || reason.contains("end_l1_block")
}

/// The delegation feed: a UNION over the four HorizonStaking delegation-event tables, mapped to
/// the vocabulary the frontend already speaks.
pub fn delegation_events_sql(indexer: Option<&str>, first: i64, since: i64) -> Option<String> {
    if let Some(a) = indexer
        && !is_address(a)
    {
        return None;
    }
    let first = clamp(first, 1, 1000);
    let row = |ev: &str, table: &str, col: &str| {
        format!(
            "SELECT tx_hash || '-' || CAST(log_index AS VARCHAR) AS id, '{ev}' AS \"eventType\", \
             LOWER({col}) AS indexer, LOWER(delegator) AS delegator, CAST(tokens AS VARCHAR) AS tokens, \
             block_timestamp AS ts, tx_hash AS \"txHash\" FROM \"{table}\""
        )
    };
    let union = [
        row(
            "delegation",
            "staking__tokens_delegated",
            "\"serviceProvider\"",
        ),
        row(
            "undelegation",
            "staking__tokens_undelegated",
            "\"serviceProvider\"",
        ),
        row(
            "withdrawal",
            "staking__delegated_tokens_withdrawn",
            "\"serviceProvider\"",
        ),
        row(
            "withdrawal",
            "staking__stake_delegated_withdrawn",
            "indexer",
        ),
    ]
    .join(" UNION ALL ");

    let mut where_clause = format!("ts > {since}");
    if let Some(a) = indexer {
        where_clause.push_str(&format!(" AND indexer = '{a}'"));
    }
    Some(format!(
        "SELECT id, \"eventType\", indexer, delegator, tokens, CAST(ts AS VARCHAR) AS \"timestamp\", \"txHash\" \
         FROM ({union}) t WHERE {where_clause} ORDER BY ts DESC, id DESC LIMIT {first}"
    ))
}

// ---------------------------------------------------------------------------------------------
// Identifiers. Everything below interpolates only values that have passed one of these.
// ---------------------------------------------------------------------------------------------

/// A subgraph deployment id: `0x` and sixty-four lower-case hex characters.
pub fn is_deployment_id(s: &str) -> bool {
    s.len() == 66
        && s.starts_with("0x")
        && s[2..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A GNS subgraph id, which is a `uint256` and reaches us as **decimal**, not as hex.
///
/// This used to defer to [`is_deployment_id`], on the stated belief that the two identifiers share
/// a shape. They do not: the gns nest answers `deployment_subgraphs` with a seventy-seven digit
/// decimal, so every id the nest returned failed the check and `/api/subgraph-versions` answered
/// `500 the gns nest returned a subgraph id we will not query` for every deployment that has ever
/// been published. Live until the route was probed against a real deployment.
///
/// Decimal digits, no sign, no leading zero, and no wider than a `uint256`.
pub fn is_subgraph_id(s: &str) -> bool {
    const UINT256_MAX: &str =
        "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    !s.is_empty()
        && s.len() <= UINT256_MAX.len()
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s == "0" || !s.starts_with('0'))
        && (s.len() < UINT256_MAX.len() || s <= UINT256_MAX)
}

/// The all-zero POI, which means "closed without a proof" and is never a real one.
pub const ZERO_POI: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// The GraphTallyCollector, used to tell a tally redemption apart from any other escrow collection.
pub const TALLY_COLLECTOR: &str = "0x8f69f5c07477ac46fbc491b1e6d91e2bb0111a9e";

/// `AND LOWER(col) = '<addr>'`, or nothing. Returns `None` when the address is not one, so a
/// caller cannot accidentally build a statement around a value that failed validation.
fn receiver_clause(column: &str, receiver: Option<&str>) -> Option<String> {
    match receiver {
        None => Some(String::new()),
        Some(a) if is_address(a) => Some(format!(" AND LOWER({column}) = '{a}'")),
        Some(_) => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Curators and delegators
// ---------------------------------------------------------------------------------------------

pub fn curators_sql(first: i64, skip: i64) -> String {
    let first = clamp(first, 1, 1000);
    let skip = clamp(skip, 0, 100_000);
    format!(
        "SELECT id, CAST(total_signalled_tokens AS VARCHAR) AS total_signalled_tokens, \
         CAST(total_unsignalled_tokens AS VARCHAR) AS total_unsignalled_tokens, \
         CAST(realized_rewards AS VARCHAR) AS realized_rewards, signal_count, active_signal_count \
         FROM lodestar_curators c WHERE c.total_signalled_tokens > 0 AND c.active_signal_count > 0 AND NOT c.is_gns \
         ORDER BY c.total_signalled_tokens DESC, c.id ASC LIMIT {first} OFFSET {skip}"
    )
}

pub fn curator_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT id, CAST(total_signalled_tokens AS VARCHAR) AS total_signalled_tokens, \
         CAST(total_unsignalled_tokens AS VARCHAR) AS total_unsignalled_tokens, \
         CAST(realized_rewards AS VARCHAR) AS realized_rewards, signal_count, active_signal_count \
         FROM lodestar_curators WHERE id = '{addr}'"
    ))
}

pub fn curator_signals_sql(addr: &str, first: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 1000);
    Some(format!(
        "SELECT id, subgraph_deployment, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(unsignalled_tokens AS VARCHAR) AS unsignalled_tokens, CAST(signal AS VARCHAR) AS signal, \
         last_signal_change, CAST(realized_rewards AS VARCHAR) AS realized_rewards, \
         CAST(deployment_signalled_tokens AS VARCHAR) AS deployment_signalled_tokens, \
         CAST(deployment_query_fees_amount AS VARCHAR) AS deployment_query_fees_amount, \
         CAST(deployment_staked_tokens AS VARCHAR) AS deployment_staked_tokens \
         FROM lodestar_curator_signals s WHERE s.curator = '{addr}' \
         ORDER BY s.signalled_tokens DESC, s.subgraph_deployment LIMIT {first}"
    ))
}

pub fn delegator_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT id, CAST(total_staked_tokens AS VARCHAR) AS total_staked_tokens, \
         CAST(total_unstaked_tokens AS VARCHAR) AS total_unstaked_tokens, \
         CAST(total_realized_rewards AS VARCHAR) AS total_realized_rewards, stakes_count, active_stakes_count \
         FROM lodestar_delegators WHERE id = '{addr}'"
    ))
}

pub fn delegator_stakes_sql(addr: &str, first: i64, active_only: bool) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 1000);
    let active = if active_only { " AND p.active" } else { "" };
    // `lodestar_delegator_positions` rather than `lodestar_delegator_stakes`: the stakes view joins
    // the whole delegation pool for `staked_tokens`, and DuckDB pushes no filter into the right side
    // of a LEFT JOIN, so one delegator paid the pool fold and the indexers fold both (21 s on
    // 2026-09-26). The indexers row carries the same two pool figures, so the worth of the shares is
    // computed here with the view's own formula, and only the indexers fold remains (8.6 s).
    Some(format!(
        "WITH s AS (SELECT p.id, p.indexer, p.total_delegated_tokens, p.share_amount, p.locked_tokens, p.locked_until, \
         p.realized_rewards, p.total_undelegated_tokens, p.created_at, p.last_undelegated_at, p.active, \
         CASE WHEN i.delegator_shares > 0 \
              THEN (p.share_amount // i.delegator_shares) * i.delegated_tokens \
                   + CAST(CAST(p.share_amount % i.delegator_shares AS DOUBLE) * CAST(i.delegated_tokens AS DOUBLE) / CAST(i.delegator_shares AS DOUBLE) AS HUGEINT) \
              ELSE 0 END AS staked_tokens, \
         i.staked_tokens AS indexer_staked_tokens, i.delegated_tokens AS indexer_delegated_tokens, \
         i.delegated_thawing_tokens AS indexer_delegated_thawing_tokens, i.delegator_shares AS indexer_delegator_shares, \
         i.indexing_reward_cut, i.query_fee_cut, i.allocation_count, i.url, i.geohash \
         FROM lodestar_delegator_positions p LEFT JOIN lodestar_indexers i ON i.id = p.indexer \
         WHERE p.delegator = '{addr}'{active}) \
         SELECT id, indexer, CAST(total_delegated_tokens AS VARCHAR) AS staked_tokens, \
         CAST(staked_tokens AS VARCHAR) AS current_tokens, CAST(share_amount AS VARCHAR) AS share_amount, \
         CAST(locked_tokens AS VARCHAR) AS locked_tokens, locked_until, \
         CAST(realized_rewards AS VARCHAR) AS realized_rewards, \
         CAST(total_undelegated_tokens AS VARCHAR) AS unstaked_tokens, created_at, last_undelegated_at, active, \
         CAST(indexer_staked_tokens AS VARCHAR) AS indexer_staked_tokens, \
         CAST(indexer_delegated_tokens AS VARCHAR) AS indexer_delegated_tokens, \
         CAST(indexer_delegated_thawing_tokens AS VARCHAR) AS indexer_delegated_thawing_tokens, \
         CAST(indexer_delegator_shares AS VARCHAR) AS indexer_delegator_shares, \
         indexing_reward_cut, query_fee_cut, allocation_count, url, geohash \
         FROM s ORDER BY staked_tokens DESC, indexer LIMIT {first}"
    ))
}

// ---------------------------------------------------------------------------------------------
// One indexer
// ---------------------------------------------------------------------------------------------

pub fn indexer_detail_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT id, CAST(staked_tokens AS VARCHAR) AS staked_tokens, CAST(locked_tokens AS VARCHAR) AS locked_tokens, \
         locked_until, CAST(delegated_tokens AS VARCHAR) AS delegated_tokens, \
         CAST(delegated_thawing_tokens AS VARCHAR) AS delegated_thawing_tokens, \
         CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, allocation_count, indexing_reward_cut, query_fee_cut, \
         last_delegation_parameter_update, CAST(rewards_earned AS VARCHAR) AS rewards_earned, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, \
         CAST(delegator_shares AS VARCHAR) AS delegator_shares, \
         CAST(provisioned_tokens AS VARCHAR) AS provisioned_tokens, delegator_count, url, geohash, created_at \
         FROM lodestar_indexers WHERE id = '{addr}'"
    ))
}

/// Operators, taken as the newest state per operator across both staking eras. A `UNION` of the
/// Horizon and legacy events, ranked, keeping only the latest row and only where still allowed.
pub fn indexer_operators_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT operator FROM (SELECT LOWER(operator) AS operator, allowed, \
         ROW_NUMBER() OVER (PARTITION BY LOWER(operator) ORDER BY block_number DESC, log_index DESC) AS rn FROM ( \
         SELECT operator, allowed, block_number, log_index FROM staking__operator_set WHERE LOWER(\"serviceProvider\") = '{addr}' \
         UNION ALL SELECT operator, allowed, block_number, log_index FROM staking_legacy__set_operator WHERE LOWER(indexer) = '{addr}')) \
         WHERE rn = 1 AND allowed ORDER BY operator"
    ))
}

pub fn indexer_delegators_sql(addr: &str, first: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 1000);
    // Not `lodestar_delegator_stakes`: its per-position history fold is computed even though nothing
    // here reads it, and for an indexer with 334,612 delegation events it exceeds the nest's memory limit.
    Some(format!(
        "WITH events AS ( \
         SELECT LOWER(delegator) AS delegator, 'in' AS kind, CAST(tokens AS HUGEINT) AS tok, CAST(shares AS HUGEINT) AS sh \
         FROM staking_legacy__stake_delegated WHERE LOWER(indexer) = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'out', CAST(tokens AS HUGEINT), CAST(shares AS HUGEINT) \
         FROM staking_legacy__stake_delegated_locked WHERE LOWER(indexer) = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'in', CAST(tokens AS HUGEINT), CAST(shares AS HUGEINT) \
         FROM staking__tokens_delegated WHERE LOWER(\"serviceProvider\") = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'out', CAST(tokens AS HUGEINT), CAST(shares AS HUGEINT) \
         FROM staking__tokens_undelegated WHERE LOWER(\"serviceProvider\") = '{addr}'), \
         positions AS (SELECT delegator, SUM(CASE WHEN kind = 'in' THEN sh ELSE -sh END) AS share_amount, \
         SUM(CASE WHEN kind = 'in' THEN tok ELSE 0 END) AS total_delegated_tokens FROM events GROUP BY 1) \
         SELECT delegator || '-' || '{addr}' AS id, delegator, CAST(total_delegated_tokens AS VARCHAR) AS staked_tokens, \
         CAST(share_amount AS VARCHAR) AS share_amount FROM positions WHERE share_amount > 0 \
         ORDER BY total_delegated_tokens DESC, delegator ASC LIMIT {first}"
    ))
}

/// The columns the delegator list may sort by. Stake and share of the pool are one order: every
/// position in a pool is valued at the same rate, so both sort by shares.
pub fn indexer_delegators_order_column(order_by: &str) -> &'static str {
    match order_by {
        "delegatedAt" => "delegated_at",
        "lastChangeAt" => "last_change_at",
        "thawingTokens" => "thawing_tokens",
        _ => "share_amount",
    }
}

/// Every delegator of one indexer, a page at a time, with the size of the whole list on each row.
///
/// A position is listed while it holds shares or has Horizon tokens still thawing. A legacy lock
/// never withdrawn is carried as `locked_tokens` but does not keep a position listed: those sit
/// there forever, 1.2M GRT on one indexer. The pool is `lodestar_indexer_pool`, so a position is
/// valued as the delegator page values it.
pub fn indexer_delegators_page_sql(
    addr: &str,
    first: i64,
    skip: i64,
    order_by: &str,
    descending: bool,
) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 1000);
    let skip = clamp(skip, 0, 1_000_000);
    let col = indexer_delegators_order_column(order_by);
    let dir = if descending { "DESC" } else { "ASC" };
    Some(format!(
        "WITH events AS ( \
         SELECT LOWER(delegator) AS delegator, 'in' AS kind, CAST(tokens AS HUGEINT) AS tok, CAST(shares AS HUGEINT) AS sh, CAST(block_timestamp AS BIGINT) AS ts \
         FROM staking_legacy__stake_delegated WHERE LOWER(indexer) = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'lock', CAST(tokens AS HUGEINT), CAST(shares AS HUGEINT), CAST(block_timestamp AS BIGINT) \
         FROM staking_legacy__stake_delegated_locked WHERE LOWER(indexer) = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'in', CAST(tokens AS HUGEINT), CAST(shares AS HUGEINT), CAST(block_timestamp AS BIGINT) \
         FROM staking__tokens_delegated WHERE LOWER(\"serviceProvider\") = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'thaw', CAST(tokens AS HUGEINT), CAST(shares AS HUGEINT), CAST(block_timestamp AS BIGINT) \
         FROM staking__tokens_undelegated WHERE LOWER(\"serviceProvider\") = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'unlock', CAST(tokens AS HUGEINT), CAST(0 AS HUGEINT), CAST(block_timestamp AS BIGINT) \
         FROM staking__stake_delegated_withdrawn WHERE LOWER(indexer) = '{addr}' \
         UNION ALL SELECT LOWER(delegator), 'release', CAST(tokens AS HUGEINT), CAST(0 AS HUGEINT), CAST(block_timestamp AS BIGINT) \
         FROM staking__delegated_tokens_withdrawn WHERE LOWER(\"serviceProvider\") = '{addr}'), \
         positions AS (SELECT delegator, \
         SUM(CASE WHEN kind = 'in' THEN sh WHEN kind IN ('lock', 'thaw') THEN -sh ELSE 0 END) AS share_amount, \
         SUM(CASE WHEN kind = 'in' THEN tok ELSE 0 END) AS total_delegated_tokens, \
         GREATEST(SUM(CASE WHEN kind = 'thaw' THEN tok WHEN kind = 'release' THEN -tok ELSE 0 END), 0) AS thawing_tokens, \
         GREATEST(SUM(CASE WHEN kind = 'lock' THEN tok WHEN kind = 'unlock' THEN -tok ELSE 0 END), 0) AS locked_tokens, \
         MIN(ts) FILTER (WHERE kind = 'in') AS delegated_at, MAX(ts) AS last_change_at FROM events GROUP BY 1), \
         thaw AS (SELECT LOWER(owner) AS delegator, MAX(CAST(\"thawingUntil\" AS BIGINT)) AS thawing_until FROM staking__thaw_request_created \
         WHERE LOWER(\"serviceProvider\") = '{addr}' AND CAST(\"requestType\" AS INTEGER) IN (1, 2) GROUP BY 1), \
         pool AS (SELECT delegator_shares, delegated_tokens FROM lodestar_indexer_pool WHERE id = '{addr}') \
         SELECT p.delegator || '-' || '{addr}' AS id, p.delegator, CAST(p.share_amount AS VARCHAR) AS share_amount, \
         CAST(CASE WHEN pool.delegator_shares > 0 THEN (p.share_amount // pool.delegator_shares) * pool.delegated_tokens \
         + CAST(CAST(p.share_amount % pool.delegator_shares AS DOUBLE) * CAST(pool.delegated_tokens AS DOUBLE) / CAST(pool.delegator_shares AS DOUBLE) AS HUGEINT) \
         ELSE 0 END AS VARCHAR) AS current_tokens, \
         CAST(p.total_delegated_tokens AS VARCHAR) AS total_delegated_tokens, CAST(p.thawing_tokens AS VARCHAR) AS thawing_tokens, \
         CASE WHEN p.thawing_tokens > 0 THEN t.thawing_until END AS thawing_until, CAST(p.locked_tokens AS VARCHAR) AS locked_tokens, \
         p.delegated_at, p.last_change_at, \
         CAST(pool.delegator_shares AS VARCHAR) AS pool_shares, CAST(pool.delegated_tokens AS VARCHAR) AS pool_tokens, \
         COUNT(*) OVER () AS total, COUNT(*) FILTER (WHERE p.share_amount > 0) OVER () AS active \
         FROM positions p LEFT JOIN thaw t ON t.delegator = p.delegator CROSS JOIN pool \
         WHERE p.share_amount > 0 OR p.thawing_tokens > 0 \
         ORDER BY p.{col} {dir} NULLS LAST, p.delegator ASC LIMIT {first} OFFSET {skip}"
    ))
}

pub fn indexer_active_allocations_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT a.id, CAST(a.allocated_tokens AS VARCHAR) AS allocated_tokens, a.created_at_epoch, a.subgraph_deployment, \
         CAST(a.signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST((SELECT SUM(allocated_tokens) FROM lodestar_allocations d WHERE d.subgraph_deployment = a.subgraph_deployment AND d.status = 'Active') AS VARCHAR) AS deployment_staked_tokens \
         FROM lodestar_allocations a WHERE LOWER(a.indexer) = '{addr}' AND a.status = 'Active' ORDER BY a.id"
    ))
}

pub fn indexer_closed_allocations_sql(addr: &str, first: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 5000);
    Some(format!(
        "SELECT id, CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, created_at_epoch, closed_at_epoch, closed_at, \
         CAST(indexing_rewards AS VARCHAR) AS indexing_rewards, CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, \
         poi, force_closed, subgraph_deployment \
         FROM lodestar_allocations WHERE LOWER(indexer) = '{addr}' AND status = 'Closed' \
         ORDER BY closed_at DESC, id LIMIT {first}"
    ))
}

/// Point-in-time stake and delegation, one row per cutoff, folded from the indexer ledger. One
/// statement for the whole series rather than one per point, because each is a permit.
pub fn indexer_stake_history_sql(addr: &str, cutoffs: &[i64]) -> Option<String> {
    if !is_address(addr) || cutoffs.is_empty() || cutoffs.len() > 400 {
        return None;
    }
    let values = cutoffs
        .iter()
        .map(|c| format!("({c})"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "SELECT c.cutoff, \
         CAST(COALESCE((SELECT SUM(stake_delta) FROM lodestar_indexer_ledger l WHERE l.indexer = '{addr}' AND l.ts <= c.cutoff), 0) AS VARCHAR) AS staked_tokens, \
         CAST(COALESCE((SELECT SUM(pool_delta) + SUM(thawing_delta) FROM lodestar_indexer_ledger l WHERE l.indexer = '{addr}' AND l.ts <= c.cutoff), 0) AS VARCHAR) AS delegated_tokens \
         FROM (VALUES {values}) AS c(cutoff) ORDER BY c.cutoff"
    ))
}

/// An indexer's rewards and query fees per UTC day, newest first, from `since` (a day start).
pub fn indexer_daily_sql(addr: &str, since: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let since = since.max(0);
    Some(format!(
        "SELECT indexer, day_start, CAST(total_rewards AS VARCHAR) AS total_rewards, \
         CAST(indexer_rewards AS VARCHAR) AS indexer_rewards, CAST(delegator_rewards AS VARCHAR) AS delegator_rewards, \
         CAST(reward_count AS BIGINT) AS reward_count, CAST(fees_gross AS VARCHAR) AS fees_gross, \
         CAST(fees_curators AS VARCHAR) AS fees_curators, CAST(fees_protocol AS VARCHAR) AS fees_protocol, \
         CAST(fees_net AS VARCHAR) AS fees_net, CAST(fee_count AS BIGINT) AS fee_count \
         FROM lodestar_indexer_daily WHERE indexer = '{addr}' AND day_start >= {since} ORDER BY day_start DESC"
    ))
}

/// An indexer's earnings per UTC day from `since` (a day start), oldest first: what it received and
/// what was collected, for rewards and query fees alike. Dated by the event that paid, which is the
/// view's whole reason to exist; nothing here may reach for an allocation's close.
pub fn indexer_revenue_daily_sql(addr: &str, since: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let since = since.max(0);
    Some(format!(
        "SELECT day_start, CAST(indexer_rewards AS VARCHAR) AS indexer_rewards, \
         CAST(total_rewards AS VARCHAR) AS total_rewards, CAST(fees_net AS VARCHAR) AS fees_net, \
         CAST(fees_gross AS VARCHAR) AS fees_gross \
         FROM lodestar_indexer_daily WHERE indexer = '{addr}' AND day_start >= {since} \
         AND (reward_count > 0 OR fee_count > 0) ORDER BY day_start"
    ))
}

/// The same earnings per deployment over the same days, from the view the daily figures are the
/// sum of, so the table and the chart cannot disagree.
pub fn indexer_revenue_by_deployment_sql(addr: &str, since: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let since = since.max(0);
    Some(format!(
        "SELECT deployment, CAST(SUM(indexer_rewards) AS VARCHAR) AS indexer_rewards, \
         CAST(SUM(total_rewards) AS VARCHAR) AS total_rewards, CAST(SUM(fees_net) AS VARCHAR) AS fees_net, \
         CAST(SUM(fees_gross) AS VARCHAR) AS fees_gross \
         FROM lodestar_indexer_deployment_daily WHERE indexer = '{addr}' AND day_start >= {since} \
         GROUP BY deployment"
    ))
}

pub const DELEGATION_RATIO_SQL: &str = "SELECT delegation_ratio FROM lodestar_network_params";

// ---------------------------------------------------------------------------------------------
// The QoS nest (qos-reo-nest). Every statement names exactly one day, `day = <that day>`: the nest
// refuses eight days of these views inside its memory budget. `day` is a DATE there, so a clamped
// unix day number is added to the epoch rather than a date string formatted in.
// ---------------------------------------------------------------------------------------------

const QOS_DAY_PREFIX: &str = "DATE '1970-01-01' + ";

fn qos_day(unix_day: i64) -> String {
    format!("{QOS_DAY_PREFIX}{}", unix_day.max(0))
}

/// The distinct unix days a QoS statement names, so a test can hold every statement to one.
pub fn days_named(sql: &str) -> Vec<i64> {
    let days: std::collections::BTreeSet<i64> = sql
        .split(QOS_DAY_PREFIX)
        .skip(1)
        .filter_map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .collect();
    days.into_iter().collect()
}

const QOS_ROW_COLUMNS: &str = "indexer, deployment, chain, gateway, \
     datediff('day', DATE '1970-01-01', day) AS unix_day, query_count, num_200, latency_ms, \
     blocks_behind, total_query_fees, buckets";

const QOS_INDEXER_DAY_COLUMNS: &str = "indexer, CAST(day AS VARCHAR) AS date, query_count, num_200, \
     success_rate, latency_ms, blocks_behind, total_query_fees, avg_query_fee, allocations, buckets, \
     bad_buckets, worst_bucket_start, worst_bucket_success_rate, worst_bucket_queries";

const QOS_BEHIND_COLUMNS: &str =
    "indexer, seconds_behind_peers, share_queries_over_5min_behind, share_with_block_time";

/// A statement that names no day, for the nest's provenance alone.
pub const QOS_PROVENANCE_SQL: &str = "SELECT count(*) AS publishers FROM qos_publisher";

/// Per topic, the newest bucket the nest holds and, separately, when the publisher last posted.
pub const QOS_FRESHNESS_SQL: &str = "SELECT topic, newest_bucket_start, buckets, last_post_block, \
     last_post_time FROM qos_freshness";

/// What the job compares for a stored day, from `qos_day_resolution`: buckets stored per topic,
/// postings whose document is not stored, and the day's closing block; and, naming no day, the newest
/// bucket the publisher has posted.
pub fn qos_day_probe_sql(unix_day: i64) -> String {
    let d = qos_day(unix_day);
    format!(
        "SELECT (SELECT sum(buckets_stored) FROM qos_day_resolution WHERE day = {d} AND topic = 'indexer') AS indexer_buckets, \
         (SELECT sum(buckets_stored) FROM qos_day_resolution WHERE day = {d} AND topic = 'query') AS query_buckets, \
         (SELECT sum(unresolved_documents) FROM qos_day_resolution WHERE day = {d}) AS unresolved_documents, \
         (SELECT max(closing_block) FROM qos_day_resolution WHERE day = {d}) AS closing_block, \
         (SELECT max(bucket_start) FROM qos_posting) AS newest_posted_bucket_start"
    )
}

/// Every indexer's `qos_indexer_daily` for one day.
pub fn qos_indexer_day_sql(unix_day: i64) -> String {
    format!(
        "SELECT {QOS_INDEXER_DAY_COLUMNS} FROM qos_indexer_daily WHERE day = {}",
        qos_day(unix_day)
    )
}

/// Every indexer's seconds behind its peers for one day. Asked apart from the daily figures because
/// the two joined are the statement the nest ran out of memory on.
pub fn qos_seconds_behind_day_sql(unix_day: i64) -> String {
    format!(
        "SELECT {QOS_BEHIND_COLUMNS} FROM qos_indexer_seconds_behind WHERE day = {}",
        qos_day(unix_day)
    )
}

/// One indexer's `qos_indexer_daily` for one day.
pub fn qos_indexer_daily_sql(addr: &str, unix_day: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT {QOS_INDEXER_DAY_COLUMNS} FROM qos_indexer_daily WHERE indexer = '{addr}' AND day = {}",
        qos_day(unix_day)
    ))
}

/// One indexer's seconds behind its peers for one day.
pub fn qos_indexer_seconds_behind_sql(addr: &str, unix_day: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT {QOS_BEHIND_COLUMNS} FROM qos_indexer_seconds_behind WHERE indexer = '{addr}' AND day = {}",
        qos_day(unix_day)
    ))
}

/// The gateways that measured one indexer on one day.
pub fn qos_indexer_gateways_sql(addr: &str, unix_day: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT CAST(day AS VARCHAR) AS date, string_agg(DISTINCT gateway, ',' ORDER BY gateway) AS gateways \
         FROM qos_allocation_daily WHERE indexer = '{addr}' AND day = {} GROUP BY day",
        qos_day(unix_day)
    ))
}

/// Every indexer's rows for one day.
pub fn qos_network_rows_sql(unix_day: i64) -> String {
    format!(
        "SELECT {QOS_ROW_COLUMNS} FROM qos_allocation_daily WHERE day = {}",
        qos_day(unix_day)
    )
}

/// Open stake per indexer on every deployment `addr` has an open allocation on.
pub fn indexer_allocation_shares_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT LOWER(indexer) AS indexer, subgraph_deployment, CAST(SUM(allocated_tokens) AS VARCHAR) AS allocated_tokens \
         FROM lodestar_allocations WHERE status = 'Active' AND subgraph_deployment IN \
         (SELECT subgraph_deployment FROM lodestar_allocations WHERE LOWER(indexer) = '{addr}' AND status = 'Active') \
         GROUP BY 1, 2"
    ))
}

/// Per deployment and UTC day from `from_unix_day`, the change in the subgraph's `signalledTokens`
/// (deposits net of curation tax, plus query fees collected into the pool, less burns), with every
/// earlier change folded into `from_unix_day - 1`. A running sum then gives signal at each day's end.
pub fn deployment_signal_days_sql(ids: &[String], from_unix_day: i64) -> Option<String> {
    if ids.is_empty() || ids.len() > 200 || !ids.iter().all(|i| is_deployment_id(i)) {
        return None;
    }
    let list = ids
        .iter()
        .map(|i| format!("'{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let before = clamp(from_unix_day, 1, i64::from(i32::MAX)) - 1;
    Some(format!(
        "SELECT dep AS deployment, CAST(GREATEST(ts // 86400, {before}) AS BIGINT) AS unix_day, \
         CAST(SUM(tok) AS VARCHAR) AS delta FROM (\
         SELECT LOWER(\"subgraphDeploymentID\") AS dep, CAST(tokens AS HUGEINT) - CAST(\"curationTax\" AS HUGEINT) AS tok, \
         CAST(block_timestamp AS BIGINT) AS ts FROM curation__signalled \
         UNION ALL SELECT LOWER(\"subgraphDeploymentID\"), -CAST(tokens AS HUGEINT), CAST(block_timestamp AS BIGINT) FROM curation__burned \
         UNION ALL SELECT LOWER(\"subgraphDeploymentID\"), CAST(tokens AS HUGEINT), CAST(block_timestamp AS BIGINT) FROM curation__collected\
         ) WHERE dep IN ({list}) GROUP BY 1, 2"
    ))
}

/// Open stake per indexer and deployment, network-wide.
pub const ALLOCATION_SHARES_ALL_SQL: &str = "SELECT LOWER(indexer) AS indexer, subgraph_deployment, \
     CAST(SUM(allocated_tokens) AS VARCHAR) AS allocated_tokens \
     FROM lodestar_allocations WHERE status = 'Active' GROUP BY 1, 2";

// ---------------------------------------------------------------------------------------------
// Provisions and data services
// ---------------------------------------------------------------------------------------------

const PROVISION_COLS: &str = "p.id, p.indexer, p.data_service, CAST(p.tokens_provisioned AS VARCHAR) AS tokens_provisioned, \
     CAST(COALESCE(p.tokens_allocated, 0) AS VARCHAR) AS tokens_allocated, CAST(p.tokens_thawing AS VARCHAR) AS tokens_thawing, \
     p.max_verifier_cut, p.thawing_period, p.created_at, COALESCE(p.allocation_count, 0) AS allocation_count, \
     CAST(COALESCE(p.rewards_earned, 0) AS VARCHAR) AS rewards_earned, \
     CAST(COALESCE(p.query_fees_collected, 0) AS VARCHAR) AS query_fees_collected";

pub fn provisions_by_indexer_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT {PROVISION_COLS} FROM lodestar_provisions p WHERE p.indexer = '{addr}' \
         AND p.tokens_provisioned > 0 ORDER BY p.tokens_provisioned DESC, p.data_service"
    ))
}

pub fn provisions_by_service_sql(addr: &str, first: i64, skip: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 1000);
    let skip = clamp(skip, 0, 100_000);
    Some(format!(
        "SELECT {PROVISION_COLS}, CAST(i.staked_tokens AS VARCHAR) AS indexer_staked_tokens, \
         CAST(i.delegated_tokens AS VARCHAR) AS indexer_delegated_tokens \
         FROM lodestar_provisions p LEFT JOIN lodestar_indexers i ON i.id = p.indexer \
         WHERE p.data_service = '{addr}' AND p.tokens_provisioned > 0 \
         ORDER BY p.tokens_provisioned DESC, p.indexer LIMIT {first} OFFSET {skip}"
    ))
}

pub fn data_service_totals_sql(addrs: &[String]) -> Option<String> {
    if addrs.is_empty() || addrs.len() > 100 || !addrs.iter().all(|a| is_address(a)) {
        return None;
    }
    let list = addrs
        .iter()
        .map(|a| format!("'{a}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "SELECT data_service, CAST(SUM(tokens_provisioned) AS VARCHAR) AS total_tokens_provisioned, \
         CAST(SUM(COALESCE(tokens_allocated, 0)) AS VARCHAR) AS total_tokens_allocated \
         FROM lodestar_provisions WHERE data_service IN ({list}) GROUP BY 1"
    ))
}

/// The newest sampled `maxThawingPeriod` from HorizonStaking.
///
/// A sampled `eth_call` rather than an event, so the nest stores the raw ABI return in `result`
/// and the caller decodes it. One value serves both the minimum and the maximum: the incumbent
/// reports the same figure for each, because the protocol has one.
pub const MAX_THAWING_PERIOD_SQL: &str =
    "SELECT result FROM max_thawing_period WHERE NOT reverted ORDER BY block_number DESC LIMIT 1";

/// An indexer's provision thaw requests that have not been fulfilled, oldest unlock first.
pub fn provision_thaw_requests_sql(addr: &str) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    Some(format!(
        "SELECT LOWER(c.verifier) AS data_service, c.\"thawRequestId\" AS id, c.shares, c.\"thawingUntil\" AS thawing_until, \
         c.nonce, c.block_timestamp AS created_at, c.tx_hash FROM staking__thaw_request_created c \
         WHERE c.\"requestType\" = '0' AND LOWER(c.\"serviceProvider\") = '{addr}' \
         AND NOT EXISTS (SELECT 1 FROM staking__thaw_request_fulfilled f WHERE f.\"thawRequestId\" = c.\"thawRequestId\") \
         ORDER BY CAST(c.\"thawingUntil\" AS BIGINT), c.block_number, c.log_index"
    ))
}

/// An indexer's own Horizon events: provisions, their thaw requests, slashes and fee cuts.
pub fn indexer_horizon_activity_sql(addr: &str, first: i64) -> Option<String> {
    if !is_address(addr) {
        return None;
    }
    let first = clamp(first, 1, 500);
    let sp = format!("LOWER(\"serviceProvider\") = '{addr}'");
    Some(format!(
        "WITH created AS (SELECT * FROM staking__thaw_request_created WHERE \"requestType\" = '0' AND {sp}) \
         SELECT 'ProvisionCreated' AS kind, LOWER(verifier) AS data_service, tokens, NULL AS shares, NULL AS thawing_until, \
         NULL AS thaw_request_id, NULL AS valid, NULL AS payment_type, NULL AS fee_cut, 1 AS events, block_number, log_index, block_timestamp, tx_hash \
         FROM staking__provision_created WHERE {sp} \
         UNION ALL SELECT 'ProvisionIncreased', LOWER(verifier), CAST(SUM(CAST(tokens AS HUGEINT)) AS VARCHAR), NULL, NULL, NULL, NULL, NULL, NULL, \
         COUNT(*), block_number, MAX(log_index), block_timestamp, tx_hash \
         FROM staking__provision_increased WHERE {sp} GROUP BY LOWER(verifier), block_number, block_timestamp, tx_hash \
         UNION ALL SELECT 'ProvisionThawed', LOWER(verifier), tokens, NULL, NULL, NULL, NULL, NULL, NULL, 1, block_number, log_index, block_timestamp, tx_hash \
         FROM staking__provision_thawed WHERE {sp} \
         UNION ALL SELECT 'TokensDeprovisioned', LOWER(verifier), tokens, NULL, NULL, NULL, NULL, NULL, NULL, 1, block_number, log_index, block_timestamp, tx_hash \
         FROM staking__tokens_deprovisioned WHERE {sp} \
         UNION ALL SELECT 'ProvisionSlashed', LOWER(verifier), tokens, NULL, NULL, NULL, NULL, NULL, NULL, 1, block_number, log_index, block_timestamp, tx_hash \
         FROM staking__provision_slashed WHERE {sp} \
         UNION ALL SELECT 'ThawRequestCreated', LOWER(verifier), NULL, shares, \"thawingUntil\", \"thawRequestId\", NULL, NULL, NULL, 1, block_number, log_index, block_timestamp, tx_hash \
         FROM created \
         UNION ALL SELECT 'ThawRequestFulfilled', LOWER(c.verifier), f.tokens, f.shares, f.\"thawingUntil\", f.\"thawRequestId\", f.valid, NULL, NULL, 1, \
         f.block_number, f.log_index, f.block_timestamp, f.tx_hash \
         FROM staking__thaw_request_fulfilled f JOIN created c ON c.\"thawRequestId\" = f.\"thawRequestId\" \
         UNION ALL SELECT 'DelegationFeeCutSet', LOWER(verifier), NULL, NULL, NULL, NULL, NULL, \"paymentType\", \"feeCut\", 1, block_number, log_index, block_timestamp, tx_hash \
         FROM staking__delegation_fee_cut_set WHERE {sp} \
         ORDER BY block_number DESC, log_index DESC LIMIT {first}"
    ))
}

pub const DATA_SERVICE_COUNTS_SQL: &str = "SELECT indexer, COUNT(DISTINCT data_service) AS n FROM lodestar_provisions WHERE tokens_provisioned > 0 GROUP BY 1";

// ---------------------------------------------------------------------------------------------
// Proof of indexing
// ---------------------------------------------------------------------------------------------

/// Closed allocations carrying a real proof, with the deployment's active stake alongside, so the
/// consensus view can weight each proof by what stands behind it.
pub fn poi_allocations_sql(deployment: Option<&str>, first: i64) -> Option<String> {
    let dep = match deployment {
        None => String::new(),
        Some(d) if is_deployment_id(d) => format!(" AND a.subgraph_deployment = '{d}'"),
        Some(_) => return None,
    };
    let first = clamp(first, 1, 2000);
    Some(format!(
        "WITH staked AS (SELECT subgraph_deployment, CAST(SUM(CAST(allocated_tokens AS HUGEINT)) AS VARCHAR) AS staked \
         FROM lodestar_allocations WHERE status = 'Active' GROUP BY 1) \
         SELECT a.id, a.poi, a.indexer, CAST(a.allocated_tokens AS VARCHAR) AS allocated_tokens, a.closed_at_epoch, a.closed_at, \
         a.subgraph_deployment, CAST(a.signalled_tokens AS VARCHAR) AS signalled_tokens, COALESCE(s.staked, '0') AS staked_tokens \
         FROM lodestar_allocations a LEFT JOIN staked s ON s.subgraph_deployment = a.subgraph_deployment \
         WHERE a.status = 'Closed' AND a.poi IS NOT NULL AND a.poi <> '{ZERO_POI}'{dep} \
         ORDER BY a.closed_at DESC, a.id DESC LIMIT {first}"
    ))
}

// ---------------------------------------------------------------------------------------------
// Payments: escrow and tally
// ---------------------------------------------------------------------------------------------

/// Escrow balances, folded from deposits, withdrawals and collections, with the newest thaw state
/// joined on. A balance is a fold over events; there is no balance table to read.
pub fn escrow_accounts_sql(receiver: Option<&str>, first: i64) -> Option<String> {
    let rc = receiver_clause("receiver", receiver)?;
    let first = clamp(first, 1, 500);
    let having = if receiver.is_some() {
        ""
    } else {
        "HAVING SUM(mv.d) > 0 "
    };
    Some(format!(
        "WITH mv AS (\
         SELECT LOWER(payer) p, LOWER(collector) c, LOWER(receiver) r, CAST(tokens AS HUGEINT) d FROM escrow__deposit WHERE 1=1{rc} \
         UNION ALL SELECT LOWER(payer), LOWER(collector), LOWER(receiver), -CAST(tokens AS HUGEINT) FROM escrow__withdraw WHERE 1=1{rc} \
         UNION ALL SELECT LOWER(payer), LOWER(collector), LOWER(receiver), -CAST(tokens AS HUGEINT) FROM escrow__escrow_collected WHERE 1=1{rc}\
         ), th AS (\
         SELECT LOWER(payer) p, LOWER(collector) c, LOWER(receiver) r, block_number, log_index, CAST(tokens AS HUGEINT) t, CAST(\"thawEndTimestamp\" AS HUGEINT) te FROM escrow__thaw WHERE 1=1{rc} \
         UNION ALL SELECT LOWER(payer), LOWER(collector), LOWER(receiver), block_number, log_index, 0, 0 FROM escrow__cancel_thaw WHERE 1=1{rc} \
         UNION ALL SELECT LOWER(payer), LOWER(collector), LOWER(receiver), block_number, log_index, 0, 0 FROM escrow__withdraw WHERE 1=1{rc}\
         ), last_th AS (\
         SELECT p, c, r, t, te FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY p, c, r ORDER BY block_number DESC, log_index DESC) rn FROM th) WHERE rn = 1\
         ) \
         SELECT mv.p AS payer, mv.c AS collector, mv.r AS receiver, CAST(SUM(mv.d) AS VARCHAR) AS balance, \
         CAST(COALESCE(MAX(lt.t), 0) AS VARCHAR) AS thawing, CAST(COALESCE(MAX(lt.te), 0) AS VARCHAR) AS thaw_end \
         FROM mv LEFT JOIN last_th lt ON lt.p = mv.p AND lt.c = mv.c AND lt.r = mv.r \
         GROUP BY 1, 2, 3 {having}ORDER BY SUM(mv.d) DESC, 1, 2, 3 LIMIT {first}"
    ))
}

pub fn escrow_transactions_sql(receiver: Option<&str>, first: i64) -> Option<String> {
    let rc = receiver_clause("receiver", receiver)?;
    let first = clamp(first, 1, 500);
    let ev = |table: &str, ty: &str| {
        format!(
            "SELECT tx_hash, log_index, block_timestamp, LOWER(payer) AS payer, LOWER(receiver) AS receiver, \
             CAST(tokens AS VARCHAR) AS amount, '{ty}' AS type, LOWER(collector) AS collector FROM {table} WHERE 1=1{rc}"
        )
    };
    let cancel = format!(
        "SELECT tx_hash, log_index, block_timestamp, LOWER(payer), LOWER(receiver), CAST(\"tokensThawing\" AS VARCHAR), 'cancel_thaw', LOWER(collector) FROM escrow__cancel_thaw WHERE 1=1{rc}"
    );
    let union = [
        ev("escrow__deposit", "deposit"),
        ev("escrow__thaw", "thaw"),
        cancel,
        ev("escrow__withdraw", "withdraw"),
        ev("escrow__escrow_collected", "redeem"),
    ]
    .join(" UNION ALL ");
    Some(format!(
        "WITH ev AS ({union}), \
         newest AS (SELECT * FROM ev ORDER BY block_timestamp DESC, tx_hash DESC, log_index DESC LIMIT {first}), \
         fees AS (\
         SELECT tx_hash, LOWER(payer) AS payer, LOWER(\"serviceProvider\") AS receiver, CAST(\"tokensCollected\" AS VARCHAR) AS amount, \"allocationId\" AS allocation_id, \
         ROW_NUMBER() OVER (PARTITION BY tx_hash, LOWER(payer), LOWER(\"serviceProvider\"), CAST(\"tokensCollected\" AS VARCHAR) ORDER BY log_index) AS rn \
         FROM subgraph_service__query_fees_collected WHERE tx_hash IN (SELECT tx_hash FROM newest)\
         ), ranked AS (\
         SELECT n.*, ROW_NUMBER() OVER (PARTITION BY tx_hash, payer, receiver, amount ORDER BY log_index) AS rn FROM newest n WHERE type = 'redeem' AND collector = '{TALLY_COLLECTOR}'\
         ) \
         SELECT n.tx_hash, n.log_index, n.block_timestamp, n.payer, n.receiver, n.amount, n.type, f.allocation_id \
         FROM newest n LEFT JOIN ranked r ON r.tx_hash = n.tx_hash AND r.log_index = n.log_index \
         LEFT JOIN fees f ON f.tx_hash = r.tx_hash AND f.payer = r.payer AND f.receiver = r.receiver AND f.amount = r.amount AND f.rn = r.rn \
         ORDER BY n.block_timestamp DESC, n.tx_hash DESC, n.log_index DESC"
    ))
}

pub fn tally_collected_sql(receiver: Option<&str>, first: i64) -> Option<String> {
    let rc = receiver_clause("receiver", receiver)?;
    let first = clamp(first, 1, 500);
    Some(format!(
        "SELECT LOWER(payer) AS payer, LOWER(receiver) AS receiver, \"collectionId\" AS collection_id, \
         CAST(SUM(CAST(tokens AS HUGEINT)) AS VARCHAR) AS tokens FROM tally__payment_collected WHERE 1=1{rc} \
         GROUP BY 1, 2, 3 ORDER BY SUM(CAST(tokens AS HUGEINT)) DESC, 1, 2, 3 LIMIT {first}"
    ))
}

/// Every payer's collections, summed with no cap. A total taken from `tally_collected_sql` sums only
/// the largest collections, which understated one indexer by half and the network by two thirds.
pub fn tally_by_payer_sql(receiver: Option<&str>) -> Option<String> {
    let rc = receiver_clause("receiver", receiver)?;
    Some(format!(
        "SELECT LOWER(payer) AS payer, COUNT(DISTINCT \"collectionId\") AS collections, \
         CAST(SUM(CAST(tokens AS HUGEINT)) AS VARCHAR) AS tokens FROM tally__payment_collected WHERE 1=1{rc} \
         GROUP BY 1 ORDER BY SUM(CAST(tokens AS HUGEINT)) DESC, 1"
    ))
}

// ---------------------------------------------------------------------------------------------
// Deployments and curation
// ---------------------------------------------------------------------------------------------

const DEPLOYMENT_COLS: &str = "id, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, CAST(staked_tokens AS VARCHAR) AS staked_tokens, \
     CAST(query_fees_amount AS VARCHAR) AS query_fees_amount, created_at, active_allocation_count, curator_count";

pub fn deployments_order_column(order_by: &str) -> &'static str {
    match order_by {
        "stakedTokens" => "staked_tokens",
        "queryFeesAmount" => "query_fees_amount",
        "createdAt" => "created_at",
        _ => "signalled_tokens",
    }
}

/// The directory. The signal floor of one GRT is the nest view's own, not a display filter: below
/// it the row is noise rather than a subgraph anyone is serving.
pub fn deployments_list_sql(first: i64, skip: i64, order_by: &str, descending: bool) -> String {
    let col = deployments_order_column(order_by);
    let dir = if descending { "DESC" } else { "ASC" };
    let first = clamp(first, 1, 1000);
    let skip = clamp(skip, 0, 100_000);
    format!(
        "SELECT {DEPLOYMENT_COLS} FROM lodestar_deployments d WHERE d.signalled_tokens > 1000000000000000000 \
         ORDER BY d.{col} {dir}, d.id LIMIT {first} OFFSET {skip}"
    )
}

/// Every deployment the directory lists, under the same floor as [`deployments_list_sql`].
pub fn directory_sql() -> String {
    format!(
        "SELECT {DEPLOYMENT_COLS} FROM lodestar_deployments WHERE signalled_tokens > 1000000000000000000"
    )
}

pub fn deployments_by_id_sql(ids: &[String]) -> Option<String> {
    if ids.is_empty() || ids.len() > 500 || !ids.iter().all(|i| is_deployment_id(i)) {
        return None;
    }
    let list = ids
        .iter()
        .map(|i| format!("'{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "SELECT {DEPLOYMENT_COLS} FROM lodestar_deployments WHERE id IN ({list})"
    ))
}

pub const DEPLOYMENT_IDS_SQL: &str =
    "SELECT id FROM lodestar_deployments WHERE signalled_tokens > 0 OR staked_tokens > 0";

pub fn deployment_sql(dep: &str) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    Some(format!(
        "SELECT subgraph_deployment, CAST(MAX(signalled_tokens) AS VARCHAR) AS signalled_tokens, \
         CAST(SUM(allocated_tokens) FILTER (WHERE status = 'Active') AS VARCHAR) AS staked_tokens, COUNT(*) AS allocations \
         FROM lodestar_allocations WHERE LOWER(subgraph_deployment) = '{dep}' GROUP BY 1"
    ))
}

pub fn allocations_by_deployment_sql(dep: &str, first: i64) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    let first = clamp(first, 1, 500);
    Some(format!(
        "SELECT LOWER(a.indexer) AS indexer, i.url, CAST(a.allocated_tokens AS VARCHAR) AS allocated_tokens \
         FROM lodestar_allocations a LEFT JOIN lodestar_indexers i ON i.id = LOWER(a.indexer) \
         WHERE LOWER(a.subgraph_deployment) = '{dep}' AND a.status = 'Active' \
         ORDER BY a.allocated_tokens DESC, a.indexer LIMIT {first}"
    ))
}

pub fn deployment_signals_sql(dep: &str, first: i64) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    let first = clamp(first, 1, 1000);
    Some(format!(
        "SELECT id, curator, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(unsignalled_tokens AS VARCHAR) AS unsignalled_tokens, CAST(signal AS VARCHAR) AS signal, \
         last_signal_change, CAST(realized_rewards AS VARCHAR) AS realized_rewards, \
         CAST(deployment_signalled_tokens AS VARCHAR) AS deployment_signalled_tokens, \
         CAST(deployment_query_fees_amount AS VARCHAR) AS deployment_query_fees_amount \
         FROM lodestar_curator_signals s WHERE LOWER(s.subgraph_deployment) = '{dep}' AND s.signal > 0 \
         ORDER BY s.signalled_tokens DESC, s.curator LIMIT {first}"
    ))
}

pub fn deployment_signal_transactions_sql(dep: &str, first: i64) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    let first = clamp(first, 1, 5000);
    Some(format!(
        "SELECT CAST(block_timestamp AS BIGINT) AS timestamp, 'MintSignal' AS type, \
         CAST(CAST(tokens AS HUGEINT) - CAST(\"curationTax\" AS HUGEINT) AS VARCHAR) AS tokens \
         FROM curation__signalled WHERE LOWER(\"subgraphDeploymentID\") = '{dep}' \
         UNION ALL SELECT CAST(block_timestamp AS BIGINT), 'BurnSignal', CAST(tokens AS VARCHAR) \
         FROM curation__burned WHERE LOWER(\"subgraphDeploymentID\") = '{dep}' ORDER BY timestamp LIMIT {first}"
    ))
}

pub fn deployment_allocations_history_sql(dep: &str, first: i64) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    let first = clamp(first, 1, 5000);
    Some(format!(
        "SELECT CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, created_at, closed_at FROM lodestar_allocations \
         WHERE LOWER(subgraph_deployment) = '{dep}' ORDER BY created_at LIMIT {first}"
    ))
}

pub fn deployment_fees_since_sql(since: i64, first: i64) -> String {
    let first = clamp(first, 1, 1000);
    format!(
        "SELECT LOWER(subgraph_deployment) AS id, CAST(SUM(query_fees_collected) AS VARCHAR) AS query_fees \
         FROM lodestar_allocations WHERE status = 'Closed' AND closed_at >= {since} GROUP BY 1 \
         HAVING SUM(query_fees_collected) > 0 ORDER BY SUM(query_fees_collected) DESC LIMIT {first}"
    )
}

/// As [`deployment_fees_since_sql`], for every deployment that earned: the directory filters and
/// sorts on the figure, so the largest two hundred are not enough.
pub fn deployment_fees_window_sql(since: i64) -> String {
    format!(
        "SELECT LOWER(subgraph_deployment) AS id, CAST(SUM(query_fees_collected) AS VARCHAR) AS query_fees \
         FROM lodestar_allocations WHERE status = 'Closed' AND closed_at >= {since} GROUP BY 1 \
         HAVING SUM(query_fees_collected) > 0"
    )
}

pub fn deployment_serving_indexers_sql(dep: &str, first: i64) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    let first = clamp(first, 1, 100);
    Some(format!(
        "SELECT a.indexer AS id, i.url FROM lodestar_allocations a JOIN lodestar_indexers i ON i.id = a.indexer \
         WHERE a.status = 'Active' AND LOWER(a.subgraph_deployment) = '{dep}' \
         ORDER BY a.allocated_tokens DESC LIMIT {first}"
    ))
}

pub fn indexer_url_sql(indexer: &str) -> Option<String> {
    if !is_address(indexer) {
        return None;
    }
    Some(format!(
        "SELECT url FROM lodestar_indexers WHERE id = '{indexer}'"
    ))
}

// ---------------------------------------------------------------------------------------------
// Whole-network folds, used by the refresh and ingest jobs
// ---------------------------------------------------------------------------------------------

pub const INDEXERS_ALL_SQL: &str = "SELECT id, CAST(staked_tokens AS VARCHAR) AS staked_tokens, CAST(locked_tokens AS VARCHAR) AS locked_tokens, locked_until, \
     CAST(delegated_tokens AS VARCHAR) AS delegated_tokens, CAST(delegated_thawing_tokens AS VARCHAR) AS delegated_thawing_tokens, \
     CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, allocation_count, indexing_reward_cut, query_fee_cut, \
     last_delegation_parameter_update, CAST(rewards_earned AS VARCHAR) AS rewards_earned, \
     CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, CAST(delegator_shares AS VARCHAR) AS delegator_shares, \
     CAST(provisioned_tokens AS VARCHAR) AS provisioned_tokens, url, geohash, created_at \
     FROM lodestar_indexers WHERE staked_tokens > 0 AND id <> '0xb43b2cccceada5292732a8c58ae134adefce09bb' ORDER BY id";

pub const ACTIVE_ALLOCATIONS_ALL_SQL: &str = "SELECT a.id, LOWER(a.indexer) AS indexer, CAST(a.allocated_tokens AS VARCHAR) AS allocated_tokens, \
     CAST(a.signalled_tokens AS VARCHAR) AS signalled_tokens, CAST(d.staked AS VARCHAR) AS deployment_staked_tokens \
     FROM lodestar_allocations a \
     JOIN (SELECT subgraph_deployment, SUM(allocated_tokens) AS staked FROM lodestar_allocations WHERE status = 'Active' GROUP BY 1) d \
     ON d.subgraph_deployment = a.subgraph_deployment WHERE a.status = 'Active'";

pub fn delegation_events_since_sql(since: i64) -> String {
    format!(
        "SELECT event_type, indexer, CAST(tokens AS VARCHAR) AS tokens FROM lodestar_delegations \
         WHERE timestamp > {since} AND event_type IN ('delegation', 'undelegation')"
    )
}

/// The instant the frozen archive's history ends and the live nest's begins.
///
/// The two windows are disjoint by construction, so no event is counted twice and none is missed.
pub const LEGACY_HISTORY_END_EXCLUSIVE: i64 = 1_787_555_748;

/// Daily delegation inflow and outflow, **aggregated by the nest**.
///
/// This must never fetch raw events. There are hundreds of thousands of them and the nest caps a
/// result at 50,000 rows, so a raw scan comes back truncated and the daily sums are quietly short -
/// a chart that looks plausible and is wrong. Grouping in SQL returns one row per day, which no
/// window can overflow, and the sums are computed where the data is.
///
/// `legacy` picks the schema: the frozen archive carries both the pre-Horizon `staking_legacy__`
/// tables and the Horizon ones, while the live nest has only the latter.
///
/// The cast is from `tokens`, the exact text, not from `tokens_dec`: `_dec` is NULL past 38 digits,
/// so a sum over it would drop those rows without a word, which is why Burrmill refuses to sum a
/// `TRY_CAST` value (nuthatch shadow replay, 2026-09-27). A `DOUBLE` reads the text whole.
pub fn daily_flow_sql(start: i64, end: i64, legacy: bool) -> String {
    let events = if legacy {
        "SELECT block_timestamp, CAST(tokens AS DOUBLE) / 1e18 AS tokens, 'inflow' AS direction          FROM staking_legacy__stake_delegated          UNION ALL SELECT block_timestamp, CAST(tokens AS DOUBLE) / 1e18, 'outflow'          FROM staking_legacy__stake_delegated_locked          UNION ALL SELECT block_timestamp, CAST(tokens AS DOUBLE) / 1e18, 'inflow'          FROM staking__tokens_delegated          UNION ALL SELECT block_timestamp, CAST(tokens AS DOUBLE) / 1e18, 'outflow'          FROM staking__tokens_undelegated"
    } else {
        "SELECT block_timestamp, CAST(tokens AS DOUBLE) / 1e18 AS tokens, 'inflow' AS direction          FROM staking__tokens_delegated          UNION ALL SELECT block_timestamp, CAST(tokens AS DOUBLE) / 1e18, 'outflow'          FROM staking__tokens_undelegated"
    };
    format!(
        "WITH events AS ({events})          SELECT strftime(to_timestamp(block_timestamp), '%Y-%m-%d') AS date,          SUM(CASE WHEN direction = 'inflow' THEN tokens ELSE 0 END) AS inflows,          SUM(CASE WHEN direction = 'outflow' THEN tokens ELSE 0 END) AS outflows          FROM events WHERE block_timestamp >= {start} AND block_timestamp < {end}          GROUP BY 1 ORDER BY 1"
    )
}

pub fn closed_allocations_since_sql(since: i64) -> String {
    format!(
        "SELECT LOWER(indexer) AS indexer, CAST(indexing_delegator_rewards AS VARCHAR) AS indexing_delegator_rewards, closed_at \
         FROM lodestar_allocations WHERE status = 'Closed' AND closed_at >= {since} AND indexing_delegator_rewards > 0"
    )
}

pub fn exchange_rates_as_of_sql(cutoff: i64) -> String {
    format!(
        "SELECT indexer, CAST(SUM(pool_delta) AS VARCHAR) AS pool_tokens, CAST(SUM(shares_delta) AS VARCHAR) AS pool_shares \
         FROM lodestar_indexer_ledger WHERE ts <= {cutoff} GROUP BY 1 HAVING SUM(shares_delta) > 0"
    )
}

/// The newest provisions, for the activity feed.
pub fn newest_provisions_sql(first: i64) -> String {
    let first = clamp(first, 1, 200);
    format!(
        "SELECT tx_hash, log_index, block_number, block_timestamp, LOWER(\"serviceProvider\") AS indexer, \
         LOWER(verifier) AS verifier, CAST(tokens AS VARCHAR) AS tokens FROM staking__provision_created \
         ORDER BY block_number DESC, log_index DESC LIMIT {first}"
    )
}

// ---------------------------------------------------------------------------------------------
// GNS (the `/gns` nest)
// ---------------------------------------------------------------------------------------------

/// Subgraphs published per week, which is the developer-activity chart.
pub fn subgraphs_published_since_sql(cutoff: i64) -> String {
    format!(
        "SELECT block_timestamp AS \"createdAt\" FROM \"gns__subgraph_published\" \
         WHERE block_timestamp >= {cutoff} ORDER BY block_timestamp"
    )
}

pub fn deployment_subgraph_sql(dep: &str) -> Option<String> {
    if !is_deployment_id(dep) {
        return None;
    }
    Some(format!(
        "SELECT subgraph_id FROM deployment_subgraphs WHERE LOWER(deployment_id) = '{dep}' \
         ORDER BY created_at DESC, subgraph_id LIMIT 1"
    ))
}

/// A subgraph's versions, newest first.
///
/// The id comes from `deployment_subgraph_sql` rather than from a caller, and is checked anyway:
/// a builder that trusts its input because of where it was called from stops being true the first
/// time somebody calls it from somewhere else.
pub fn subgraph_versions_sql(subgraph_id: &str, limit: i64) -> Option<String> {
    if !is_subgraph_id(subgraph_id) || !(1..=1000).contains(&limit) {
        return None;
    }
    Some(format!(
        "SELECT deployment_id, version_metadata, version_number, created_at \
         FROM subgraph_versions WHERE subgraph_id = '{subgraph_id}' \
         ORDER BY version_number DESC LIMIT {limit}"
    ))
}

/// Signal and active stake for a set of deployments, in one pass.
///
/// `MAX(signalled_tokens)` and a filtered `SUM` of allocations, matching the shape the single
/// deployment view uses, so a version row and a deployment page cannot disagree about the same
/// deployment.
pub fn deployments_signal_stake_sql(ids: &[String]) -> Option<String> {
    if ids.is_empty() || ids.len() > 200 || !ids.iter().all(|i| is_deployment_id(i)) {
        return None;
    }
    let list = ids
        .iter()
        .map(|i| format!("'{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "SELECT LOWER(subgraph_deployment) AS subgraph_deployment, \
         CAST(MAX(signalled_tokens) AS VARCHAR) AS signalled_tokens, \
         CAST(COALESCE(SUM(allocated_tokens) FILTER (WHERE status = 'Active'), 0) AS VARCHAR) AS staked_tokens \
         FROM lodestar_allocations WHERE LOWER(subgraph_deployment) IN ({list}) GROUP BY 1"
    ))
}

pub const SUBGRAPH_METADATA_SQL: &str = "SELECT subgraph_metadata FROM subgraph_current WHERE NOT deprecated AND subgraph_metadata IS NOT NULL";

// ---------------------------------------------------------------------------------------------
// DIPS (the `/dips` nests)
// ---------------------------------------------------------------------------------------------

pub const DIPS_CURRENT_ALLOCATION_SQL: &str = "SELECT * FROM dips_current_allocation";
pub const DIPS_TIMELINE_SQL: &str = "SELECT * FROM dips_timeline ORDER BY block_number";

/// The newest distribution per target. A join against the per-target maximum block, because the
/// question is "when was this target last paid", not "every time it was ever paid".
pub fn dips_latest_per_target_sql(table: &str) -> Option<String> {
    // An allowlist, not interpolation: these two table names are the only ones this shape fits.
    const ALLOWED: [&str; 2] = [
        "issuance_allocator__issuance_distributed",
        "issuance_allocator__issuance_self_mint_allowance",
    ];
    if !ALLOWED.contains(&table) {
        return None;
    }
    Some(format!(
        "SELECT d.target, d.block_number, d.block_timestamp, d.amount_dec FROM \"{table}\" d \
         JOIN (SELECT target AS t, max(block_number) AS bn FROM \"{table}\" GROUP BY target) l \
         ON d.target = l.t AND d.block_number = l.bn"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_l1_epochs_statement_is_the_legacy_one_plus_the_range() {
        let l1 = super::epochs_l1_sql(20);
        assert!(
            l1.starts_with(
                "SELECT id, start_block, end_block, start_l1_block, end_l1_block, CAST("
            )
        );
        assert_eq!(
            l1.replace(" start_l1_block, end_l1_block,", ""),
            super::epochs_sql(20)
        );
    }

    #[test]
    fn a_nest_without_the_l1_columns_is_recognised_by_its_refusal() {
        // The first line of 4.7.0's answer from a nest indexed before `l1_blocks`, as `nest_reason` keeps it.
        let refusal = "plan error: Binder Error: Referenced column \"start_l1_block\" not found in FROM clause!";
        assert!(super::lacks_epoch_l1(refusal));
        assert!(!super::lacks_epoch_l1(
            "plan error: Binder Error: Referenced column \"start_block\" not found in FROM clause!"
        ));
        assert!(!super::lacks_epoch_l1("server busy"));
    }

    #[test]
    fn horizon_detail_sql_refuses_what_is_not_an_address() {
        assert!(provision_thaw_requests_sql("0x0e91' OR 1=1 --").is_none());
        assert!(indexer_horizon_activity_sql("nope", 10).is_none());
        let a = "0x0e9193def3016ade132afbfc3f9a3018813700a0";
        let thaws = provision_thaw_requests_sql(a).unwrap();
        assert!(
            thaws.contains("\"requestType\" = '0'"),
            "provision thaws only"
        );
        assert!(
            thaws.contains("NOT EXISTS"),
            "fulfilled requests are not pending"
        );
        assert!(
            indexer_horizon_activity_sql(a, 100_000)
                .unwrap()
                .ends_with("LIMIT 500")
        );
    }

    #[test]
    fn a_deployment_id_is_thirty_two_bytes_of_lower_hex() {
        assert!(is_deployment_id(
            "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
        ));
        assert!(!is_deployment_id(
            "0xABCDEF1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
        ));
        assert!(!is_deployment_id("0xdeadbeef"));
    }

    #[test]
    fn an_indexers_delegators_are_summed_at_the_source_and_never_folded() {
        let addr = "0xf92f430dd8567b0d466358c79594ab58d919a6d4";
        let sql = indexer_delegators_sql(addr, 100).unwrap();
        assert!(!sql.contains("lodestar_delegator_stakes"), "{sql}");
        for source in [
            "FROM staking_legacy__stake_delegated WHERE LOWER(indexer) = ",
            "FROM staking_legacy__stake_delegated_locked WHERE LOWER(indexer) = ",
            "FROM staking__tokens_delegated WHERE LOWER(\"serviceProvider\") = ",
            "FROM staking__tokens_undelegated WHERE LOWER(\"serviceProvider\") = ",
        ] {
            assert!(
                sql.contains(&format!("{source}'{addr}'")),
                "missing {source}"
            );
        }
        assert!(sql.contains("WHERE share_amount > 0"));
        assert!(sql.ends_with("LIMIT 100"));
    }

    /// 106,652 positions on the largest indexer (2026-09-23). The page is cut by the nest and the
    /// fold behind `lodestar_delegator_stakes` is never asked for.
    #[test]
    fn every_delegator_is_paged_at_the_source() {
        let addr = "0xf92f430dd8567b0d466358c79594ab58d919a6d4";
        let sql = indexer_delegators_page_sql(addr, 25, 50, "lastChangeAt", false).unwrap();
        assert!(!sql.contains("lodestar_delegator_stakes"), "{sql}");
        for source in [
            "FROM staking_legacy__stake_delegated WHERE LOWER(indexer) = ",
            "FROM staking_legacy__stake_delegated_locked WHERE LOWER(indexer) = ",
            "FROM staking__tokens_delegated WHERE LOWER(\"serviceProvider\") = ",
            "FROM staking__tokens_undelegated WHERE LOWER(\"serviceProvider\") = ",
            "FROM staking__stake_delegated_withdrawn WHERE LOWER(indexer) = ",
            "FROM staking__delegated_tokens_withdrawn WHERE LOWER(\"serviceProvider\") = ",
            "FROM lodestar_indexer_pool WHERE id = ",
        ] {
            assert!(
                sql.contains(&format!("{source}'{addr}'")),
                "missing {source}"
            );
        }
        assert!(sql.contains("COUNT(*) OVER () AS total"));
        assert!(sql.ends_with(
            "ORDER BY p.last_change_at ASC NULLS LAST, p.delegator ASC LIMIT 25 OFFSET 50"
        ));
        // The nest takes the statement in the URL and refuses a request line over 16 KB.
        assert!(sql.len() < 8_192, "{} bytes", sql.len());
    }

    #[test]
    fn the_delegator_sort_is_an_allowlist() {
        let addr = "0xf92f430dd8567b0d466358c79594ab58d919a6d4";
        for (asked, column) in [
            ("stake", "share_amount"),
            ("share", "share_amount"),
            ("delegatedAt", "delegated_at"),
            ("lastChangeAt", "last_change_at"),
            ("thawingTokens", "thawing_tokens"),
            ("1; DROP TABLE x", "share_amount"),
        ] {
            let sql = indexer_delegators_page_sql(addr, 10, 0, asked, true).unwrap();
            assert!(
                sql.contains(&format!("ORDER BY p.{column} DESC NULLS LAST")),
                "{asked} did not sort by {column}"
            );
        }
        let sql = indexer_delegators_page_sql(addr, 1_000_000, -5, "stake", true).unwrap();
        assert!(sql.ends_with("LIMIT 1000 OFFSET 0"), "{sql}");
    }

    /// Every builder that takes an identifier must refuse rather than interpolate. This walks the
    /// whole surface with one hostile value, so a new builder added without validation is caught
    /// here rather than in production.
    #[test]
    fn no_builder_interpolates_an_unvalidated_identifier() {
        let bad = "' OR 1=1 --";
        assert!(curator_sql(bad).is_none());
        assert!(curator_signals_sql(bad, 10).is_none());
        assert!(delegator_sql(bad).is_none());
        assert!(delegator_stakes_sql(bad, 10, false).is_none());
        assert!(indexer_detail_sql(bad).is_none());
        assert!(indexer_operators_sql(bad).is_none());
        assert!(indexer_delegators_sql(bad, 10).is_none());
        assert!(indexer_delegators_page_sql(bad, 10, 0, "stake", true).is_none());
        assert!(indexer_active_allocations_sql(bad).is_none());
        assert!(indexer_closed_allocations_sql(bad, 10).is_none());
        assert!(indexer_stake_history_sql(bad, &[1, 2]).is_none());
        assert!(indexer_daily_sql(bad, 0).is_none());
        assert!(indexer_revenue_daily_sql(bad, 0).is_none());
        assert!(indexer_revenue_by_deployment_sql(bad, 0).is_none());
        assert!(qos_indexer_daily_sql(bad, 0).is_none());
        assert!(qos_indexer_seconds_behind_sql(bad, 0).is_none());
        assert!(qos_indexer_gateways_sql(bad, 0).is_none());
        assert!(indexer_allocation_shares_sql(bad).is_none());
        assert!(provisions_by_indexer_sql(bad).is_none());
        assert!(provisions_by_service_sql(bad, 10, 0).is_none());
        assert!(data_service_totals_sql(&[bad.to_string()]).is_none());
        assert!(poi_allocations_sql(Some(bad), 10).is_none());
        assert!(escrow_accounts_sql(Some(bad), 10).is_none());
        assert!(escrow_transactions_sql(Some(bad), 10).is_none());
        assert!(tally_collected_sql(Some(bad), 10).is_none());
        assert!(tally_by_payer_sql(Some(bad)).is_none());
        assert!(deployments_by_id_sql(&[bad.to_string()]).is_none());
        assert!(deployment_sql(bad).is_none());
        assert!(allocations_by_deployment_sql(bad, 10).is_none());
        assert!(deployment_signals_sql(bad, 10).is_none());
        assert!(deployment_signal_transactions_sql(bad, 10).is_none());
        assert!(deployment_allocations_history_sql(bad, 10).is_none());
        assert!(deployment_serving_indexers_sql(bad, 10).is_none());
        assert!(indexer_url_sql(bad).is_none());
        assert!(deployment_subgraph_sql(bad).is_none());
        assert!(subgraph_versions_sql(bad, 100).is_none());
        assert!(deployments_signal_stake_sql(&[bad.to_string()]).is_none());
        assert!(deployment_signal_days_sql(&[bad.to_string()], 20_000).is_none());
    }

    /// Signal as of a day is the subgraph's `signalledTokens` cut at that day, so its three terms are
    /// lodestar_deployments' own, and nothing older than the window comes back as its own row.
    #[test]
    fn signal_days_read_the_deployment_views_terms_and_fold_older_changes() {
        let dep = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let q = deployment_signal_days_sql(&[dep.to_string()], 20_704).unwrap();
        let filter = format!("WHERE dep IN ('{dep}')");
        for fragment in [
            "CAST(tokens AS HUGEINT) - CAST(\"curationTax\" AS HUGEINT)",
            "-CAST(tokens AS HUGEINT)",
            "FROM curation__burned",
            "FROM curation__collected",
            "GREATEST(ts // 86400, 20703)",
            filter.as_str(),
        ] {
            assert!(q.contains(fragment), "{fragment} missing from {q}");
        }
        assert!(deployment_signal_days_sql(&[], 20_704).is_none());
        assert!(deployment_signal_days_sql(&vec![dep.to_string(); 201], 20_704).is_none());
    }

    /// The version list and the deployment page must agree about the same deployment, so both read
    /// signal and stake the same way rather than two ways that happen to match today.
    #[test]
    fn versions_and_the_deployment_view_measure_stake_alike() {
        let dep = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let many = deployments_signal_stake_sql(&[dep.to_string()]).unwrap();
        let one = deployment_sql(dep).unwrap();
        for fragment in [
            "MAX(signalled_tokens)",
            "FILTER (WHERE status = 'Active')",
            "lodestar_allocations",
        ] {
            assert!(
                many.contains(fragment),
                "the set query is missing {fragment}"
            );
            assert!(
                one.contains(fragment),
                "the single query is missing {fragment}"
            );
        }
    }

    /// One statement for the whole set, not one per deployment: a subgraph with forty versions
    /// would otherwise be forty round trips to the nest behind one request.
    #[test]
    fn a_versions_signal_lookup_is_one_statement_for_the_set() {
        let ids: Vec<String> = (0..3)
            .map(|n| format!("0x{}{}", "0".repeat(63), n))
            .collect();
        let sql = deployments_signal_stake_sql(&ids).unwrap();
        assert_eq!(sql.matches("SELECT").count(), 1);
        for id in &ids {
            assert!(sql.contains(id), "{id} is not in the statement");
        }
        // And a set nobody asked about is no statement rather than one matching everything.
        assert!(deployments_signal_stake_sql(&[]).is_none());
    }

    #[test]
    fn a_version_limit_outside_its_range_is_refused() {
        // Decimal, because a GNS subgraph id is a `uint256` and not a hash. This test used a hex
        // one, which is the belief that made the route answer 500 in production.
        let id = "89208477503844421768723574082634065786850291557104984756921772861043965025630";
        assert!(subgraph_versions_sql(id, 100).is_some());
        assert!(subgraph_versions_sql(id, 0).is_none());
        assert!(subgraph_versions_sql(id, -1).is_none());
        assert!(subgraph_versions_sql(id, 1001).is_none());
    }

    #[test]
    fn an_absent_filter_is_not_a_failure() {
        assert!(escrow_accounts_sql(None, 10).is_some());
        assert!(tally_collected_sql(None, 10).is_some());
        assert!(tally_by_payer_sql(None).is_some());
        assert!(poi_allocations_sql(None, 10).is_some());
    }

    #[test]
    fn the_collected_total_is_not_capped() {
        let q = tally_by_payer_sql(Some("0xf92f430dd8567b0d466358c79594ab58d919a6d4")).unwrap();
        assert!(
            !q.contains("LIMIT"),
            "a total summed from a capped list is short: {q}"
        );
    }

    /// Without a receiver the balances fold keeps only positive ones; with one, a zero or negative
    /// balance is still the answer to "what does this account hold".
    #[test]
    fn a_receiver_filter_drops_the_positive_only_having() {
        assert!(escrow_accounts_sql(None, 10).unwrap().contains("HAVING"));
        let addr = "0xb43b2cccceada5292732a8c58ae134adefce09bb";
        assert!(
            !escrow_accounts_sql(Some(addr), 10)
                .unwrap()
                .contains("HAVING")
        );
    }

    /// Equal balances and events sharing a block timestamp tie across the LIMIT, so without a unique
    /// key last in the ORDER BY which tied rows make the page changes between runs.
    #[test]
    fn a_page_cut_through_ties_ends_its_order_on_a_unique_key() {
        let addr = "0xb43b2cccceada5292732a8c58ae134adefce09bb";
        for receiver in [None, Some(addr)] {
            let q = escrow_accounts_sql(receiver, 100).unwrap();
            assert!(
                q.ends_with("GROUP BY 1, 2, 3 HAVING SUM(mv.d) > 0 ORDER BY SUM(mv.d) DESC, 1, 2, 3 LIMIT 100")
                    || q.ends_with("GROUP BY 1, 2, 3 ORDER BY SUM(mv.d) DESC, 1, 2, 3 LIMIT 100"),
                "{q}"
            );
        }
        for indexer in [None, Some(addr)] {
            let q = delegation_events_sql(indexer, 100, 0).unwrap();
            assert!(q.ends_with("ORDER BY ts DESC, id DESC LIMIT 100"), "{q}");
        }
    }

    /// Equal collection totals tie the same way, on the tally page and on the per-payer fold.
    #[test]
    fn a_tally_ordered_by_its_total_ends_its_order_on_its_group_key() {
        let addr = "0xb43b2cccceada5292732a8c58ae134adefce09bb";
        for receiver in [None, Some(addr)] {
            let q = tally_collected_sql(receiver, 50).unwrap();
            assert!(
                q.ends_with(
                    "GROUP BY 1, 2, 3 ORDER BY SUM(CAST(tokens AS HUGEINT)) DESC, 1, 2, 3 LIMIT 50"
                ),
                "{q}"
            );
            let q = tally_by_payer_sql(receiver).unwrap();
            assert!(
                q.ends_with("GROUP BY 1 ORDER BY SUM(CAST(tokens AS HUGEINT)) DESC, 1"),
                "{q}"
            );
        }
    }

    #[test]
    fn a_daily_series_is_one_indexer_from_a_day_that_cannot_be_negative() {
        let addr = "0x090f7382f9ea85c733cd501f4d87f16cb5b83ed3";
        let sql = indexer_daily_sql(addr, -5).unwrap();
        assert!(sql.contains(&format!("indexer = '{addr}'")));
        assert!(sql.contains("day_start >= 0 "));
        assert!(sql.contains("ORDER BY day_start DESC"));
    }

    #[test]
    fn revenue_is_read_by_the_day_it_was_paid_and_never_by_allocation_close() {
        let addr = "0xf92f430dd8567b0d466358c79594ab58d919a6d4";
        let daily = indexer_revenue_daily_sql(addr, 1_786_406_400).unwrap();
        let by = indexer_revenue_by_deployment_sql(addr, 1_786_406_400).unwrap();
        assert!(daily.contains("FROM lodestar_indexer_daily "));
        assert!(by.contains("FROM lodestar_indexer_deployment_daily "));
        for sql in [&daily, &by] {
            assert!(sql.contains("day_start >= 1786406400"));
            assert!(!sql.contains("closed_at"), "{sql}");
            assert!(sql.contains("fees_net") && sql.contains("indexer_rewards"));
        }
    }

    #[test]
    fn a_stake_history_series_is_one_statement_not_one_per_point() {
        let addr = "0xb43b2cccceada5292732a8c58ae134adefce09bb";
        let sql = indexer_stake_history_sql(addr, &[100, 200, 300]).unwrap();
        assert_eq!(
            sql.matches("SELECT").count(),
            3,
            "one outer select plus two correlated folds"
        );
        assert!(sql.contains("(100), (200), (300)"));
        assert!(indexer_stake_history_sql(addr, &[]).is_none());
    }

    /// The flow chart must be summed by the nest, never by scanning events here. A raw scan of a
    /// long window exceeds the nest's 50,000-row cap and comes back truncated, which makes the
    /// daily totals short and the chart quietly wrong while looking entirely plausible.
    #[test]
    fn the_flow_chart_is_aggregated_at_the_source() {
        let sql = daily_flow_sql(0, i64::MAX, false);
        assert!(sql.contains("GROUP BY 1"), "must aggregate in the nest");
        assert!(sql.contains("SUM(CASE WHEN direction = 'inflow'"));
        assert!(
            !sql.contains("lodestar_delegations"),
            "the flow reads raw events, not the fold"
        );
    }

    /// The archive holds both eras; the live nest holds only Horizon. Asking the live nest for the
    /// pre-Horizon tables is a 400.
    #[test]
    fn each_era_is_read_from_the_nest_that_has_it() {
        let archive = daily_flow_sql(0, LEGACY_HISTORY_END_EXCLUSIVE, true);
        let live = daily_flow_sql(LEGACY_HISTORY_END_EXCLUSIVE, i64::MAX, false);
        assert!(archive.contains("staking_legacy__stake_delegated"));
        assert!(
            !live.contains("staking_legacy__"),
            "the live nest has no pre-Horizon tables"
        );
        assert!(live.contains("staking__tokens_delegated"));
    }

    /// Disjoint windows, so no day is counted twice and none is dropped.
    #[test]
    fn the_two_windows_meet_exactly_once() {
        let archive = daily_flow_sql(0, LEGACY_HISTORY_END_EXCLUSIVE, true);
        let live = daily_flow_sql(LEGACY_HISTORY_END_EXCLUSIVE, i64::MAX, false);
        assert!(archive.contains(&format!("block_timestamp < {LEGACY_HISTORY_END_EXCLUSIVE}")));
        assert!(live.contains(&format!(
            "block_timestamp >= {LEGACY_HISTORY_END_EXCLUSIVE}"
        )));
    }

    #[test]
    fn the_zero_poi_is_excluded_from_consensus() {
        assert!(poi_allocations_sql(None, 10).unwrap().contains(ZERO_POI));
    }

    use super::*;

    #[test]
    fn only_a_real_address_is_an_address() {
        assert!(is_address("0xb43b2cccceada5292732a8c58ae134adefce09bb"));
        assert!(
            !is_address("0xB43B2CCCCEADA5292732A8C58AE134ADEFCE09BB"),
            "must be lower-cased first"
        );
        assert!(!is_address("0xb43b"));
        assert!(!is_address("'; DROP TABLE x; --"));
    }

    #[test]
    fn an_unvalidated_address_never_reaches_a_statement() {
        assert!(delegation_events_sql(Some("' OR 1=1 --"), 10, 0).is_none());
    }

    #[test]
    fn an_unknown_sort_column_falls_back_rather_than_interpolating() {
        assert_eq!(
            indexers_order_column("stakedTokens; DROP TABLE x"),
            "staked_tokens"
        );
        let sql = indexers_sql(10, 0, "stakedTokens; DROP TABLE x", true);
        assert!(!sql.contains("DROP"), "{sql}");
    }

    #[test]
    fn page_sizes_are_clamped() {
        assert!(indexers_sql(999_999, -5, "stakedTokens", true).contains("LIMIT 1000 OFFSET 0"));
    }

    #[test]
    fn our_own_indexer_stays_out_of_the_public_directory() {
        assert!(indexers_sql(10, 0, "stakedTokens", true).contains(INDEXERS_EXCLUDED));
    }
}

#[cfg(test)]
mod subgraph_id_tests {
    use super::*;

    /// The seventy-seven digit decimal the gns nest actually returned for the deployment that
    /// exposed this, taken from the incumbent's own answer for the same request.
    const REAL: &str =
        "89208477503844421768723574082634065786850291557104984756921772861043965025630";

    #[test]
    fn a_gns_subgraph_id_is_a_decimal_uint256() {
        assert!(is_subgraph_id(REAL));
        assert!(is_subgraph_id("0"));
        assert!(is_subgraph_id("1"));
        assert!(subgraph_versions_sql(REAL, 100).is_some());
    }

    /// The regression. A deployment id is hex and is not one of these, and treating it as one is
    /// what made the route answer 500 for every published subgraph.
    #[test]
    fn a_deployment_id_is_not_a_subgraph_id() {
        let deployment = "0x".to_string() + &"ab".repeat(32);
        assert!(is_deployment_id(&deployment));
        assert!(!is_subgraph_id(&deployment));
    }

    /// The id is interpolated into the statement, so the check is the guard.
    #[test]
    fn nothing_that_is_not_a_number_gets_through() {
        for bad in [
            "",
            "12'; DROP TABLE subgraph_versions --",
            "-1",
            "1.5",
            "0123",
            " 12",
            "12 ",
            "1e30",
            // One digit wider than a uint256, and the same width but larger.
            "1157920892373161954235709850086879078532699846656405640394575840079131296399350",
            "115792089237316195423570985008687907853269984665640564039457584007913129639936",
        ] {
            assert!(!is_subgraph_id(bad), "{bad:?} should not be a subgraph id");
            assert!(subgraph_versions_sql(bad, 100).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn the_ceiling_itself_is_allowed() {
        let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert!(is_subgraph_id(max));
    }
}
