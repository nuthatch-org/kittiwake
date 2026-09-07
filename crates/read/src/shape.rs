//! Mapping the nest's columns to the contract the dashboard is written against.
//!
//! The nest names things after the events it indexes: `total_tokens_staked`, snake_case, flat. The
//! frontend's hooks were written against a different vocabulary, camelCase and nested. Neither is
//! more correct; what matters is that this service returns the one its callers already parse, and
//! that the translation lives in one place rather than being improvised per route.
//!
//! Two rules run through all of it. **Wei stays a decimal string** - it does not survive an `f64`,
//! and a field that quietly became a number would break every consumer that parses it. And
//! **absent is null, not zero**: a thawing period that does not exist under Horizon is `null`,
//! because zero would read as "no waiting period" rather than "the concept does not apply".

use serde_json::{Value, json};

/// A column as the decimal string the frontend expects.
pub fn s(v: &Value, key: &str) -> Value {
    match v.get(key) {
        Some(Value::String(x)) => Value::String(x.clone()),
        Some(Value::Number(n)) => Value::String(n.to_string()),
        _ => Value::Null,
    }
}

/// A column as a string, defaulting to "0" where the frontend expects a figure rather than a gap.
pub fn s0(v: &Value, key: &str) -> Value {
    match s(v, key) {
        Value::Null => Value::String("0".into()),
        other => other,
    }
}

/// A column as an integer, accepting the string form the nest sometimes returns.
pub fn i(v: &Value, key: &str) -> Value {
    match v.get(key) {
        Some(Value::Number(n)) => Value::Number(n.clone()),
        Some(Value::String(x)) => x.parse::<i64>().map(|n| json!(n)).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// As [`i`], but zero rather than null where a count is expected.
pub fn i0(v: &Value, key: &str) -> Value {
    match i(v, key) {
        Value::Null => json!(0),
        other => other,
    }
}

/// A plain string column, or null.
pub fn t(v: &Value, key: &str) -> Value {
    match v.get(key) {
        Some(Value::String(x)) => Value::String(x.clone()),
        _ => Value::Null,
    }
}

/// Add two wei figures without going through a float.
///
/// These are 27-digit numbers. `f64` carries about 15 significant digits, so adding them as floats
/// would be wrong by more than a whole GRT and look entirely plausible.
pub fn add_wei(a: &Value, key_a: &str, key_b: &str) -> Value {
    let parse = |k: &str| -> u128 {
        match a.get(k) {
            Some(Value::String(x)) => x.parse().unwrap_or(0),
            Some(Value::Number(n)) => n.as_u64().map(u128::from).unwrap_or(0),
            _ => 0,
        }
    };
    Value::String((parse(key_a) + parse(key_b)).to_string())
}

// ---------------------------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------------------------

/// One epoch. `totalQueryFees` is the two fee columns summed, as the incumbent reports it.
pub fn epoch(r: &Value) -> Value {
    json!({
        "id": s(r, "id"),
        "startBlock": i(r, "start_block"),
        "endBlock": i(r, "end_block"),
        "signalledTokens": s0(r, "signalled_tokens"),
        "stakeDeposited": s0(r, "stake_deposited"),
        "totalRewards": s0(r, "total_rewards"),
        "totalIndexerRewards": s0(r, "total_indexer_rewards"),
        "totalDelegatorRewards": s0(r, "total_delegator_rewards"),
        "totalQueryFees": add_wei(r, "query_fees_collected", "curator_query_fees"),
    })
}

/// One indexer in the directory.
///
/// `account` is a nested object the frontend reads for the display name. The nest has no ENS or
/// metadata, so both are null on this path: the refresh job fills them in Postgres, and the
/// directory route reads the nest directly.
pub fn indexer(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "stakedTokens": s0(r, "staked_tokens"),
        "lockedTokens": s0(r, "locked_tokens"),
        "delegatedTokens": s0(r, "delegated_tokens"),
        "allocatedTokens": s0(r, "allocated_tokens"),
        "allocationCount": i0(r, "allocation_count"),
        "indexingRewardCut": i0(r, "indexing_reward_cut"),
        "queryFeeCut": i0(r, "query_fee_cut"),
        "lastDelegationParameterUpdate": i0(r, "last_delegation_parameter_update"),
        // The nest does not carry the cooldown; zero means "none set", which is what it means.
        "delegatorParameterCooldown": json!(0),
        "rewardsEarned": s0(r, "rewards_earned"),
        "delegatorShares": s0(r, "delegator_shares"),
        "url": t(r, "url"),
        "geoHash": t(r, "geohash"),
        "createdAt": i0(r, "created_at"),
        "account": json!({
            "id": t(r, "id"),
            "defaultDisplayName": Value::Null,
            "metadata": Value::Null,
        }),
    })
}

