//! Jobs whose output is read straight back out of the process, with no database in between.
//!
//! The service this replaces ran these as stateless invocations, so their results had to travel
//! through Redis to reach the route that served them. One long-lived process needs no courier: the
//! job writes into the same store the route reads. That removes a network round trip from every
//! request and a whole class of "the cron wrote it but the route cannot see it" failure.

use futures::StreamExt;
use kittiwake_cache::{Entry, Store};
use kittiwake_nest::{Lane, NestGate, QuerySpec};
use kittiwake_read::{shape, sql};
use kittiwake_types::NestId;

use crate::{IngestError, Ingested};

/// Refresh the Horizon activity feed.
pub async fn horizon_activity(
    store: &Store,
    gate: &NestGate,
    lane: Lane,
) -> Result<Ingested, IngestError> {
    let mut delegations = QuerySpec::cron(
        NestId::Alloc,
        sql::delegation_events_sql(None, 20, 0)
            .ok_or_else(|| IngestError::Refused("delegation query refused its own input".into()))?,
    );
    let mut provisions = QuerySpec::cron(NestId::Alloc, sql::newest_provisions_sql(10));
    delegations.lane = lane;
    provisions.lane = lane;

    let (d, p) = tokio::join!(gate.query(&delegations), gate.query(&provisions));
    let d = d?;
    let p = p?;

    let feed = shape::horizon_activity(&d.rows, &p.rows);
    let count = feed.as_array().map(|a| a.len()).unwrap_or(0);
    store
        .insert(
            shape::HORIZON_ACTIVITY_KEY.to_string(),
            Entry::new(feed, d.provenance.clone()),
        )
        .await;

    Ok(Ingested { rows: count as u64 })
}

// ---------------------------------------------------------------------------------------------
// Chain health
// ---------------------------------------------------------------------------------------------

/// How many indexers we ask at once. Politeness towards other people's nodes, not throughput.
const PROBE_CONCURRENCY: usize = 10;

/// Indexers sampled per run, in score order.
const SAMPLE_SIZE: i64 = 50;

/// Above this, `blocksBehind` is stale or cross-chain noise rather than a lag measurement, and
/// averaging it in would swamp the median with a single misconfigured node.
const BEHIND_NOISE_CAP: i64 = 10_000;

/// Where the chain-health job keeps what it learned on previous runs.
///
/// The history *is* the signal - a chain that has been dead for three days must still read as dead
/// after a restart - so Postgres is the right home for it and [`History::Db`] is what runs in
/// production. [`History::Memory`] exists because the alternative, when no database is configured,
/// is not running the job at all, and a chain-lag panel that shows nothing is worse than one whose
/// stall clock restarts on deploy. The cost of the fallback is bounded and stated: after a restart
/// a halted chain reads live for ninety minutes and stalled for six hours before it reads halted
/// again. Nothing reads as healthier than it is for longer than that.
pub enum History {
    Db(kittiwake_db::Pool),
    Memory(MemoryHistory),
}

#[derive(Default)]
pub struct MemoryHistory {
    heads: tokio::sync::Mutex<std::collections::BTreeMap<String, crate::liveness::ChainHeadRecord>>,
    chains: tokio::sync::Mutex<std::collections::BTreeMap<String, Vec<String>>>,
}

impl History {
    async fn heads(
        &self,
    ) -> Result<std::collections::BTreeMap<String, crate::liveness::ChainHeadRecord>, IngestError>
    {
        match self {
            History::Db(pool) => Ok(kittiwake_db::chain_head_records(pool)
                .await
                .map_err(|e| IngestError::Refused(format!("head history unreadable: {e}")))?
                .into_iter()
                .map(|(n, head, first, last)| {
                    (
                        n,
                        crate::liveness::ChainHeadRecord {
                            head,
                            head_first_seen_at: first,
                            last_checked_at: last,
                        },
                    )
                })
                .collect()),
            History::Memory(m) => Ok(m.heads.lock().await.clone()),
        }
    }

    async fn put_heads(
        &self,
        rows: &[kittiwake_db::ChainHeadRow],
        records: &std::collections::BTreeMap<String, crate::liveness::ChainHeadRecord>,
    ) -> Result<(), IngestError> {
        match self {
            History::Db(pool) => kittiwake_db::upsert_chain_head_records(pool, rows)
                .await
                .map(|_| ())
                .map_err(|e| IngestError::Refused(format!("head history unwritable: {e}"))),
            History::Memory(m) => {
                *m.heads.lock().await = records.clone();
                Ok(())
            }
        }
    }

