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
pub const INDEXERS_EXCLUDED: &str = "0xb43b2cccceada5292732a8c58ae134adefce09bb";

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
         FROM ({union}) t WHERE {where_clause} ORDER BY ts DESC LIMIT {first}"
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
    let active = if active_only { " AND s.active" } else { "" };
    Some(format!(
        "SELECT s.id, s.indexer, CAST(s.total_delegated_tokens AS VARCHAR) AS staked_tokens, \
         CAST(s.staked_tokens AS VARCHAR) AS current_tokens, CAST(s.share_amount AS VARCHAR) AS share_amount, \
         CAST(s.locked_tokens AS VARCHAR) AS locked_tokens, s.locked_until, \
         CAST(s.realized_rewards AS VARCHAR) AS realized_rewards, \
         CAST(s.total_undelegated_tokens AS VARCHAR) AS unstaked_tokens, s.created_at, s.last_undelegated_at, s.active, \
         CAST(i.staked_tokens AS VARCHAR) AS indexer_staked_tokens, \
         CAST(i.delegated_tokens AS VARCHAR) AS indexer_delegated_tokens, \
         CAST(i.delegated_thawing_tokens AS VARCHAR) AS indexer_delegated_thawing_tokens, \
         CAST(i.delegator_shares AS VARCHAR) AS indexer_delegator_shares, \
         i.indexing_reward_cut, i.query_fee_cut, i.allocation_count, i.url, i.geohash \
         FROM lodestar_delegator_stakes s LEFT JOIN lodestar_indexers i ON i.id = s.indexer \
         WHERE s.delegator = '{addr}'{active} ORDER BY s.staked_tokens DESC, s.indexer LIMIT {first}"
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
         CAST(provisioned_tokens AS VARCHAR) AS provisioned_tokens, url, geohash, created_at \
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
    Some(format!(
        "SELECT id, delegator, CAST(s.total_delegated_tokens AS VARCHAR) AS staked_tokens, \
         CAST(share_amount AS VARCHAR) AS share_amount FROM lodestar_delegator_stakes s \
         WHERE s.indexer = '{addr}' AND s.active ORDER BY s.total_delegated_tokens DESC, s.delegator ASC LIMIT {first}"
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

pub const DELEGATION_RATIO_SQL: &str = "SELECT delegation_ratio FROM lodestar_network_params";

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
         GROUP BY 1, 2, 3 {having}ORDER BY SUM(mv.d) DESC LIMIT {first}"
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
         GROUP BY 1, 2, 3 ORDER BY SUM(CAST(tokens AS HUGEINT)) DESC LIMIT {first}"
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

/// The pre-Horizon half of the delegation flow, from the frozen archive.
///
/// The archive is a raw event store, not a nest with the `lodestar_*` views: it carries
/// `staking_legacy__stake_delegated` and `..._locked` and nothing folded on top. Asking it for
/// `lodestar_delegations`, as the first version of this did, is a 400 from the nest and a broken
/// chart. The output shape matches [`delegation_events_since_sql`] so the caller cannot tell the
/// two halves apart.
///
/// `StakeDelegatedLocked` is the pre-Horizon undelegation: tokens leave the pool and are locked
/// until a thawing period ends.
///
/// The archive holds 346,288 delegations spanning 2023-02-13 to 2025-12-09, so a recent window
/// legitimately returns nothing from it. That is an empty result, not a failure.
pub fn legacy_delegation_events_since_sql(since: i64) -> String {
    format!(
        "SELECT 'delegation' AS event_type, LOWER(indexer) AS indexer, CAST(tokens AS VARCHAR) AS tokens          FROM staking_legacy__stake_delegated WHERE block_timestamp > {since}          UNION ALL          SELECT 'undelegation', LOWER(indexer), CAST(tokens AS VARCHAR)          FROM staking_legacy__stake_delegated_locked WHERE block_timestamp > {since}"
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
         ORDER BY created_at DESC LIMIT 1"
    ))
}

pub const SUBGRAPH_METADATA_SQL: &str = "SELECT subgraph_metadata FROM subgraph_current WHERE NOT deprecated AND subgraph_metadata IS NOT NULL";

// ---------------------------------------------------------------------------------------------
// DIPS (the `/dips` nests)
// ---------------------------------------------------------------------------------------------

pub const DIPS_CURRENT_ALLOCATION_SQL: &str = "SELECT * FROM dips_current_allocation";
pub const DIPS_TIMELINE_SQL: &str = "SELECT * FROM dips_timeline ORDER BY block_number";

#[cfg(test)]
mod tests {
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
        assert!(indexer_active_allocations_sql(bad).is_none());
        assert!(indexer_closed_allocations_sql(bad, 10).is_none());
        assert!(indexer_stake_history_sql(bad, &[1, 2]).is_none());
        assert!(provisions_by_indexer_sql(bad).is_none());
        assert!(provisions_by_service_sql(bad, 10, 0).is_none());
        assert!(data_service_totals_sql(&[bad.to_string()]).is_none());
        assert!(poi_allocations_sql(Some(bad), 10).is_none());
        assert!(escrow_accounts_sql(Some(bad), 10).is_none());
        assert!(escrow_transactions_sql(Some(bad), 10).is_none());
        assert!(tally_collected_sql(Some(bad), 10).is_none());
        assert!(deployments_by_id_sql(&[bad.to_string()]).is_none());
        assert!(deployment_sql(bad).is_none());
        assert!(allocations_by_deployment_sql(bad, 10).is_none());
        assert!(deployment_signals_sql(bad, 10).is_none());
        assert!(deployment_signal_transactions_sql(bad, 10).is_none());
        assert!(deployment_allocations_history_sql(bad, 10).is_none());
        assert!(deployment_serving_indexers_sql(bad, 10).is_none());
        assert!(indexer_url_sql(bad).is_none());
        assert!(deployment_subgraph_sql(bad).is_none());
    }

    #[test]
    fn an_absent_filter_is_not_a_failure() {
        assert!(escrow_accounts_sql(None, 10).is_some());
        assert!(tally_collected_sql(None, 10).is_some());
        assert!(poi_allocations_sql(None, 10).is_some());
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

    /// The two halves of the delegation flow come from different schemas and must not be asked
    /// the same question.
    #[test]
    fn the_archive_is_queried_by_its_own_tables_not_the_live_views() {
        let live = delegation_events_since_sql(100);
        let archive = legacy_delegation_events_since_sql(100);
        assert!(live.contains("lodestar_delegations"));
        assert!(
            !archive.contains("lodestar_delegations"),
            "the archive has no such view"
        );
        assert!(archive.contains("staking_legacy__stake_delegated"));
        assert!(archive.contains("staking_legacy__stake_delegated_locked"));
        // Same columns out, so the caller cannot tell which half it is reading.
        for col in ["event_type", "indexer", "tokens"] {
            assert!(
                archive.contains(col),
                "{col} missing from the archive query"
            );
        }
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