pub fn curator(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "totalSignalledTokens": s0(r, "total_signalled_tokens"),
        "totalUnsignalledTokens": s0(r, "total_unsignalled_tokens"),
        "realizedRewards": s0(r, "realized_rewards"),
        "signalCount": i0(r, "signal_count"),
        "activeSignalCount": i0(r, "active_signal_count"),
    })
}

/// A delegation event. The SQL already aliases `eventType` and `txHash`, so those pass through.
pub fn delegation_event(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "eventType": t(r, "eventType"),
        "indexer": t(r, "indexer"),
        "delegator": t(r, "delegator"),
        "tokens": s0(r, "tokens"),
        "timestamp": s(r, "timestamp"),
        "txHash": t(r, "txHash"),
    })
}

/// One provision, with the data service it is against nested inside it.
///
/// The service-wide totals are not in this row - they are a fold over every provision to that
/// service - so they are null here rather than a per-indexer figure wearing a network-wide name.
pub fn provision_with_totals(r: &Value, totals: Option<&Value>) -> Value {
    let mut out = provision(r);
    if let Some(tot) = totals {
        out["dataService"]["totalTokensProvisioned"] = s0(tot, "total_tokens_provisioned");
        out["dataService"]["totalTokensAllocated"] = s0(tot, "total_tokens_allocated");
    }
    out
}

pub fn provision(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "tokensProvisioned": s0(r, "tokens_provisioned"),
        "tokensAllocated": s0(r, "tokens_allocated"),
        "tokensThawing": s0(r, "tokens_thawing"),
        "maxVerifierCut": s0(r, "max_verifier_cut"),
        "thawingPeriod": s0(r, "thawing_period"),
        "createdAt": s0(r, "created_at"),
        "allocationCount": i0(r, "allocation_count"),
        "rewardsEarned": s0(r, "rewards_earned"),
        "queryFeesCollected": s0(r, "query_fees_collected"),
        "dataService": json!({
            "id": t(r, "data_service"),
            "totalTokensProvisioned": Value::Null,
            "totalTokensAllocated": Value::Null,
            "minimumThawingPeriod": Value::Null,
            "maximumThawingPeriod": Value::Null,
        }),
    })
}

/// One subgraph deployment in the directory.
///
/// `ipfsHash` is the `Qm…` form the reader recognises; the nest stores the bytes32 id. Converting
/// between them needs base58 and is done by the metadata pipeline, so the hash is the id here until
/// that lands, and `displayName` is null rather than invented.
pub fn deployment(r: &Value, fees_30d: Option<&Value>) -> Value {
    let id = t(r, "id");
    let id_str = id.as_str().unwrap_or_default().to_string();
    let mut out = json!({
        "id": id,
        "ipfsHash": t(r, "id"),
        "signalledTokens": s0(r, "signalled_tokens"),
        "stakedTokens": s0(r, "staked_tokens"),
        "queryFeesAmount": s0(r, "query_fees_amount"),
        "createdAt": i0(r, "created_at"),
        "displayName": Value::Null,
        "categories": json!([]),
        "curatorSignals": placeholders(&id_str, i0(r, "curator_count").as_i64().unwrap_or(0)),
        "indexerAllocations": placeholders(
            &id_str,
            i0(r, "active_allocation_count").as_i64().unwrap_or(0),
        ),
    });
    if let Some(f) = fees_30d {
        out["queryFees30d"] = f.clone();
    }
    out
}

