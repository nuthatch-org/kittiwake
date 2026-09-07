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
pub fn provision_with_totals(r: &Value, totals: Option<&Value>, thawing: Option<&str>) -> Value {
    let mut out = provision(r);
    if let Some(tot) = totals {
        out["dataService"]["totalTokensProvisioned"] = s0(tot, "total_tokens_provisioned");
        out["dataService"]["totalTokensAllocated"] = s0(tot, "total_tokens_allocated");
    }
    if let Some(t) = thawing {
        out["dataService"]["minimumThawingPeriod"] = Value::String(t.to_string());
        out["dataService"]["maximumThawingPeriod"] = Value::String(t.to_string());
    }
    out
}

/// Decode a sampled `eth_call` return - a 32-byte big-endian word, hex encoded - as a decimal
/// string. Returns `None` rather than zero on anything unexpected: a thawing period of zero would
/// read as "no waiting period", which is a different and dangerous claim.
pub fn decode_uint_word(hex: &str) -> Option<String> {
    let h = hex.strip_prefix("0x").unwrap_or(hex);
    if h.is_empty() || h.len() > 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(h, 16).ok().map(|v| v.to_string())
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
        "ipfsHash": ipfs_or_id(&id_str),
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

// ---------------------------------------------------------------------------------------------
// Derived shapes: figures the nest does not hold, computed from ones it does
// ---------------------------------------------------------------------------------------------

/// Wei as whole GRT. For display figures only, never for a balance a reader might act on.
fn grt(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::String(x)) => x.parse::<f64>().unwrap_or(0.0) / 1e18,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) / 1e18,
        _ => 0.0,
    }
}

/// One epoch as an issuance-and-burn point.
///
/// `disputeBurn` is zero from this source: the burn is only known once a dispute is accepted and
/// `StakeSlashed` is indexed, and none ever has been. Zero here means "none happened", which is
/// true, rather than "we did not look".
pub fn token_metric(r: &Value) -> Value {
    let issuance = grt(r, "total_rewards");
    let query_fee_tax_burn = grt(r, "taxed_query_fees");
    json!({
        "epoch": i0(r, "id"),
        "issuance": issuance,
        "queryFeeTaxBurn": query_fee_tax_burn,
        "disputeBurn": 0,
        "totalBurn": query_fee_tax_burn,
        "net": issuance - query_fee_tax_burn,
    })
}

/// The Monday of the week containing `unix`, as `YYYY-MM-DD` in UTC.
///
/// Weeks start on Monday because that is what the chart's axis says. Sunday-start weeks would
/// shift every point by a day and look almost right.
pub fn week_start_utc(unix: i64) -> String {
    use chrono::{DateTime, Datelike, Duration, Utc};
    let d: DateTime<Utc> = DateTime::from_timestamp(unix, 0).unwrap_or_default();
    let back = d.weekday().num_days_from_monday() as i64;
    (d - Duration::days(back)).format("%Y-%m-%d").to_string()
}

/// Weekly publication counts, with a running cumulative and the current week flagged partial.
///
/// Every week between the first and last is emitted, including the empty ones. A chart that skips
/// quiet weeks compresses time and makes a lull look like continuity.
pub fn developer_activity(rows: &[Value], window_months: i64, now: i64) -> Value {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for r in rows {
        let Some(ts) = r.get("createdAt").and_then(|v| v.as_i64()) else {
            continue;
        };
        *counts.entry(week_start_utc(ts)).or_insert(0) += 1;
    }

    let current = week_start_utc(now);
    let mut weeks = Vec::new();
    let mut cumulative = 0i64;
    let mut total = 0i64;

    if let (Some(first), Some(last)) = (
        counts.keys().next().cloned(),
        counts.keys().next_back().cloned(),
    ) {
        let parse = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        if let (Some(mut d), Some(end)) = (parse(&first), parse(&last)) {
            while d <= end {
                let key = d.format("%Y-%m-%d").to_string();
                let count = counts.get(&key).copied().unwrap_or(0);
                cumulative += count;
                total += count;
                weeks.push(json!({
                    "weekStart": key.clone(),
                    "count": count,
                    "cumulative": cumulative,
                    "partial": key == current,
                }));
                d += chrono::Duration::days(7);
            }
        }
    }

    // Week over week compares the last two complete weeks: the current one is still filling and
    // would always read as a collapse.
    let complete: Vec<i64> = weeks
        .iter()
        .filter(|w| w["partial"] != json!(true))
        .filter_map(|w| w["count"].as_i64())
        .collect();
    let wow = match complete.len() {
        0 | 1 => Value::Null,
        n => {
            let (last, prev) = (complete[n - 1], complete[n - 2]);
            if prev == 0 {
                Value::Null
            } else {
                json!(((last - prev) as f64 / prev as f64) * 100.0)
            }
        }
    };

    json!({
        "weeks": weeks,
        "windowMonths": window_months,
        "totalInWindow": total,
        "lastWeekCount": complete.last().copied().unwrap_or(0),
        "weekOverWeekPct": wow,
        "source": "nuthatch",
    })
}

/// Sum a wei column across rows, exactly.
fn sum_wei(rows: &[Value], key: &str) -> String {
    let mut total: i128 = 0;
    for r in rows {
        let v = match r.get(key) {
            Some(Value::String(x)) => x.parse::<i128>().unwrap_or(0),
            Some(Value::Number(n)) => n.as_i64().map(i128::from).unwrap_or(0),
            _ => 0,
        };
        total += v;
    }
    total.to_string()
}

fn distinct(rows: &[Value], key: &str) -> usize {
    let mut seen: std::collections::BTreeSet<&str> = Default::default();
    for r in rows {
        if let Some(v) = r.get(key).and_then(|x| x.as_str()) {
            seen.insert(v);
        }
    }
    seen.len()
}

/// The payments panel: three lists and the totals over them.
pub fn payments(accounts: &[Value], transactions: &[Value], tally: &[Value]) -> Value {
    let escrow: Vec<Value> = accounts
        .iter()
        .map(|r| {
            json!({
                "id": format!(
                    "{}-{}-{}",
                    r.get("payer").and_then(|v| v.as_str()).unwrap_or_default(),
                    r.get("collector").and_then(|v| v.as_str()).unwrap_or_default(),
                    r.get("receiver").and_then(|v| v.as_str()).unwrap_or_default()
                ),
                "balance": s0(r, "balance"),
                "totalAmountThawing": s0(r, "thawing"),
                "thawEndTimestamp": s0(r, "thaw_end"),
                "payer": { "id": t(r, "payer") },
                "receiver": { "id": t(r, "receiver") },
            })
        })
        .collect();

    let recent: Vec<Value> = transactions
        .iter()
        .map(|r| {
            json!({
                "id": format!(
                    "{}-{}",
                    r.get("tx_hash").and_then(|v| v.as_str()).unwrap_or_default(),
                    r.get("log_index").and_then(|v| v.as_i64()).unwrap_or(0)
                ),
                "type": t(r, "type"),
                "amount": s0(r, "amount"),
                "timestamp": s0(r, "block_timestamp"),
                "allocationId": t(r, "allocation_id"),
                "payer": { "id": t(r, "payer") },
                "receiver": { "id": t(r, "receiver") },
            })
        })
        .collect();

    let collectors: Vec<Value> = tally
        .iter()
        .map(|r| {
            json!({
                "id": format!(
                    "{}-{}",
                    r.get("collection_id").and_then(|v| v.as_str()).unwrap_or_default(),
                    r.get("receiver").and_then(|v| v.as_str()).unwrap_or_default()
                ),
                "collectionId": t(r, "collection_id"),
                "tokens": s0(r, "tokens"),
                "payer": { "id": t(r, "payer") },
                "receiver": { "id": t(r, "receiver") },
            })
        })
        .collect();

    json!({
        "escrowAccounts": escrow,
        "recentTransactions": recent,
        "topCollectors": collectors,
        "totalEscrowBalance": sum_wei(accounts, "balance"),
        "totalThawing": sum_wei(accounts, "thawing"),
        "totalCollected": sum_wei(tally, "tokens"),
        "activePayers": distinct(accounts, "payer"),
        "activeReceivers": distinct(accounts, "receiver"),
    })
}

// ---------------------------------------------------------------------------------------------
// One indexer, in full
// ---------------------------------------------------------------------------------------------

/// The ratios the profile page shows, derived exactly as the incumbent derives them.
///
/// Two subtleties worth keeping. **Locked tokens do not count as usable own stake**: they are
/// spoken for, so an indexer with most of its stake locked is not as well capitalised as its
/// headline figure suggests. And **delegation above capacity is not usable either**: the ratios
/// are taken against `min(maxUsable, usableOwn + delegated)`, so over-delegating cannot flatter
/// an indexer's own-stake ratio.
pub fn derived_indexer_metrics(r: &Value, delegation_ratio: f64) -> Value {
    let g = |k: &str| grt(r, k);
    let staked = g("staked_tokens");
    let locked = g("locked_tokens");
    let delegated = g("delegated_tokens");

    let usable_own = staked - locked;
    let max_usable = usable_own + usable_own * delegation_ratio;
    let total_usable = max_usable.min(usable_own + delegated);

    let own_stake_ratio = if total_usable == 0.0 {
        0.0
    } else {
        usable_own / total_usable
    };
    let delegated_stake_ratio = if own_stake_ratio == 0.0 {
        0.0
    } else {
        1.0 - own_stake_ratio
    };

    let cut = match r.get("indexing_reward_cut") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    };
    let delegator_cut = (1_000_000.0 - cut) / 1_000_000.0;
    let effective_cut = if delegated_stake_ratio == 0.0 {
        0.0
    } else {
        1.0 - delegator_cut / delegated_stake_ratio
    };

    let max_delegated = staked * delegation_ratio;
    let dilution_denom = max_delegated.max(delegated);
    let dilution = if dilution_denom == 0.0 {
        0.0
    } else {
        1.0 - max_delegated / dilution_denom
    };
    let own_generation = if own_stake_ratio == 0.0 {
        0.0
    } else {
        cut / 1_000_000.0 / own_stake_ratio
    };

    // Capacity in wei, exactly: own stake plus delegation up to the ratio, and no further.
    let wei = |k: &str| -> u128 {
        match r.get(k) {
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            Some(Value::Number(n)) => n.as_u64().map(u128::from).unwrap_or(0),
            _ => 0,
        }
    };
    let staked_wei = wei("staked_tokens");
    let delegated_wei = wei("delegated_tokens");
    let max_delegated_wei = staked_wei.saturating_mul(delegation_ratio as u128);
    let capacity = staked_wei + delegated_wei.min(max_delegated_wei);

    json!({
        "tokenCapacity": capacity.to_string(),
        "ownStakeRatio": own_stake_ratio.to_string(),
        "delegatedStakeRatio": delegated_stake_ratio.to_string(),
        "indexingRewardEffectiveCut": effective_cut.to_string(),
        "overDelegationDilution": dilution.to_string(),
        "indexerRewardsOwnGenerationRatio": own_generation.to_string(),
    })
}