    async fn chains(&self) -> Result<std::collections::BTreeMap<String, Vec<String>>, IngestError> {
        match self {
            History::Db(pool) => Ok(kittiwake_db::indexer_chains(pool)
                .await
                .map_err(|e| IngestError::Refused(format!("chain snapshots unreadable: {e}")))?
                .into_iter()
                .collect()),
            History::Memory(m) => Ok(m.chains.lock().await.clone()),
        }
    }

    async fn put_chains(
        &self,
        rows: &[(String, Vec<String>)],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), IngestError> {
        match self {
            History::Db(pool) => kittiwake_db::upsert_indexer_chains(pool, rows, now)
                .await
                .map(|_| ())
                .map_err(|e| IngestError::Refused(format!("chain snapshots unwritable: {e}"))),
            History::Memory(m) => {
                let mut held = m.chains.lock().await;
                for (addr, chains) in rows {
                    held.insert(addr.clone(), chains.clone());
                }
                Ok(())
            }
        }
    }

    /// The directory of indexers to probe.
    ///
    /// From Postgres when there is one, because that is the scored order the incumbent samples in.
    /// Otherwise straight from the nest, unscored, which gives the same set of indexers in a
    /// different order - and since the sample is capped at fifty, a different order is a different
    /// sample. Worth knowing when comparing the two systems' numbers.
    async fn directory(
        &self,
        gate: &NestGate,
        lane: Lane,
    ) -> Result<Vec<(String, String)>, IngestError> {
        match self {
            History::Db(pool) => Ok(kittiwake_db::enriched_indexers(pool, SAMPLE_SIZE)
                .await
                .map_err(|e| IngestError::Refused(format!("directory unreadable: {e}")))?
                .into_iter()
                .filter_map(|ix| Some((ix.address.to_lowercase(), ix.url?)))
                .collect()),
            History::Memory(_) => {
                let mut spec = QuerySpec::cron(NestId::Alloc, sql::INDEXERS_ALL_SQL.to_string());
                spec.lane = lane;
                let answer = gate.query(&spec).await?;
                // The nest returns them ordered by address, which as a sample of fifty out of
                // several hundred would be arbitrary. Stake is the closest ordering available
                // without the score, and the scored query tiebreaks on it anyway.
                let mut by_stake: Vec<(u128, String, String)> = answer
                    .rows
                    .iter()
                    .filter_map(|r| {
                        let addr = r.get("id")?.as_str()?.to_lowercase();
                        let url = r.get("url")?.as_str()?.to_string();
                        let staked = r
                            .get("staked_tokens")
                            .and_then(|v| v.as_str())
                            .and_then(|v| v.parse::<u128>().ok())
                            .unwrap_or(0);
                        Some((staked, addr, url))
                    })
                    .collect();
                by_stake.sort_unstable_by_key(|(staked, _, _)| std::cmp::Reverse(*staked));
                Ok(by_stake
                    .into_iter()
                    .take(SAMPLE_SIZE as usize)
                    .map(|(_, addr, url)| (addr, url))
                    .collect())
            }
        }
    }
}