/// `n` objects carrying only an id of the form `<deployment>-<i>`.
///
/// This looks like a placeholder because it is one. The incumbent sends these arrays and the
/// frontend reads only their `length` - they carry a count, not data. Reproducing the exact shape
/// costs one loop; fetching the real signals and allocations per deployment would cost a query per
/// row of the directory and tell the reader nothing more.
fn placeholders(deployment_id: &str, n: i64) -> Value {
    Value::Array(
        (0..n.clamp(0, 10_000))
            .map(|i| json!({ "id": format!("{deployment_id}-{i}") }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wei_survives_the_translation_as_a_string() {
        let r = json!({"staked_tokens": "707387524504002513347899608"});
        assert_eq!(
            indexer(&r)["stakedTokens"],
            json!("707387524504002513347899608"),
            "wei must never become a number"
        );
    }

    /// 27-digit figures do not survive an f64, and the error is larger than a whole GRT.
    #[test]
    fn two_wei_figures_are_added_exactly() {
        let r = json!({
            "query_fees_collected": "23679683366672773102064606",
            "curator_query_fees": "1183984168333638655103230"
        });
        assert_eq!(
            add_wei(&r, "query_fees_collected", "curator_query_fees"),
            json!("24863667535006411757167836")
        );
    }

    #[test]
    fn a_missing_figure_is_zero_and_a_missing_name_is_null() {
        let empty = json!({});
        assert_eq!(curator(&empty)["totalSignalledTokens"], json!("0"));
        assert_eq!(curator(&empty)["signalCount"], json!(0));
        assert_eq!(indexer(&empty)["url"], Value::Null);
        assert_eq!(
            indexer(&empty)["account"]["defaultDisplayName"],
            Value::Null
        );
    }

    #[test]
    fn an_epoch_reports_both_fee_columns_as_one_total() {
        let r = json!({
            "id": 1374, "start_block": 100, "end_block": 200,
            "query_fees_collected": "10", "curator_query_fees": "5"
        });
        let e = epoch(&r);
        assert_eq!(
            e["id"],
            json!("1374"),
            "the incumbent sends the id as a string"
        );
        assert_eq!(e["startBlock"], json!(100));
        assert_eq!(e["totalQueryFees"], json!("15"));
    }

    #[test]
    fn a_number_arriving_as_a_string_is_still_a_number_where_one_is_expected() {
        let r = json!({"allocation_count": "7", "created_at": "1700000000"});
        assert_eq!(indexer(&r)["allocationCount"], json!(7));
        assert_eq!(indexer(&r)["createdAt"], json!(1_700_000_000));
    }

    /// The arrays carry a count. Their length is the whole of what the frontend reads.
    #[test]
    fn the_nested_arrays_carry_the_counts_the_frontend_reads() {
        let r = json!({"id": "0xabc", "curator_count": 2, "active_allocation_count": 3});
        let d = deployment(&r, None);
        assert_eq!(d["curatorSignals"].as_array().unwrap().len(), 2);
        assert_eq!(d["indexerAllocations"].as_array().unwrap().len(), 3);
        assert_eq!(d["curatorSignals"][0]["id"], json!("0xabc-0"));
    }

    #[test]
    fn a_deployment_gains_its_thirty_day_fees_only_when_asked() {
        let r = json!({"id": "0xabc", "signalled_tokens": "1"});
        assert!(deployment(&r, None).get("queryFees30d").is_none());
        assert_eq!(
            deployment(&r, Some(&json!("42")))["queryFees30d"],
            json!("42")
        );
    }
}