fn deployment_stub(r: &Value, with_stats: bool) -> Value {
    let id = t(r, "subgraph_deployment");
    let mut out = json!({
        "id": id.clone(),
        "ipfsHash": ipfs_or_id(id.as_str().unwrap_or_default()),
        "versions": json!([]),
    });
    if with_stats {
        out["signalledTokens"] = s0(r, "signalled_tokens");
        out["stakedTokens"] = s0(r, "deployment_staked_tokens");
    }
    out
}

pub fn active_allocation(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "allocatedTokens": s0(r, "allocated_tokens"),
        "createdAtEpoch": i0(r, "created_at_epoch"),
        "subgraphDeployment": deployment_stub(r, true),
    })
}

pub fn closed_allocation(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "allocatedTokens": s0(r, "allocated_tokens"),
        "createdAtEpoch": i0(r, "created_at_epoch"),
        "closedAtEpoch": i0(r, "closed_at_epoch"),
        "closedAt": i0(r, "closed_at"),
        "indexingRewards": s0(r, "indexing_rewards"),
        "queryFeesCollected": s0(r, "query_fees_collected"),
        "poi": t(r, "poi"),
        "forceClosed": r.get("force_closed").cloned().unwrap_or(json!(false)),
        "subgraphDeployment": deployment_stub(r, false),
    })
}

