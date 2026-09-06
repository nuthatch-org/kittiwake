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

#[cfg(test)]
mod tests {
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