/// Probe the sampled indexers, fold what they say into the chain head history, and write the lag
/// and dropped-chain answers the two routes read.
pub async fn chain_health(
    store: &Store,
    history: &History,
    gate: &NestGate,
    lane: Lane,
    ext: &kittiwake_ext::Ext,
    now_ms: i64,
) -> Result<Ingested, IngestError> {
    use crate::liveness::{ChainLiveness, advance_chain_head, classify_liveness};
    use std::collections::{BTreeMap, BTreeSet};

    // The literal check only skips the obvious cases cheaply; `probe_indexer` resolves and checks
    // again before it sends anything, which is the check that actually decides.
    let candidates: Vec<(String, String)> = history
        .directory(gate, lane)
        .await?
        .into_iter()
        .filter(|(_, url)| kittiwake_ssrf::check_literal(url).is_ok())
        .collect();

    let probed = futures::stream::iter(
        candidates
            .into_iter()
            .map(|(addr, url)| async move { (addr, ext.probe_indexer(&url).await) }),
    )
    .buffer_unordered(PROBE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut chain_sampled: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut chain_behind: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut observed_head: BTreeMap<String, i64> = BTreeMap::new();
    let mut current_chains: Vec<(String, Vec<String>)> = Vec::new();

    for (addr, probe) in &probed {
        // An unreachable node tells us nothing. Recording an empty chain set for it would read as
        // "dropped every chain" on the next run, which is the loudest possible way to be wrong.
        if !probe.reachable || probe.statuses.is_empty() {
            continue;
        }

        let mut mine: BTreeSet<String> = BTreeSet::new();
        for st in &probe.statuses {
            let Some(network) = st
                .network
                .as_deref()
                .filter(|n| !n.is_empty() && *n != "unknown")
            else {
                continue;
            };
            mine.insert(network.to_string());

            // The chain tip belongs to the node, not to any one deployment, so a FAILED subgraph
            // still supplies a usable head observation. Collect it before the failed skip below.
            // Take the higher of the two: a node whose chain tracking has stalled can report
            // chainHeadBlock below a latestBlock it has already indexed.
            let tip = st
                .chain_head_block
                .unwrap_or(0)
                .max(st.latest_block.unwrap_or(0));
            if tip > 0 {
                let slot = observed_head.entry(network.to_string()).or_insert(0);
                *slot = (*slot).max(tip);
            }

            if st.health == "failed" {
                continue;
            }
            chain_sampled
                .entry(network.to_string())
                .or_default()
                .insert(addr.clone());
            if !st.synced
                && let Some(behind) = st.blocks_behind.filter(|b| *b <= BEHIND_NOISE_CAP)
            {
                chain_behind
                    .entry(network.to_string())
                    .or_default()
                    .push(behind);
            }
        }
        current_chains.push((addr.clone(), mine.into_iter().collect()));
    }

    // Dropped chains, against what each indexer was last seen serving.
    let previous = history.chains().await?;
    let dropped = dropped_since(&previous, &current_chains);
    let now = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_else(chrono::Utc::now);
    history.put_chains(&current_chains, now).await?;

    // Head history: fold this run's tips in, then classify.
    let prior = history.heads().await?;

    let mut records = prior.clone();
    for (network, tip) in &observed_head {
        records.insert(
            network.clone(),
            advance_chain_head(prior.get(network).copied(), *tip, now_ms),
        );
    }
    let to_write: Vec<kittiwake_db::ChainHeadRow> = observed_head
        .keys()
        .filter_map(|n| {
            records
                .get(n)
                .map(|r| (n.clone(), r.head, r.head_first_seen_at, r.last_checked_at))
        })
        .collect();
    history.put_heads(&to_write, &records).await?;

    let mut chains = serde_json::Map::new();
    let mut not_live: Vec<&str> = Vec::new();
    for (network, sampled) in &chain_sampled {
        let behinds = chain_behind.get(network).cloned().unwrap_or_default();
        let verdict = classify_liveness(records.get(network).copied(), now_ms);
        if matches!(
            verdict.liveness,
            ChainLiveness::Stalled | ChainLiveness::Halted
        ) {
            not_live.push(network);
        }
        chains.insert(
            network.clone(),
            serde_json::json!({
                "medianBlocksBehind": median(&behinds),
                "sampledIndexers": sampled.len(),
                "laggingCount": behinds.len(),
                "liveness": verdict.liveness,
                "observedHead": verdict.head,
                "headStalledForMs": verdict.stalled_for_ms,
            }),
        );
    }

    if !not_live.is_empty() {
        tracing::warn!(chains = ?not_live, "chains whose head is not advancing");
    }

    let count = chains.len() as u64;
    let provenance = kittiwake_types::Provenance {
        source: Some("indexer status endpoints".into()),
        ..Default::default()
    };
    store
        .insert(
            shape::CHAIN_LAG_KEY.to_string(),
            Entry::new(
                serde_json::json!({ "chains": chains, "computedAt": now_ms }),
                provenance.clone(),
            ),
        )
        .await;
    store
        .insert(
            shape::DROPPED_CHAINS_KEY.to_string(),
            Entry::new(
                serde_json::to_value(&dropped).unwrap_or_default(),
                provenance,
            ),
        )
        .await;

    Ok(Ingested { rows: count })
}

/// Which chains each indexer has stopped serving since we last looked.
///
/// Only indexers present in `current` are considered, and `current` holds only the ones that
/// answered this run. An indexer we could not reach is absent from it, and so reports nothing -
/// rather than reporting that it has dropped every chain it ever served, which is the loudest
/// possible way to be wrong about a node that is merely offline for a minute.
fn dropped_since(
    previous: &std::collections::BTreeMap<String, Vec<String>>,
    current: &[(String, Vec<String>)],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut dropped = std::collections::BTreeMap::new();
    for (addr, chains) in current {
        let Some(before) = previous.get(addr).filter(|b| !b.is_empty()) else {
            continue;
        };
        let gone: Vec<String> = before
            .iter()
            .filter(|c| !chains.contains(c))
            .cloned()
            .collect();
        if !gone.is_empty() {
            dropped.insert(addr.clone(), gone);
        }
    }
    dropped
}

/// The median, as a JSON number. Even-length lists average the two middle values, which is what
/// the incumbent does and what keeps a two-sample chain from reporting the worse of the pair.
fn median(values: &[i64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::json!(0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        let avg = (sorted[mid - 1] + sorted[mid]) as f64 / 2.0;
        serde_json::json!(avg)
    } else {
        serde_json::json!(sorted[mid])
    }
}

// ---------------------------------------------------------------------------------------------
// The IPFS cache behind search
// ---------------------------------------------------------------------------------------------

/// How many documents one run may fetch. The incumbent's figure. It keeps a run inside its slot;
/// the remainder is picked up on the next one, and `remaining` says whether that is keeping up.
const WARM_BUDGET: usize = 150;

/// Fill the IPFS cache that name and manifest search read.
///
/// Not a live job in the sense the others are: it writes to `ipfs_metadata`, which the platform's
/// own warm cron also writes. Both write the same document under the same CID, though, and the
/// upsert is idempotent, so two writers here race to the same answer rather than to different ones.
/// That is why it is not behind `scheduler_enabled` with the rest.
pub async fn warm_ipfs(
    pool: &kittiwake_db::Pool,
    ipfs: &kittiwake_ext::Ipfs,
    gate: &NestGate,
    lane: Lane,
) -> Result<Ingested, IngestError> {
    let warmed = kittiwake_read::search::warm(pool, ipfs, gate, lane, WARM_BUDGET)
        .await
        .map_err(|e| IngestError::Refused(e.to_string()))?;
    if warmed.remaining > 0 {
        tracing::info!(
            fetched = warmed.fetched,
            remaining = warmed.remaining,
            "ipfs cache warmed; more to do"
        );
    }
    Ok(Ingested {
        rows: warmed.fetched as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::{History, dropped_since, median};
    use std::collections::BTreeMap;

    fn prev(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(a, cs)| {
                (
                    a.to_string(),
                    cs.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    fn now(pairs: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        prev(pairs).into_iter().collect()
    }

    #[test]
    fn a_chain_that_is_gone_from_an_answering_node_is_reported() {
        let d = dropped_since(
            &prev(&[("0xaa", &["mainnet", "arbitrum-one"])]),
            &now(&[("0xaa", &["mainnet"])]),
        );
        assert_eq!(d.get("0xaa").unwrap(), &vec!["arbitrum-one".to_string()]);
    }

    /// The failure worth guarding: an unreachable node is absent from `current`, and absence must
    /// mean "we did not look", never "it dropped everything".
    #[test]
    fn an_indexer_that_did_not_answer_reports_nothing() {
        let d = dropped_since(&prev(&[("0xaa", &["mainnet", "celo"])]), &now(&[]));
        assert!(d.is_empty());
    }

    #[test]
    fn a_first_sighting_has_dropped_nothing() {
        let d = dropped_since(&prev(&[]), &now(&[("0xaa", &["mainnet"])]));
        assert!(d.is_empty());
    }

    #[test]
    fn an_added_chain_is_not_a_drop() {
        let d = dropped_since(
            &prev(&[("0xaa", &["mainnet"])]),
            &now(&[("0xaa", &["mainnet", "base"])]),
        );
        assert!(d.is_empty());
    }

    /// The fallback has to actually remember between runs, or the stall clock never accumulates
    /// and every chain reads live for ever.
    #[tokio::test]
    async fn the_memory_history_remembers_across_runs() {
        let h = History::Memory(Default::default());
        let rec = crate::liveness::ChainHeadRecord {
            head: 100,
            head_first_seen_at: 1,
            last_checked_at: 2,
        };
        let mut records = BTreeMap::new();
        records.insert("mainnet".to_string(), rec);
        h.put_heads(&[("mainnet".into(), 100, 1, 2)], &records)
            .await
            .unwrap();
        assert_eq!(h.heads().await.unwrap().get("mainnet"), Some(&rec));

        h.put_chains(
            &[("0xaa".to_string(), vec!["mainnet".to_string()])],
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(
            h.chains().await.unwrap().get("0xaa"),
            Some(&vec!["mainnet".to_string()])
        );
    }

    #[test]
    fn the_median_of_an_even_list_is_the_average_of_the_middle_pair() {
        assert_eq!(median(&[10, 20, 30, 40]), serde_json::json!(25.0));
        assert_eq!(median(&[1, 2]), serde_json::json!(1.5));
    }

    #[test]
    fn the_median_of_an_odd_list_is_the_middle_value() {
        assert_eq!(median(&[30, 10, 20]), serde_json::json!(20));
    }

    /// No lagging indexer means no lag, not an absent figure: the chart plots zero.
    #[test]
    fn an_empty_list_is_zero() {
        assert_eq!(median(&[]), serde_json::json!(0));
    }
}