/// The whole indexer profile.
#[allow(clippy::too_many_arguments)]
pub fn indexer_detail(
    row: &Value,
    operators: &[Value],
    active: &[Value],
    closed: &[Value],
    delegation_ratio: f64,
) -> Value {
    let mut out = json!({
        "id": t(row, "id"),
        "account": json!({
            "id": t(row, "id"),
            "defaultDisplayName": Value::Null,
            "metadata": Value::Null,
            "operators": operators
                .iter()
                .map(|o| json!({ "id": t(o, "operator") }))
                .collect::<Vec<_>>(),
        }),
        "stakedTokens": s0(row, "staked_tokens"),
        "lockedTokens": s0(row, "locked_tokens"),
        "delegatedTokens": s0(row, "delegated_tokens"),
        "delegatedThawingTokens": s0(row, "delegated_thawing_tokens"),
        "allocatedTokens": s0(row, "allocated_tokens"),
        "provisionedTokens": s0(row, "provisioned_tokens"),
        "allocationCount": i0(row, "allocation_count"),
        "indexingRewardCut": i0(row, "indexing_reward_cut"),
        "queryFeeCut": i0(row, "query_fee_cut"),
        "delegatorParameterCooldown": json!(0),
        "lastDelegationParameterUpdate": i0(row, "last_delegation_parameter_update"),
        "rewardsEarned": s0(row, "rewards_earned"),
        "queryFeesCollected": s0(row, "query_fees_collected"),
        "delegatorShares": s0(row, "delegator_shares"),
        "url": t(row, "url"),
        "geoHash": t(row, "geohash"),
        "createdAt": i0(row, "created_at"),
        "allocations": active.iter().map(active_allocation).collect::<Vec<_>>(),
        "closedAllocations": closed.iter().map(closed_allocation).collect::<Vec<_>>(),
    });

    if let Value::Object(derived) = derived_indexer_metrics(row, delegation_ratio) {
        for (k, v) in derived {
            out[k] = v;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Proof-of-indexing consensus
// ---------------------------------------------------------------------------------------------

pub const ZERO_POI: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// The consensus view across closed allocations.
///
/// **Consensus is weighted by stake, not by headcount.** Ten small indexers agreeing does not
/// outvote one large one; what the network pays out on is stake, so that is what the percentage
/// measures. A deployment where the majority stake agrees is at consensus even if most indexers
/// disagree, and that is the honest reading of the incentive.
///
/// Allocations closed without a proof carry the all-zero POI. They are excluded rather than
/// counted as disagreement: an indexer that closed without proving anything has not diverged, it
/// has abstained.
///
/// Only the **latest epoch** decides a deployment's verdict. An old divergence that has since
/// resolved is history, and reporting it as current would keep a fixed deployment flagged for ever.
pub fn poi_overview(rows: &[Value]) -> Value {
    use std::collections::BTreeMap;

    /// The nest returns some integer columns as JSON strings and others as numbers, in the same
    /// row. A bare `as_i64()` reads `"1374"` as absent, which collapsed every deployment into one
    /// empty epoch group and reported perfect consensus across the whole network.
    fn num(v: &Value, key: &str) -> Option<i64> {
        match v.get(key)? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    let real: Vec<&Value> = rows
        .iter()
        .filter(|r| {
            r.get("poi")
                .and_then(|p| p.as_str())
                .is_some_and(|p| p != ZERO_POI)
        })
        .collect();

    let mut by_deployment: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for a in &real {
        let key = a
            .get("subgraph_deployment")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        by_deployment.entry(key.to_string()).or_default().push(a);
    }

    let mut deployments = Vec::new();
    let mut total_divergent = 0i64;
    let mut consensus_weighted = 0.0f64;
    let mut stake_weighted = 0.0f64;

    for (deployment_id, allocs) in &by_deployment {
        let latest_epoch = allocs
            .iter()
            .filter_map(|a| num(a, "closed_at_epoch"))
            .max()
            .unwrap_or(0);
        let latest: Vec<&&Value> = allocs
            .iter()
            .filter(|a| num(a, "closed_at_epoch") == Some(latest_epoch))
            .collect();

        // Stake behind each distinct proof in the latest epoch.
        let mut stakes: BTreeMap<String, f64> = BTreeMap::new();
        for a in &latest {
            let poi = a
                .get("poi")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            *stakes.entry(poi).or_insert(0.0) += grt(a, "allocated_tokens");
        }
        let consensus_poi = stakes
            .iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(p, _)| p.clone());
        let total_real: f64 = stakes.values().sum();
        let consensus_stake = consensus_poi
            .as_ref()
            .and_then(|p| stakes.get(p))
            .copied()
            .unwrap_or(0.0);
        let divergent = latest
            .iter()
            .filter(|a| a.get("poi").and_then(|v| v.as_str()).map(str::to_string) != consensus_poi)
            .count() as i64;

        let has_divergence = divergent > 0;
        if has_divergence {
            total_divergent += 1;
        }
        consensus_weighted += consensus_stake;
        stake_weighted += total_real;

        let unique: std::collections::BTreeSet<&str> = allocs
            .iter()
            .filter_map(|a| a.get("indexer").and_then(|v| v.as_str()))
            .collect();
        let latest_closed_at = allocs
            .iter()
            .filter_map(|a| num(a, "closed_at"))
            .max()
            .unwrap_or(0);
        let first = allocs[0];

        deployments.push(json!({
            "deploymentId": deployment_id,
            "ipfsHash": ipfs_or_id(deployment_id),
            "signal": grt(first, "signalled_tokens"),
            "stake": grt(first, "staked_tokens"),
            "latestEpoch": latest_epoch,
            "latestClosedAt": latest_closed_at,
            "allocationCount": allocs.len(),
            "uniqueIndexers": unique.len(),
            "consensusPct": if total_real > 0.0 { (consensus_stake / total_real) * 100.0 } else { 0.0 },
            "hasDivergence": has_divergence,
            "divergentCount": divergent,
            "hasRealPois": true,
        }));
    }

    // Divergent deployments first, then by how much is at stake behind them.
    deployments.sort_by(|a, b| {
        let (da, db) = (a["hasDivergence"].as_bool(), b["hasDivergence"].as_bool());
        db.cmp(&da).then(
            b["allocationCount"]
                .as_i64()
                .cmp(&a["allocationCount"].as_i64()),
        )
    });

    json!({
        "summary": {
            "totalAllocations": real.len(),
            "deploymentsTracked": deployments.len(),
            // No stake anywhere means nothing to disagree about, which is full consensus.
            "overallConsensusRate": if stake_weighted > 0.0 {
                (consensus_weighted / stake_weighted) * 100.0
            } else {
                100.0
            },
            "divergentDeployments": total_divergent,
        },
        "deployments": deployments,
    })
}

/// One delegator of an indexer.
pub fn delegator(r: &Value) -> Value {
    json!({
        "id": t(r, "id"),
        "delegator": json!({ "id": t(r, "delegator") }),
        "stakedTokens": s0(r, "staked_tokens"),
        "shareAmount": s0(r, "share_amount"),
    })
}

// ---------------------------------------------------------------------------------------------
// Direct Indexer Payments
// ---------------------------------------------------------------------------------------------

/// Arbitrum One, reconciled against the contracts repository's address book rather than against a
/// previous reading of this file.
pub mod dips_addr {
    pub const ISSUANCE_ALLOCATOR: &str = "0xb64f29b2d81140ffc3a135e319561a1bd03b1a7e";
    pub const AGREEMENT_MANAGER: &str = "0x51f860b03dee6a6ea27392dcceccd908204149f2";
    pub const RECURRING_COLLECTOR: &str = "0xff0dc7310fbfbcc2524dae230cd4f34727eb84ee";
    /// The indexing-agreement side of the split. Non-zero here means DIPS is funding indexers.
    pub const DEFAULT_ALLOCATION: &str = "0x28cd50e9e02856908f4c1966ab035b1f6c4dde1e";
    pub const REWARDS_MANAGER: &str = "0x971b9d3d0ae3eca029cab5ea1fb0f72c85e6a525";
    pub const INNOVATION_ALLOCATION: &str = "0x2ff06ba8086f37ba656a5b75405bf985f738b16e";
    pub const RECLAIMED_REWARDS: &str = "0xe26cdc4ef915d12551ea67a7cbb838e91a24bb37";
}

/// A name for an address the panel would otherwise render as a bare hex string.
fn dips_label(addr: &str) -> Option<&'static str> {
    Some(match addr {
        dips_addr::DEFAULT_ALLOCATION => "Indexing agreements (DefaultAllocation)",
        dips_addr::REWARDS_MANAGER => "Indexing rewards (RewardsManager)",
        dips_addr::INNOVATION_ALLOCATION => "Innovation allocation (GIP-0089)",
        dips_addr::RECLAIMED_REWARDS => "Reclaimed rewards",
        dips_addr::ISSUANCE_ALLOCATOR => "Issuance Allocator",
        dips_addr::AGREEMENT_MANAGER => "Recurring Agreement Manager",
        dips_addr::RECURRING_COLLECTOR => "Recurring Collector",
        "0x02753bae61c08abd4351bce7f48524935c2cc78e" => "Rewards Eligibility Oracle A",
        "0xeebc4919a239c1315a7e0652e692812719bad591" => "Rewards Eligibility Oracle B",
        "0xae656a0aa51cd465b7506f98f2e8fbb82aa79894" => "Network Operator",
        "0x7700d56d2cfafa620048633b2586b063ecd93dd1" => "Innovation Operator",
        _ => return None,
    })
}

fn dips_step_label(step: &str) -> &str {
    match step {
        "issuance_rate_set" => "Allocator issuance rate set",
        "agreement_manager_wired" => "Agreement manager wired to allocator",
        "collector_pause_guardian_set" => "Collector pause guardian set",
        "eligibility_oracle_set" => "Provider eligibility oracle set",
        "default_target_set" => "Default target set",
        "target_allocation_set" => "Target allocation set",
        other => other,
    }
}

fn dec_grt(v: &Value, key: &str) -> f64 {
    match v.get(key) {
        Some(Value::String(x)) => x.parse::<f64>().unwrap_or(0.0) / 1e18,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) / 1e18,
        _ => 0.0,
    }
}

/// The issuance split, and how it was arrived at.
///
/// Two targets are always present whether or not the allocator has ever mentioned them: the
/// indexing-agreement side and the rewards manager. A panel that hides a target sitting at zero
/// cannot show that it is at zero, which is the whole question DIPS raises.
///
/// `configuredNotDistributed` is the interesting flag: a target with a rate configured that has
/// never received a distribution. Configured and unpaid is a different state from unconfigured,
/// and it is the one worth noticing.
pub fn dips(
    allocations: &[Value],
    timeline_rows: &[Value],
    distributed: &[Value],
    self_minted: &[Value],
) -> Value {
    use std::collections::BTreeMap;

    let mut delivered: BTreeMap<String, i64> = BTreeMap::new();
    for r in distributed.iter().chain(self_minted.iter()) {
        let Some(target) = r.get("target").and_then(|v| v.as_str()) else {
            continue;
        };
        let ts = match r.get("block_timestamp") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        };
        let e = delivered.entry(target.to_lowercase()).or_insert(0);
        *e = (*e).max(ts);
    }

    let observed: BTreeMap<String, &Value> = allocations
        .iter()
        .filter_map(|r| Some((r.get("target")?.as_str()?.to_lowercase(), r)))
        .collect();

    let rate_of =
        |r: &Value| dec_grt(r, "allocator_minting_rate_dec") + dec_grt(r, "self_minting_rate_dec");
    let total_rate: f64 = allocations.iter().map(rate_of).sum();

    let mut targets: Vec<String> = observed.keys().cloned().collect();
    for always in [dips_addr::DEFAULT_ALLOCATION, dips_addr::REWARDS_MANAGER] {
        if !targets.iter().any(|t| t == always) {
            targets.push(always.to_string());
        }
    }

    let mut allocs: Vec<Value> = targets
        .iter()
        .map(|target| {
            let row = observed.get(target);
            let rate = row.map(|r| rate_of(r)).unwrap_or(0.0);
            let last = delivered.get(target).copied();
            json!({
                "target": target,
                "label": dips_label(target).unwrap_or(target),
                "rate": rate,
                "sharePct": if total_rate > 0.0 { (rate / total_rate) * 100.0 } else { 0.0 },
                "selfMinting": row.map(|r| dec_grt(r, "self_minting_rate_dec") > 0.0).unwrap_or(false),
                "lastDistributedAt": last,
                "configuredNotDistributed": rate > 0.0 && last.is_none(),
                "observed": row.is_some(),
            })
        })
        .collect();
    allocs.sort_by(|a, b| {
        b["rate"]
            .as_f64()
            .partial_cmp(&a["rate"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let agreement_rate = observed
        .get(dips_addr::DEFAULT_ALLOCATION)
        .map(|r| rate_of(r))
        .unwrap_or(0.0);

    let timeline: Vec<Value> = timeline_rows
        .iter()
        .map(|r| {
            let step = r.get("step").and_then(|v| v.as_str()).unwrap_or_default();
            let subject = r
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            json!({
                "block": i0(r, "block_number"),
                "timestamp": i0(r, "block_timestamp"),
                "txHash": t(r, "tx_hash"),
                "step": step,
                "label": dips_step_label(step),
                "subject": subject,
                "subjectLabel": dips_label(&subject.to_lowercase()),
                "rate": r.get("rate_dec").filter(|v| !v.is_null()).map(|_| dec_grt(r, "rate_dec")),
            })
        })
        .collect();

    json!({
        "available": true,
        "totalRate": total_rate,
        "agreementRate": agreement_rate,
        // Live means the indexing-agreement side is actually funded, not merely wired up.
        "live": agreement_rate > 0.0,
        "configuredNotDistributed": allocs
            .iter()
            .filter(|a| a["configuredNotDistributed"] == json!(true))
            .map(|a| a["target"].clone())
            .collect::<Vec<_>>(),
        "allocations": allocs,
        "timeline": timeline,
        "lastConfiguredAt": timeline.last().map(|t| t["timestamp"].clone()),
    })
}

// ---------------------------------------------------------------------------------------------
// Deployment identifiers
// ---------------------------------------------------------------------------------------------

/// The `Qm…` IPFS hash for a subgraph deployment id.
///
/// The chain stores a deployment as a bare 32-byte hash; IPFS names the same bytes as a multihash,
/// which is the two-byte prefix `0x1220` (sha2-256, 32 bytes long) followed by the hash, base58
/// encoded. They are the same identifier in two dresses.
///
/// This matters beyond cosmetics. An indexer's status endpoint answers in `Qm…` and the nest
/// answers in hex, so without the conversion the two never join - every allocation looks
/// unserved, and a perfectly healthy indexer reads as serving nothing at all.
pub fn deployment_ipfs_hash(hex_id: &str) -> Option<String> {
    let h = hex_id.strip_prefix("0x").unwrap_or(hex_id);
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(34);
    bytes.push(0x12);
    bytes.push(0x20);
    for i in (0..64).step_by(2) {
        bytes.push(u8::from_str_radix(&h[i..i + 2], 16).ok()?);
    }
    Some(bs58::encode(bytes).into_string())
}

/// The inverse: a `Qm…` CIDv0 back to the bytes32 id the nest stores.
///
/// Strict on purpose. A CIDv0 is exactly the multihash `0x1220 || <32 bytes>`, so anything that
/// decodes to a different length or a different prefix is not one, and returning `None` is the only
/// honest answer. Silently truncating or padding would produce a plausible-looking id that joins to
/// nothing, which is the failure that takes a day to find.
pub fn ipfs_hash_to_bytes32(cid: &str) -> Option<String> {
    let bytes = bs58::decode(cid).into_vec().ok()?;
    if bytes.len() != 34 || bytes[0] != 0x12 || bytes[1] != 0x20 {
        return None;
    }
    let mut out = String::with_capacity(66);
    out.push_str("0x");
    for b in &bytes[2..] {
        out.push_str(&format!("{b:02x}"));
    }
    Some(out)
}

/// The `Qm…` form when the id converts, the id itself otherwise. Never empty, never invented.
pub fn ipfs_or_id(hex_id: &str) -> String {
    deployment_ipfs_hash(hex_id).unwrap_or_else(|| hex_id.to_string())
}

// ---------------------------------------------------------------------------------------------
// The Horizon activity feed
// ---------------------------------------------------------------------------------------------

/// The cache key the activity job writes and the activity route reads.
///
/// In the service this replaces these were two separate stateless invocations, so the feed had to
/// travel through Redis to get from one to the other. One process needs no such courier: the job
/// writes into the same store the route reads, and three network round trips per request go away.
pub const HORIZON_ACTIVITY_KEY: &str = "live:horizon-activity";

/// Where the chain-health job leaves its two answers, for the same reason.
pub const CHAIN_LAG_KEY: &str = "live:chain-lag";
pub const DROPPED_CHAINS_KEY: &str = "live:dropped-chains";

fn delegation_activity_type(event_type: &str) -> &'static str {
    match event_type {
        "undelegation" => "undelegated",
        "withdrawal" => "withdrawn",
        _ => "delegated",
    }
}

/// The newest twenty-five things to happen on HorizonStaking, delegations and provisions merged.
pub fn horizon_activity(delegations: &[Value], provisions: &[Value]) -> Value {
    let mut events: Vec<Value> = Vec::with_capacity(delegations.len() + provisions.len());

    for e in delegations {
        let ts = match e.get("timestamp") {
            Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        events.push(json!({
            "id": format!("d-{}", e.get("id").and_then(|v| v.as_str()).unwrap_or_default()),
            "type": delegation_activity_type(
                e.get("eventType").and_then(|v| v.as_str()).unwrap_or_default()
            ),
            // Delegation events are folded by timestamp rather than by block, so the block is not
            // in the answer. Zero, rather than a guess.
            "block": 0,
            "txHash": t(e, "txHash"),
            "timestamp": ts,
            "serviceProvider": t(e, "indexer"),
            "delegator": t(e, "delegator"),
            "tokensGRT": grt(e, "tokens"),
        }));
    }

    for p in provisions {
        events.push(json!({
            "id": format!(
                "p-{}-{}",
                p.get("tx_hash").and_then(|v| v.as_str()).unwrap_or_default(),
                p.get("log_index").and_then(|v| v.as_i64()).unwrap_or(0)
            ),
            "type": "provision",
            "block": i0(p, "block_number"),
            "txHash": t(p, "tx_hash"),
            "timestamp": i0(p, "block_timestamp"),
            "serviceProvider": t(p, "indexer"),
            "verifier": t(p, "verifier"),
            "tokensGRT": grt(p, "tokens"),
        }));
    }

    events.sort_by_key(|e| std::cmp::Reverse(e["timestamp"].as_i64().unwrap_or(0)));
    events.truncate(25);
    Value::Array(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monday, because that is what the chart's axis claims. A Sunday-start week shifts every
    /// point by a day and looks almost right.
    #[test]
    fn weeks_start_on_monday_in_utc() {
        // 2026-09-07 is a Monday; 2026-09-06 a Sunday.
        let monday = 1_788_739_200; // 2026-09-07T00:00:00Z
        assert_eq!(week_start_utc(monday), "2026-09-07");
        assert_eq!(
            week_start_utc(monday - 1),
            "2026-08-31",
            "Sunday belongs to the week before"
        );
        assert_eq!(week_start_utc(monday + 6 * 86_400), "2026-09-07");
    }

    /// A quiet week is a week with no publications, not a week that did not happen.
    #[test]
    fn empty_weeks_are_emitted_so_the_axis_does_not_compress() {
        let rows = vec![
            json!({"createdAt": 1_785_888_000}), // 2026-08-03 week
            json!({"createdAt": 1_787_097_600}), // two weeks later
        ];
        let out = developer_activity(&rows, 12, 1_788_739_200);
        let weeks = out["weeks"].as_array().unwrap();
        assert_eq!(weeks.len(), 3, "the quiet week in the middle must appear");
        assert_eq!(weeks[1]["count"], json!(0));
        assert_eq!(weeks[2]["cumulative"], json!(2));
    }

    /// The current week is still filling; comparing to it would always read as a collapse.
    #[test]
    fn week_over_week_ignores_the_week_still_in_progress() {
        let now = 1_788_739_200; // a Monday
        let rows = vec![
            json!({"createdAt": now - 14 * 86_400}),
            json!({"createdAt": now - 14 * 86_400}),
            json!({"createdAt": now - 7 * 86_400}),
            json!({"createdAt": now}),
        ];
        let out = developer_activity(&rows, 12, now);
        assert_eq!(
            out["lastWeekCount"],
            json!(1),
            "the last complete week, not the current one"
        );
        assert_eq!(out["weekOverWeekPct"], json!(-50.0));
        assert_eq!(out["totalInWindow"], json!(4));
    }

    #[test]
    fn a_dispute_burn_of_zero_means_none_happened() {
        let r = json!({"id": 1374, "total_rewards": "2000000000000000000", "taxed_query_fees": "500000000000000000"});
        let m = token_metric(&r);
        assert_eq!(m["issuance"], json!(2.0));
        assert_eq!(m["queryFeeTaxBurn"], json!(0.5));
        assert_eq!(m["disputeBurn"], json!(0));
        assert_eq!(m["net"], json!(1.5));
    }

    #[test]
    fn payment_totals_are_summed_exactly_and_parties_counted_once() {
        let accounts = vec![
            json!({"payer": "0xa", "collector": "0xc", "receiver": "0xr", "balance": "1000000000000000000000000", "thawing": "5"}),
            json!({"payer": "0xa", "collector": "0xc", "receiver": "0xs", "balance": "2000000000000000000000000", "thawing": "0"}),
        ];
        let out = payments(&accounts, &[], &[]);
        assert_eq!(
            out["totalEscrowBalance"],
            json!("3000000000000000000000000")
        );
        assert_eq!(out["totalThawing"], json!("5"));
        assert_eq!(out["activePayers"], json!(1), "one payer, twice");
        assert_eq!(out["activeReceivers"], json!(2));
    }

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
    fn a_sampled_call_return_decodes_to_a_decimal_string() {
        assert_eq!(
            decode_uint_word("0x000000000000000000000000000000000000000000000000000000000024ea00"),
            Some("2419200".to_string())
        );
        assert_eq!(decode_uint_word("0x"), None);
        assert_eq!(
            decode_uint_word("not hex"),
            None,
            "a bad word is absent, never zero"
        );
    }

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

    /// Locked stake is spoken for. Counting it as usable would flatter an indexer that has most of
    /// its own stake committed elsewhere.
    #[test]
    fn locked_tokens_do_not_count_as_usable_own_stake() {
        let free = json!({"staked_tokens": "1000000000000000000000000", "locked_tokens": "0",
                          "delegated_tokens": "0", "indexing_reward_cut": 100000});
        let locked = json!({"staked_tokens": "1000000000000000000000000",
                            "locked_tokens": "900000000000000000000000",
                            "delegated_tokens": "0", "indexing_reward_cut": 100000});
        let a = derived_indexer_metrics(&free, 16.0);
        let b = derived_indexer_metrics(&locked, 16.0);
        assert_eq!(a["ownStakeRatio"], json!("1"));
        assert_eq!(
            b["ownStakeRatio"],
            json!("1"),
            "with no delegation both are wholly own-staked"
        );
        // Capacity is measured on the headline stake, which is what the contracts do.
        assert_eq!(a["tokenCapacity"], b["tokenCapacity"]);
    }

    /// Delegating past capacity cannot improve the ratios; it dilutes.
    #[test]
    fn over_delegation_dilutes_rather_than_flatters() {
        let within = json!({"staked_tokens": "100000000000000000000000", "locked_tokens": "0",
                            "delegated_tokens": "800000000000000000000000", "indexing_reward_cut": 100000});
        let beyond = json!({"staked_tokens": "100000000000000000000000", "locked_tokens": "0",
                            "delegated_tokens": "3200000000000000000000000", "indexing_reward_cut": 100000});
        let a = derived_indexer_metrics(&within, 16.0);
        let b = derived_indexer_metrics(&beyond, 16.0);
        assert_eq!(
            a["overDelegationDilution"],
            json!("0"),
            "within capacity, no dilution"
        );
        assert!(
            b["overDelegationDilution"]
                .as_str()
                .unwrap()
                .parse::<f64>()
                .unwrap()
                > 0.4,
            "past capacity the dilution is real: {}",
            b["overDelegationDilution"]
        );
        // Capacity counts delegation, but only up to own stake times the ratio. Delegating four
        // times the cap buys exactly the cap.
        assert_eq!(
            a["tokenCapacity"],
            json!("900000000000000000000000"),
            "100k own + 800k delegated"
        );
        assert_eq!(
            b["tokenCapacity"],
            json!("1700000000000000000000000"),
            "100k own + 1600k, the ceiling, not the 3200k delegated"
        );
    }

    #[test]
    fn an_indexer_with_no_stake_yields_zeroes_rather_than_a_division_by_zero() {
        let empty = json!({"staked_tokens": "0", "locked_tokens": "0", "delegated_tokens": "0"});
        let d = derived_indexer_metrics(&empty, 16.0);
        for k in [
            "ownStakeRatio",
            "delegatedStakeRatio",
            "overDelegationDilution",
        ] {
            assert_eq!(d[k], json!("0"), "{k}");
        }
        assert_eq!(d["tokenCapacity"], json!("0"));
    }

    #[test]
    fn a_profile_carries_its_operators_and_both_allocation_lists() {
        let row = json!({"id": "0xabc", "staked_tokens": "1", "delegated_tokens": "0"});
        let ops = vec![json!({"operator": "0xop"})];
        let active = vec![json!({"id": "0xa1", "subgraph_deployment": "0xdep"})];
        let closed = vec![json!({"id": "0xc1", "subgraph_deployment": "0xdep", "poi": "0xpoi"})];
        let d = indexer_detail(&row, &ops, &active, &closed, 16.0);
        assert_eq!(d["account"]["operators"][0]["id"], json!("0xop"));
        assert_eq!(
            d["allocations"][0]["subgraphDeployment"]["id"],
            json!("0xdep")
        );
        assert_eq!(d["closedAllocations"][0]["poi"], json!("0xpoi"));
        assert!(
            d["ownStakeRatio"].is_string(),
            "the derived ratios are merged in"
        );
    }

    /// Ten small indexers agreeing do not outvote one large one: the network pays on stake.
    #[test]
    fn consensus_is_weighted_by_stake_not_by_headcount() {
        let big = |poi: &str, tokens: &str| {
            json!({"subgraph_deployment": "0xdep", "poi": poi, "indexer": format!("0x{tokens}"),
                   "allocated_tokens": tokens, "closed_at_epoch": 1, "closed_at": 100,
                   "signalled_tokens": "0", "staked_tokens": "0"})
        };
        let rows = vec![
            big("0xtruth", "9000000000000000000000"),
            big("0xwrong", "100000000000000000000"),
            big("0xwrong2", "100000000000000000000"),
        ];
        let out = poi_overview(&rows);
        let d = &out["deployments"][0];
        assert!(
            d["consensusPct"].as_f64().unwrap() > 97.0,
            "{}",
            d["consensusPct"]
        );
        assert_eq!(
            d["divergentCount"],
            json!(2),
            "two proofs disagree, by headcount"
        );
        assert!(d["hasDivergence"].as_bool().unwrap());
    }

    /// Closing without a proof is abstention, not disagreement.
    #[test]
    fn allocations_closed_without_a_proof_are_excluded_not_counted_against() {
        let rows = vec![
            json!({"subgraph_deployment": "0xdep", "poi": "0xtruth", "indexer": "0xa",
                   "allocated_tokens": "1000", "closed_at_epoch": 1, "closed_at": 1,
                   "signalled_tokens": "0", "staked_tokens": "0"}),
            json!({"subgraph_deployment": "0xdep", "poi": ZERO_POI, "indexer": "0xb",
                   "allocated_tokens": "1000", "closed_at_epoch": 1, "closed_at": 1,
                   "signalled_tokens": "0", "staked_tokens": "0"}),
        ];
        let out = poi_overview(&rows);
        assert_eq!(out["summary"]["totalAllocations"], json!(1));
        assert_eq!(out["deployments"][0]["consensusPct"], json!(100.0));
        assert_eq!(out["deployments"][0]["divergentCount"], json!(0));
    }

    /// A divergence that has since resolved is history. Judging on the latest epoch keeps a fixed
    /// deployment from being flagged for ever.
    #[test]
    fn only_the_latest_epoch_decides_the_verdict() {
        let at = |epoch: i64, poi: &str, who: &str| {
            json!({"subgraph_deployment": "0xdep", "poi": poi, "indexer": who,
                   "allocated_tokens": "1000", "closed_at_epoch": epoch, "closed_at": epoch,
                   "signalled_tokens": "0", "staked_tokens": "0"})
        };
        let rows = vec![
            at(1, "0xtruth", "0xa"),
            at(1, "0xwrong", "0xb"),
            at(2, "0xtruth", "0xa"),
            at(2, "0xtruth", "0xb"),
        ];
        let out = poi_overview(&rows);
        let d = &out["deployments"][0];
        assert_eq!(d["latestEpoch"], json!(2));
        assert!(
            !d["hasDivergence"].as_bool().unwrap(),
            "epoch 2 agrees; epoch 1 is history"
        );
        assert_eq!(
            d["allocationCount"],
            json!(4),
            "the count still spans all epochs"
        );
    }

    /// The nest sends integers as strings on some columns and numbers on others, in the same row.
    /// Reading only one form reported perfect consensus across the entire network.
    #[test]
    fn an_epoch_arriving_as_a_string_is_still_an_epoch() {
        let as_string = json!({"subgraph_deployment": "0xdep", "poi": "0xa", "indexer": "0xi",
                               "allocated_tokens": "1000", "closed_at_epoch": "1374",
                               "closed_at": 100, "signalled_tokens": "0", "staked_tokens": "0"});
        let out = poi_overview(&[as_string]);
        assert_eq!(out["deployments"][0]["latestEpoch"], json!(1374));
        assert_eq!(
            out["deployments"][0]["consensusPct"],
            json!(100.0),
            "one proof, unopposed, is full consensus - not zero"
        );
    }

    #[test]
    fn nothing_at_stake_is_full_consensus_rather_than_none() {
        let out = poi_overview(&[]);
        assert_eq!(out["summary"]["overallConsensusRate"], json!(100.0));
        assert_eq!(out["summary"]["deploymentsTracked"], json!(0));
    }

    /// A target sitting at zero must still appear. A panel that hides it cannot show that DIPS is
    /// configured and unfunded, which is the whole question the panel exists to answer.
    #[test]
    fn the_two_headline_targets_appear_even_when_the_allocator_never_mentions_them() {
        let out = dips(&[], &[], &[], &[]);
        let targets: Vec<&str> = out["allocations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["target"].as_str().unwrap())
            .collect();
        assert!(targets.contains(&dips_addr::DEFAULT_ALLOCATION));
        assert!(targets.contains(&dips_addr::REWARDS_MANAGER));
        assert_eq!(out["live"], json!(false), "nothing configured is not live");
        assert_eq!(out["agreementRate"], json!(0.0));
    }

    /// Configured and unpaid is a different state from unconfigured, and the one worth noticing.
    #[test]
    fn a_configured_target_that_has_never_been_paid_is_flagged() {
        let allocations = vec![json!({
            "target": dips_addr::DEFAULT_ALLOCATION,
            "allocator_minting_rate_dec": "1000000000000000000",
            "self_minting_rate_dec": "0"
        })];
        let out = dips(&allocations, &[], &[], &[]);
        let a = &out["allocations"][0];
        assert_eq!(a["configuredNotDistributed"], json!(true));
        assert_eq!(out["live"], json!(true), "a funded agreement side is live");
        assert_eq!(
            out["configuredNotDistributed"][0],
            json!(dips_addr::DEFAULT_ALLOCATION)
        );

        // Once a distribution lands, the flag clears.
        let paid = vec![json!({"target": dips_addr::DEFAULT_ALLOCATION, "block_timestamp": 1700})];
        let out2 = dips(&allocations, &[], &paid, &[]);
        assert_eq!(
            out2["allocations"][0]["configuredNotDistributed"],
            json!(false)
        );
        assert_eq!(out2["allocations"][0]["lastDistributedAt"], json!(1700));
    }

    #[test]
    fn a_known_address_is_named_and_an_unknown_one_is_shown_as_itself() {
        assert_eq!(
            dips_label(dips_addr::REWARDS_MANAGER),
            Some("Indexing rewards (RewardsManager)")
        );
        assert_eq!(dips_label("0xdeadbeef"), None);
        assert_eq!(
            dips_step_label("issuance_rate_set"),
            "Allocator issuance rate set"
        );
        assert_eq!(dips_step_label("something_new"), "something_new");
    }

    /// A real pair, taken from the incumbent's own POI output. Getting this wrong silently unjoins
    /// every allocation from every status an indexer reports.
    #[test]
    fn a_cid_round_trips_back_to_the_id_it_came_from() {
        // The same real pair the forward test pins, walked the other way.
        let id = "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff";
        let cid = "Qmd5j2CeMs5NBUASQWjGbFmFpxGWhwKbgrTBZwV8hj65QJ";
        assert_eq!(ipfs_hash_to_bytes32(cid).as_deref(), Some(id));
        assert_eq!(deployment_ipfs_hash(id).as_deref(), Some(cid));
    }

    /// Anything that is not a CIDv0 must fail rather than produce a plausible id.
    #[test]
    fn a_non_cid_does_not_become_an_id() {
        assert_eq!(ipfs_hash_to_bytes32(""), None);
        assert_eq!(ipfs_hash_to_bytes32("not base58 at all!"), None);
        // Valid base58, wrong length - the case a lenient decoder would wave through.
        assert_eq!(ipfs_hash_to_bytes32("QmXoypiz"), None);
        assert_eq!(
            ipfs_hash_to_bytes32(
                "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff"
            ),
            None
        );
    }

    #[test]
    fn a_deployment_id_converts_to_the_ipfs_hash_the_indexer_reports() {
        assert_eq!(
            deployment_ipfs_hash(
                "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff"
            )
            .as_deref(),
            Some("Qmd5j2CeMs5NBUASQWjGbFmFpxGWhwKbgrTBZwV8hj65QJ")
        );
    }

    #[test]
    fn anything_that_is_not_a_deployment_id_is_left_alone() {
        assert_eq!(deployment_ipfs_hash("0xdeadbeef"), None);
        assert_eq!(deployment_ipfs_hash(""), None);
        assert_eq!(
            ipfs_or_id("0xdeadbeef"),
            "0xdeadbeef",
            "returned as-is, never blanked"
        );
    }

    #[test]
    fn the_activity_feed_is_newest_first_and_capped() {
        let delegations: Vec<Value> = (0..30)
            .map(|i| {
                json!({"id": format!("e{i}"), "eventType": "delegation", "indexer": "0xi",
                            "delegator": "0xd", "tokens": "1000000000000000000",
                            "timestamp": (1_700_000_000 + i).to_string(), "txHash": "0xt"})
            })
            .collect();
        let out = horizon_activity(&delegations, &[]);
        let arr = out.as_array().unwrap();
        assert_eq!(
            arr.len(),
            25,
            "the feed is capped, not truncated by the caller"
        );
        assert!(
            arr[0]["timestamp"].as_i64().unwrap() > arr[1]["timestamp"].as_i64().unwrap(),
            "newest first"
        );
        assert_eq!(arr[0]["tokensGRT"], json!(1.0));
    }

    #[test]
    fn delegation_kinds_map_to_the_feeds_own_vocabulary() {
        assert_eq!(delegation_activity_type("delegation"), "delegated");
        assert_eq!(delegation_activity_type("undelegation"), "undelegated");
        assert_eq!(delegation_activity_type("withdrawal"), "withdrawn");
        assert_eq!(delegation_activity_type("something_new"), "delegated");
    }

    #[test]
    fn provisions_and_delegations_interleave_by_time() {
        let d = vec![
            json!({"id": "e1", "eventType": "delegation", "indexer": "0xi",
                            "delegator": "0xd", "tokens": "0", "timestamp": "100", "txHash": "0xa"}),
        ];
        let p = vec![json!({"tx_hash": "0xb", "log_index": 1, "block_number": 5,
                            "block_timestamp": 200, "indexer": "0xi", "verifier": "0xv",
                            "tokens": "0"})];
        let out = horizon_activity(&d, &p);
        let arr = out.as_array().unwrap();
        assert_eq!(arr[0]["type"], json!("provision"), "the later event leads");
        assert_eq!(arr[1]["type"], json!("delegated"));
    }
}

/// One point on an indexer's stake history.
///
/// The nest answers a cutoff and two wei balances; the panel plots a date and two GRT figures. The
/// date is derived from the cutoff rather than sent alongside it, because the cutoff is the thing
/// the query was built from and a second field would be a second chance to disagree with it.
pub fn stake_history_point(r: &Value) -> Value {
    let cutoff = i0(r, "cutoff").as_i64().unwrap_or(0);
    json!({
        "date": iso_date(cutoff),
        "selfStakeGrt": num(grt(r, "staked_tokens")),
        "delegatedGrt": num(grt(r, "delegated_tokens")),
    })
}

/// A unix second to `YYYY-MM-DD`, which is what the chart's axis reads.
fn iso_date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// A float as JSON, or zero rather than `null` when it is not representable.
///
/// A chart with a null in the series draws a gap; a chart with a zero draws a zero. Neither is
/// ideal for a NaN, and zero is the one that does not break the axis.
fn num(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(json!(0))
}

#[cfg(test)]
mod stake_history_tests {
    use super::*;

    #[test]
    fn a_row_becomes_the_point_the_chart_plots() {
        let row = json!({
            "cutoff": 1_757_000_000,
            "staked_tokens": "136728062185740942355425427",
            "delegated_tokens": "0",
        });
        let p = stake_history_point(&row);
        assert_eq!(p["date"], json!("2025-09-04"));
        // 136,728,062.18... GRT, not 1.36e26.
        let self_stake = p["selfStakeGrt"].as_f64().unwrap();
        assert!(
            (self_stake - 136_728_062.185_740_94).abs() < 1.0,
            "got {self_stake}"
        );
        assert_eq!(p["delegatedGrt"].as_f64(), Some(0.0));
    }

    /// Wei is divided, never truncated to an integer and never left as the raw string: a chart
    /// plotting 1.3e26 has an axis nobody can read.
    #[test]
    fn a_missing_or_unparseable_balance_is_zero_rather_than_absent() {
        let p = stake_history_point(&json!({"cutoff": 0}));
        assert_eq!(p["selfStakeGrt"].as_f64(), Some(0.0));
        assert_eq!(p["delegatedGrt"].as_f64(), Some(0.0));
        assert_eq!(p["date"], json!("1970-01-01"));
    }
}

/// A deployment's weekly signal and stake series, folded from the two event lists.
///
/// The nest answers what happened; the chart plots what was true. Signal is the running sum of
/// `MintSignal` less `BurnSignal` up to each week, and stake is the total allocated by every
/// allocation that was open at that moment - created before it and either still open or closed
/// after it. Neither is a column anywhere; both have to be folded.
///
/// Three details that are easy to get subtly wrong, all matching the handler this replaces:
///
/// - The window starts at the earliest event **or** a year ago, whichever is later. A deployment
///   from 2021 would otherwise plot three hundred points nobody reads.
/// - Signal is floored at zero. Burns can exceed mints in the window when the mints happened before
///   it, and a negative signal is not a thing that exists.
/// - The leading flat section is trimmed at 2% of peak. A deployment signalled in month nine plots
///   thirty-five weeks of zero before anything happens, and the interesting part ends up squeezed
///   into the right-hand tenth of the chart.
pub fn subgraph_history(combined: &Value) -> Value {
    const WEEK: i64 = 7 * 86_400;
    const YEAR: i64 = 365 * 86_400;

    let mut signals: Vec<(i64, f64)> = combined
        .get("signals")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let ts = i0(r, "timestamp").as_i64().unwrap_or(0);
                    let tokens = grt(r, "tokens");
                    let minted = s(r, "type").as_str() == Some("MintSignal");
                    (ts, if minted { tokens } else { -tokens })
                })
                .collect()
        })
        .unwrap_or_default();
    signals.sort_by_key(|(ts, _)| *ts);

    let allocations: Vec<(i64, Option<i64>, f64)> = combined
        .get("allocations")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    (
                        i0(r, "created_at").as_i64().unwrap_or(0),
                        r.get("closed_at").and_then(|v| match v {
                            Value::Number(n) => n.as_i64(),
                            Value::String(x) => x.parse().ok(),
                            _ => None,
                        }),
                        grt(r, "allocated_tokens"),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    if signals.is_empty() && allocations.is_empty() {
        return json!({ "history": [] });
    }

    let now = chrono::Utc::now().timestamp();
    let earliest = signals
        .iter()
        .map(|(ts, _)| *ts)
        .chain(allocations.iter().map(|(c, _, _)| *c))
        .filter(|t| *t > 0)
        .min()
        .unwrap_or(now);
    let start = earliest.max(now - YEAR);

    let mut history = Vec::new();
    let mut ts = start;
    while ts <= now {
        let signal: f64 = signals
            .iter()
            .take_while(|(t, _)| *t <= ts)
            .map(|(_, v)| v)
            .sum();
        let stake: f64 = allocations
            .iter()
            .filter(|(created, closed, _)| *created <= ts && closed.is_none_or(|c| c > ts))
            .map(|(_, _, tokens)| tokens)
            .sum();
        history.push(json!({
            "date": iso_date(ts),
            "signalGrt": num(signal.max(0.0)),
            "stakeGrt": num(stake),
        }));
        ts += WEEK;
    }

    let peak = history
        .iter()
        .map(|p| p["signalGrt"].as_f64().unwrap_or(0.0) + p["stakeGrt"].as_f64().unwrap_or(0.0))
        .fold(0.0f64, f64::max);
    let threshold = peak * 0.02;
    let first = history
        .iter()
        .position(|p| {
            p["signalGrt"].as_f64().unwrap_or(0.0) + p["stakeGrt"].as_f64().unwrap_or(0.0)
                >= threshold
        })
        .unwrap_or(0);
    json!({ "history": history.split_off(first) })
}

#[cfg(test)]
mod subgraph_history_tests {
    use super::*;

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn nothing_at_all_is_an_empty_series_rather_than_a_year_of_zeroes() {
        let out = subgraph_history(&json!({"signals": [], "allocations": []}));
        assert_eq!(out["history"], json!([]));
    }

    #[test]
    fn signal_is_mints_less_burns_and_stake_is_what_was_open() {
        let t = now() - 20 * 86_400;
        let out = subgraph_history(&json!({
            "signals": [
                {"timestamp": t, "type": "MintSignal", "tokens": "3000000000000000000"},
                {"timestamp": t + 86_400, "type": "BurnSignal", "tokens": "1000000000000000000"},
            ],
            "allocations": [
                {"created_at": t, "closed_at": null, "allocated_tokens": "5000000000000000000"},
            ],
        }));
        let h = out["history"].as_array().unwrap();
        let last = h.last().unwrap();
        assert!((last["signalGrt"].as_f64().unwrap() - 2.0).abs() < 1e-9);
        assert!((last["stakeGrt"].as_f64().unwrap() - 5.0).abs() < 1e-9);
    }

    /// An allocation closed before a point contributes nothing to it. Counting it would make stake
    /// only ever rise, which is the bug that turns a chart into a staircase.
    #[test]
    fn a_closed_allocation_stops_counting_from_the_week_it_closed() {
        let t = now() - 60 * 86_400;
        let out = subgraph_history(&json!({
            "signals": [{"timestamp": t, "type": "MintSignal", "tokens": "1000000000000000000"}],
            "allocations": [
                {"created_at": t, "closed_at": t + 7 * 86_400, "allocated_tokens": "9000000000000000000"},
            ],
        }));
        let h = out["history"].as_array().unwrap();
        assert!(h.first().unwrap()["stakeGrt"].as_f64().unwrap() > 0.0);
        assert_eq!(h.last().unwrap()["stakeGrt"].as_f64().unwrap(), 0.0);
    }

    /// Burns exceeding mints inside the window is ordinary when the mints predate it.
    #[test]
    fn signal_never_goes_negative() {
        let t = now() - 30 * 86_400;
        let out = subgraph_history(&json!({
            "signals": [{"timestamp": t, "type": "BurnSignal", "tokens": "4000000000000000000"}],
            "allocations": [],
        }));
        for p in out["history"].as_array().unwrap() {
            assert!(p["signalGrt"].as_f64().unwrap() >= 0.0);
        }
    }
}

/// Where the GRT supply came from and where it went.
///
/// Mints and burns are summed as **integers** and divided once at the end. They are wei totals in
/// the billions of tokens; adding them as `f64` first rounds at every step, where summing exactly
/// and dividing once rounds only at the end. That last rounding is unavoidable - an `f64` holds
/// about sixteen significant figures and 1.006e27 wei needs more - so the answer is right to the
/// nearest ten-millionth of a GRT rather than exact. For a chart that is fine; anything that must
/// be exact keeps the wei string.
///
/// `globalSupply` prefers the three contract reads over the constant, and says which it used in
/// `supplyBasis`. The approximation is there so the page renders when a chain is unreachable; a
/// reader quoting an issuance rate deserves to know it was computed against a guess.
pub fn grt_flow(
    n: &Value,
    p: &Value,
    supply: Option<&kittiwake_chain::SupplyBreakdown>,
    as_of: i64,
) -> Value {
    /// The circulating supply used when the contracts cannot be read. Approximate by construction.
    const CIRCULATING_SUPPLY_APPROX: f64 = 11_500_000_000.0;

    let wei = |v: &Value, k: &str| -> u128 {
        match v.get(k) {
            Some(Value::String(x)) => x.parse().unwrap_or(0),
            Some(Value::Number(num)) => num.as_u64().unwrap_or(0) as u128,
            _ => 0,
        }
    };
    let as_grt = |x: u128| num(x as f64 / 1e18);

    let global_supply = supply
        .map(|b| b.global_supply)
        .unwrap_or(CIRCULATING_SUPPLY_APPROX);
    let issuance_per_block = grt(n, "issuance_per_block");
    let annual_issuance = issuance_per_block * kittiwake_rewards::L1_BLOCKS_PER_YEAR;
    let issuance_rate_pct = if global_supply > 0.0 && issuance_per_block >= 0.0 {
        (annual_issuance / global_supply) * 100.0
    } else {
        0.0
    };

    let minted = wei(n, "bridge_minted") + wei(n, "total_indexing_rewards");
    let burned =
        wei(n, "bridge_burned") + wei(p, "total_curation_tax") + wei(p, "total_protocol_tax");

    // Parts per million to percent, as the protocol denominates its cuts.
    let ppm = |v: &Value, k: &str| num(i0(v, k).as_f64().unwrap_or(0.0) / 10_000.0);

    json!({
        "blockNumber": as_of,
        "supply": num(grt(n, "total_supply")),
        "globalSupply": num(global_supply),
        "supplyBasis": if supply.is_some() { "onchain" } else { "approx" },
        "supplyBreakdown": supply.map(|b| json!({
            "l1TotalSupply": num(b.l1_total_supply),
            "l2TotalSupply": num(b.l2_total_supply),
            "bridgeEscrow": num(b.bridge_escrow),
            "globalSupply": num(b.global_supply),
        })),
        "minted": as_grt(minted),
        "burned": as_grt(burned),
        "indexingRewards": num(grt(n, "total_indexing_rewards")),
        "queryFees": num(grt(n, "total_query_fees")),
        "staked": num(grt(n, "total_tokens_staked")),
        "delegated": num(grt(n, "total_delegated_tokens")),
        "signalled": num(grt(n, "total_tokens_signalled")),
        "allocated": num(grt(n, "total_tokens_allocated")),
        "issuancePerBlock": num(issuance_per_block),
        "annualIssuance": num(annual_issuance),
        "issuanceRatePct": num(issuance_rate_pct),
        "counts": {
            "indexers": i0(n, "indexer_count"),
            "stakedIndexers": i0(n, "staked_indexers_count"),
            "delegators": i0(n, "delegator_count"),
            "curators": i0(n, "curator_count"),
            "currentEpoch": i0(n, "current_epoch"),
        },
        "params": {
            "protocolFeePct": ppm(p, "protocol_payment_cut"),
            "curationTaxPct": ppm(p, "curation_tax_percentage"),
            // No Horizon equivalent; the delegation tax was removed with the upgrade.
            "delegationTaxPct": 0,
            "delegationRatio": i0(p, "delegation_ratio"),
        },
        "source": "nuthatch",
    })
}

#[cfg(test)]
mod grt_flow_tests {
    use super::*;

    fn net() -> Value {
        json!({
            "total_supply": "10800000000000000000000000000",
            "total_indexing_rewards": "3000000000000000000000000000",
            "bridge_minted": "2000000000000000000000000000",
            "bridge_burned": "1000000000000000000000000000",
            "total_query_fees": "500000000000000000000000",
            "total_tokens_staked": "900000000000000000000000000",
            "total_delegated_tokens": "400000000000000000000000000",
            "total_tokens_signalled": "20000000000000000000000000",
            "total_tokens_allocated": "800000000000000000000000000",
            "issuance_per_block": "114000000000000000000",
            "indexer_count": 184,
            "current_epoch": 1374,
        })
    }

    fn params() -> Value {
        json!({
            "total_curation_tax": "5000000000000000000000000",
            "total_protocol_tax": "1000000000000000000000000",
            "protocol_payment_cut": 10000,
            "curation_tax_percentage": 10000,
            "delegation_ratio": 16,
        })
    }

    /// Mints and burns are sums of two wei totals in the billions. Adding them as floats first and
    /// dividing after loses the low digits before the division; this pins the exact figure.
    #[test]
    fn mints_and_burns_are_summed_as_integers_and_divided_once() {
        let f = grt_flow(&net(), &params(), None, 100);
        // 2e9 bridge + 3e9 rewards.
        assert_eq!(f["minted"].as_f64().unwrap(), 5_000_000_000.0);
        // 1e9 bridge + 5e6 curation + 1e6 protocol. Within a ten-millionth of a GRT: the sum is
        // exact and the single division to f64 is where the last digit goes.
        assert!(
            (f["burned"].as_f64().unwrap() - 1_006_000_000.0).abs() < 1e-6,
            "got {}",
            f["burned"]
        );
    }

    /// The reader is told which supply the issuance rate was computed against, because one of them
    /// is a guess.
    #[test]
    fn the_supply_basis_says_whether_the_chain_answered() {
        let f = grt_flow(&net(), &params(), None, 100);
        assert_eq!(f["supplyBasis"], json!("approx"));
        assert_eq!(f["globalSupply"].as_f64().unwrap(), 11_500_000_000.0);
        assert_eq!(f["supplyBreakdown"], Value::Null);

        let b = kittiwake_chain::SupplyBreakdown {
            l1_total_supply: 8.0e9,
            l2_total_supply: 4.0e9,
            bridge_escrow: 1.0e9,
            global_supply: 11.0e9,
        };
        let f = grt_flow(&net(), &params(), Some(&b), 100);
        assert_eq!(f["supplyBasis"], json!("onchain"));
        assert_eq!(f["globalSupply"].as_f64().unwrap(), 11.0e9);
        assert_eq!(
            f["supplyBreakdown"]["bridgeEscrow"].as_f64().unwrap(),
            1.0e9
        );
    }

    #[test]
    fn issuance_uses_the_one_blocks_per_year_figure() {
        let f = grt_flow(&net(), &params(), None, 100);
        let per_block = f["issuancePerBlock"].as_f64().unwrap();
        assert!((per_block - 114.0).abs() < 1e-9);
        let annual = f["annualIssuance"].as_f64().unwrap();
        assert!(
            (annual - 114.0 * kittiwake_rewards::L1_BLOCKS_PER_YEAR).abs() < 1.0,
            "got {annual}"
        );
        // Rate is annual issuance over the supply it was computed against.
        let rate = f["issuanceRatePct"].as_f64().unwrap();
        assert!((rate - (annual / 11_500_000_000.0) * 100.0).abs() < 1e-9);
    }

    /// A ppm cut of 10,000 is one percent, not ten thousand.
    #[test]
    fn cuts_are_parts_per_million_rather_than_percentages() {
        let f = grt_flow(&net(), &params(), None, 100);
        assert_eq!(f["params"]["protocolFeePct"].as_f64().unwrap(), 1.0);
        assert_eq!(f["params"]["curationTaxPct"].as_f64().unwrap(), 1.0);
        assert_eq!(f["params"]["delegationTaxPct"].as_i64().unwrap(), 0);
    }

    /// Nothing in the row must panic or invent a figure.
    #[test]
    fn an_empty_row_gives_zeroes_rather_than_nonsense() {
        let f = grt_flow(&json!({}), &json!({}), None, 0);
        assert_eq!(f["minted"].as_f64().unwrap(), 0.0);
        assert_eq!(f["issuanceRatePct"].as_f64().unwrap(), 0.0);
        assert_eq!(f["blockNumber"], json!(0));
    }
}

/// A deployment's curation: who signalled it, and the deployment's own totals.
///
/// The two totals repeat on every row because they belong to the deployment rather than to any
/// curator, so they are lifted off the first row rather than summed. Summing them would multiply
/// the deployment's signal by its number of curators, which is a plausible number and completely
/// wrong - the kind that survives a glance.
///
/// Wei stays a string throughout. These are balances a reader may compare or quote, not magnitudes
/// for a chart, and the moment one becomes an `f64` the last digits stop being true.
pub fn subgraph_curation(rows: &[Value]) -> Value {
    let Some(first) = rows.first() else {
        return json!({ "signals": [], "totalSignalledTokens": "0", "queryFeesAmount": "0" });
    };
    let signals: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": t(r, "id"),
                "curatorAddress": t(r, "curator"),
                "signalledTokens": s0(r, "signalled_tokens"),
                "unsignalledTokens": s0(r, "unsignalled_tokens"),
                "signal": s0(r, "signal"),
                "lastSignalChange": i0(r, "last_signal_change"),
                "realizedRewards": s0(r, "realized_rewards"),
            })
        })
        .collect();
    json!({
        "signals": signals,
        "totalSignalledTokens": s0(first, "deployment_signalled_tokens"),
        "queryFeesAmount": s0(first, "deployment_query_fees_amount"),
    })
}

#[cfg(test)]
mod subgraph_curation_tests {
    use super::*;

    fn row(curator: &str, signalled: &str) -> Value {
        json!({
            "id": format!("sig-{curator}"),
            "curator": curator,
            "signalled_tokens": signalled,
            "unsignalled_tokens": "0",
            "signal": "1000",
            "last_signal_change": 1_757_000_000_i64,
            "realized_rewards": "7",
            "deployment_signalled_tokens": "4851646996536021200956",
            "deployment_query_fees_amount": "123456",
        })
    }

    /// The totals belong to the deployment, not to the curators, so two curators must not double
    /// them. The bug this guards is a sum where a lift was wanted.
    #[test]
    fn the_deployment_totals_are_lifted_not_summed() {
        let out = subgraph_curation(&[row("0xaa", "100"), row("0xbb", "200")]);
        assert_eq!(out["signals"].as_array().unwrap().len(), 2);
        assert_eq!(out["totalSignalledTokens"], json!("4851646996536021200956"));
        assert_eq!(out["queryFeesAmount"], json!("123456"));
    }

    /// Wei never becomes a number here: these are balances, and a curator comparing their own
    /// position against the total deserves both to be exact.
    #[test]
    fn balances_stay_strings() {
        let out = subgraph_curation(&[row("0xaa", "4851646996536021200956")]);
        let sig = &out["signals"][0];
        assert!(sig["signalledTokens"].is_string());
        assert_eq!(sig["signalledTokens"], json!("4851646996536021200956"));
        assert!(out["totalSignalledTokens"].is_string());
    }

    #[test]
    fn a_deployment_nobody_signalled_is_zeroes_rather_than_absent() {
        let out = subgraph_curation(&[]);
        assert_eq!(out["signals"], json!([]));
        assert_eq!(out["totalSignalledTokens"], json!("0"));
        assert_eq!(out["queryFeesAmount"], json!("0"));
    }

    #[test]
    fn the_curator_is_named_as_the_frontend_reads_it() {
        let out = subgraph_curation(&[row("0xabc", "1")]);
        assert_eq!(out["signals"][0]["curatorAddress"], json!("0xabc"));
        assert_eq!(
            out["signals"][0]["lastSignalChange"],
            json!(1_757_000_000_i64)
        );
    }
}

/// A delegator's portfolio value over time, folded from their positions and each indexer's daily
/// delegation exchange rate.
///
/// The rate is what a share is worth in GRT, so a position's value is shares times rate and the
/// rewards are whatever that exceeds the principal. Two details carry the whole thing:
///
/// **Forward fill.** A rate is only snapshotted when it changes, so most indexers have no row on
/// most days. Leaving those days out would make the portfolio appear to shrink and grow as
/// individual indexers dropped in and out of the sum. The last known rate carries forward.
///
/// **Before the first snapshot, principal stands in.** An indexer with no rate yet contributes its
/// principal rather than nothing, so a new position does not read as a loss of its own value on the
/// day it opens.
///
/// Rewards are floored at zero. A rate below one is arithmetically possible after a slashing and it
/// is not a negative reward, it is a loss, and this series does not claim to measure those.
pub fn rewards_history(positions: &[(String, f64, f64)], rates: &[(String, String, f64)]) -> Value {
    use std::collections::{BTreeMap, BTreeSet};

    if positions.is_empty() {
        return json!({ "history": [] });
    }

    let mut by_indexer: BTreeMap<&str, BTreeMap<&str, f64>> = BTreeMap::new();
    let mut dates: BTreeSet<&str> = BTreeSet::new();
    for (indexer, date, rate) in rates {
        by_indexer
            .entry(indexer.as_str())
            .or_default()
            .insert(date.as_str(), *rate);
        dates.insert(date.as_str());
    }
    if dates.is_empty() {
        return json!({ "history": [] });
    }
    let dates: Vec<&str> = dates.into_iter().collect();

    // Forward fill, in date order, per indexer.
    for rates in by_indexer.values_mut() {
        let mut last: Option<f64> = None;
        for d in &dates {
            match rates.get(d) {
                Some(r) => last = Some(*r),
                None => {
                    if let Some(r) = last {
                        rates.insert(d, r);
                    }
                }
            }
        }
    }

    let total_principal: f64 = positions.iter().map(|(_, principal, _)| principal).sum();
    let history: Vec<Value> = dates
        .iter()
        .map(|date| {
            let total_value: f64 = positions
                .iter()
                .map(|(indexer, principal, shares)| {
                    match by_indexer.get(indexer.as_str()).and_then(|r| r.get(date)) {
                        Some(rate) => shares * rate,
                        None => *principal,
                    }
                })
                .sum();
            json!({
                "date": short_date(date),
                "timestamp": date_millis(date),
                "value": num(round2(total_value)),
                "rewards": num(round2((total_value - total_principal).max(0.0))),
                "principal": num(round2(total_principal)),
            })
        })
        .collect();
    json!({ "history": history })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// `2026-09-07` to `9/7`, which is what the chart's axis carries.
fn short_date(iso: &str) -> String {
    let mut parts = iso.split('-');
    let (_, m, d) = (parts.next(), parts.next(), parts.next());
    match (m, d) {
        (Some(m), Some(d)) => format!(
            "{}/{}",
            m.trim_start_matches('0'),
            d.trim_start_matches('0')
        ),
        _ => iso.to_string(),
    }
}

fn date_millis(iso: &str) -> Value {
    chrono::NaiveDate::parse_from_str(iso, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| json!(dt.and_utc().timestamp_millis()))
        .unwrap_or(json!(0))
}

#[cfg(test)]
mod rewards_history_tests {
    use super::*;

    fn pos(indexer: &str, principal: f64, shares: f64) -> (String, f64, f64) {
        (indexer.to_string(), principal, shares)
    }
    fn rate(indexer: &str, date: &str, r: f64) -> (String, String, f64) {
        (indexer.to_string(), date.to_string(), r)
    }

    #[test]
    fn no_positions_is_an_empty_series() {
        assert_eq!(rewards_history(&[], &[])["history"], json!([]));
        assert_eq!(
            rewards_history(&[pos("0xa", 100.0, 100.0)], &[])["history"],
            json!([])
        );
    }

    /// The detail the whole fold hangs on. A rate is snapshotted only when it changes, so an
    /// indexer missing from a day must keep its last known rate. Without this the portfolio
    /// visibly shrinks and grows as indexers drop in and out of the sum.
    #[test]
    fn a_missing_day_carries_the_last_known_rate_forward() {
        let out = rewards_history(
            &[pos("0xa", 100.0, 100.0)],
            &[
                rate("0xa", "2026-09-01", 1.10),
                rate("0xb", "2026-09-02", 1.00),
                rate("0xa", "2026-09-03", 1.20),
            ],
        );
        let h = out["history"].as_array().unwrap();
        assert_eq!(h.len(), 3);
        // Day two has no row for 0xa, so its day-one rate carries.
        assert_eq!(h[1]["value"].as_f64().unwrap(), 110.0);
        assert_eq!(h[2]["value"].as_f64().unwrap(), 120.0);
    }

    /// Before an indexer's first snapshot there is nothing to carry forward, and the position is
    /// worth its principal rather than nothing.
    #[test]
    fn a_position_with_no_rate_yet_is_worth_its_principal() {
        let out = rewards_history(
            &[pos("0xa", 500.0, 500.0)],
            &[rate("0xb", "2026-09-01", 1.0)],
        );
        let h = out["history"].as_array().unwrap();
        assert_eq!(h[0]["value"].as_f64().unwrap(), 500.0);
        assert_eq!(h[0]["rewards"].as_f64().unwrap(), 0.0);
    }

    /// A rate below one is a loss, not a negative reward.
    #[test]
    fn rewards_never_go_negative() {
        let out = rewards_history(
            &[pos("0xa", 100.0, 100.0)],
            &[rate("0xa", "2026-09-01", 0.80)],
        );
        let h = out["history"].as_array().unwrap();
        assert_eq!(h[0]["value"].as_f64().unwrap(), 80.0);
        assert_eq!(h[0]["rewards"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn the_axis_label_is_month_slash_day_without_leading_zeroes() {
        assert_eq!(short_date("2026-09-07"), "9/7");
        assert_eq!(short_date("2026-12-25"), "12/25");
    }

    #[test]
    fn two_positions_are_summed_and_the_principal_is_the_total() {
        let out = rewards_history(
            &[pos("0xa", 100.0, 100.0), pos("0xb", 200.0, 200.0)],
            &[
                rate("0xa", "2026-09-01", 1.5),
                rate("0xb", "2026-09-01", 1.0),
            ],
        );
        let h = &out["history"][0];
        assert_eq!(h["principal"].as_f64().unwrap(), 300.0);
        assert_eq!(h["value"].as_f64().unwrap(), 350.0);
        assert_eq!(h["rewards"].as_f64().unwrap(), 50.0);
    }
}

/// The trail behind a delegator's APR: money in and out, and the cuts the indexer set.
///
/// Newest first, the two sources interleaved by time. A parameter change where the old value equals
/// the new one is dropped: the ingest records a row per epoch the parameter was seen in, so an
/// unchanged cut would otherwise appear as an event every epoch for ever.
pub fn provenance_events(
    delegations: &[(String, Option<String>, Option<f64>, String)],
    params: &[(String, Option<i64>, i64, String)],
) -> Value {
    let mut events: Vec<(String, Value)> = Vec::new();

    for (event_type, delegator, tokens, ts) in delegations {
        let kind = match event_type.as_str() {
            "delegation" => "delegation",
            "undelegation" => "undelegation",
            "withdrawal" => "withdrawal",
            _ => continue,
        };
        events.push((
            ts.clone(),
            json!({
                "kind": kind,
                "timestamp": ts,
                "tokensGRT": tokens.map(num),
                "delegator": delegator.as_ref().map(|d| d.to_lowercase()),
                // ENS is resolved by the caller that has a mainnet RPC, not here.
                "delegatorName": Value::Null,
            }),
        ));
    }

    for (name, old, new, ts) in params {
        let kind = match name.as_str() {
            "reward_cut" => "reward_cut",
            "query_fee_cut" => "query_fee_cut",
            _ => continue,
        };
        if *old == Some(*new) {
            continue;
        }
        events.push((
            ts.clone(),
            json!({
                "kind": kind,
                "timestamp": ts,
                "oldValue": old,
                "newValue": new,
            }),
        ));
    }

    events.sort_by(|a, b| b.0.cmp(&a.0));
    json!(events.into_iter().map(|(_, e)| e).collect::<Vec<_>>())
}

/// How far the nest's delegation figures have drifted from the chain's.
///
/// `tokens - thawing` on both sides, because the contract keeps fully-thawed-but-not-withdrawn
/// tokens inside both figures until `withdraw()` fires. Any difference is indexing lag rather than
/// a difference of meaning, which is why a drift within the tolerance reads as verified rather than
/// as merely close.
pub fn pool_reconciliation(
    chain: Option<(f64, f64)>,
    nest_delegated: f64,
    nest_thawing: f64,
) -> Value {
    /// Half a percent. Wide enough for a block or two of lag, narrow enough that a real divergence
    /// does not hide inside it.
    const EPSILON: f64 = 0.005;

    let Some((chain_tokens, chain_thawing)) = chain else {
        return Value::Null;
    };
    let chain_active = chain_tokens - chain_thawing;
    let nest_active = nest_delegated - nest_thawing;
    let drift = chain_active - nest_active;
    let drift_pct = if chain_active > 0.0 {
        drift.abs() / chain_active
    } else if nest_active > 0.0 {
        1.0
    } else {
        0.0
    };
    json!({
        "verified": drift_pct <= EPSILON,
        "driftGRT": num(drift),
        "driftPct": num(drift_pct),
        "chain": { "tokens": num(chain_tokens), "thawing": num(chain_thawing), "active": num(chain_active) },
        "subgraph": { "tokens": num(nest_delegated), "thawing": num(nest_thawing), "active": num(nest_active) },
    })
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    #[test]
    fn the_two_sources_interleave_newest_first() {
        let out = provenance_events(
            &[(
                "delegation".into(),
                Some("0xAA".into()),
                Some(10.0),
                "2026-09-01T00:00:00Z".into(),
            )],
            &[(
                "reward_cut".into(),
                Some(100_000),
                200_000,
                "2026-09-05T00:00:00Z".into(),
            )],
        );
        let e = out.as_array().unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["kind"], json!("reward_cut"));
        assert_eq!(e[1]["kind"], json!("delegation"));
        assert_eq!(e[1]["delegator"], json!("0xaa"));
    }

    /// The ingest records a row per epoch the parameter was seen in, so an unchanged cut would
    /// otherwise appear on the timeline every epoch for ever.
    #[test]
    fn a_parameter_change_that_changed_nothing_is_not_an_event() {
        let out = provenance_events(
            &[],
            &[(
                "reward_cut".into(),
                Some(100_000),
                100_000,
                "2026-09-05T00:00:00Z".into(),
            )],
        );
        assert_eq!(out.as_array().unwrap().len(), 0);
    }

    #[test]
    fn an_unknown_kind_is_dropped_rather_than_guessed() {
        let out = provenance_events(
            &[("something_else".into(), None, None, "2026-09-01".into())],
            &[("cooldown".into(), None, 1, "2026-09-01".into())],
        );
        assert_eq!(out.as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_pool_within_half_a_percent_is_verified() {
        let r = pool_reconciliation(Some((1000.0, 100.0)), 900.0, 0.0);
        assert_eq!(r["verified"], json!(true));
        assert_eq!(r["chain"]["active"].as_f64().unwrap(), 900.0);
        assert_eq!(r["subgraph"]["active"].as_f64().unwrap(), 900.0);
        assert_eq!(r["driftGRT"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn a_real_divergence_is_not_verified() {
        let r = pool_reconciliation(Some((1000.0, 0.0)), 500.0, 0.0);
        assert_eq!(r["verified"], json!(false));
        assert_eq!(r["driftGRT"].as_f64().unwrap(), 500.0);
        assert_eq!(r["driftPct"].as_f64().unwrap(), 0.5);
    }

    /// A chain read that reverted is an absent reconciliation, not a drift of zero. Zero would
    /// read as agreement.
    #[test]
    fn no_chain_reading_is_null_rather_than_agreement() {
        assert_eq!(pool_reconciliation(None, 900.0, 0.0), Value::Null);
    }
}
