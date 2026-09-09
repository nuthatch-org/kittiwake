//! Query-fee revenue: `EscrowCollected` on the nest, into `rav_redemptions`.
//!
//! The shape of this is entirely determined by two pieces of archaeology, and both are worth
//! keeping in front of whoever reads it next.
//!
//! **Where the allocation comes from.** `lodestar_escrow_transactions` keys rows by
//! `tx_hash-log_index` and carries no allocation, so this reads the two event tables directly.
//! Every `EscrowCollected`, and for the ones collected by the GraphTallyCollector, the
//! `QueryFeesCollected` from the same transaction with the same payer, service provider and amount
//! to the wei. The pairing is checked rather than trusted: a pair whose amounts differ fails the
//! run, because a wrong allocation on a revenue row is a plausible number rather than an error, and
//! a plausible wrong number is the kind that survives review.
//!
//! **Why the id is built the way it is.** See [`subgraph_escrow_tx_id`]. The upsert has to dedupe
//! against rows the gateway path already wrote, which means reproducing the subgraph's own id
//! encoding exactly rather than inventing a sensible one.
//!
//! Two known and measured differences from the subgraph, at a pinned block (nuthatch#1114):
//!
//! - Seventy-five collections get no allocation here where the gateway path wrote one. They are
//!   GraphTallyCollector collections for a data service other than SubgraphService, so no
//!   `QueryFeesCollected` exists; the subgraph takes their `collectionId` from the collector's own
//!   `PaymentCollected`, which this nest does not yet index. Every one already has a null
//!   `deployment_id` in Postgres, so nothing rendered changes.
//! - The nest is a strict superset: 70,408 of 70,408 subgraph rows match, and the nest holds
//!   **nine** more - one address collecting from its own escrow to a single receiver, which the
//!   subgraph drops because no escrow account keyed `(payer, collector, receiver)` was ever funded.
//!   Those nine are real collections and this keeps them. Anyone comparing revenue across the
//!   migration should expect nine extra rows and not read them as a regression.

use kittiwake_db::Pool;
use kittiwake_nest::{Lane, NestGate};
use kittiwake_types::NestId;

use crate::{IngestError, Ingested, i, page, s, state};

/// The GraphTallyCollector. An `EscrowCollected` whose collector is this contract is a query-fee
/// collection with a matching `QueryFeesCollected` in the same transaction, which is where the
/// allocation id lives. Any other collector has no such partner and no allocation.
const GRAPH_TALLY_COLLECTOR: &str = "0x8f69f5c07477ac46fbc491b1e6d91e2bb0111a9e";

/// Re-scan this much wall-clock on every delta run, so a late-arriving or reordered transaction
/// near the cursor boundary is not missed. The upsert makes the overlap harmless.
const OVERLAP_SECONDS: i64 = 3600;

/// Well under the nest's 50,000-row cap, so `truncated` is a defect rather than a page.
const PAGE: usize = 10_000;

/// The subgraph's `paymentsEscrowTransactions.id`, rebuilt from the nest's row.
///
/// The encoding is `txHash (32 bytes) || logIndex (uint32, little-endian)`, and the subgraph's
/// index is the nest's `log_index` **plus one**. That is not a guess: joining on that basis matched
/// 70,408 of 70,408 subgraph rows at a pinned block, and the two other plausible bases matched
/// zero (nuthatch#1114). Reproducing it exactly is what lets the upsert dedupe against rows the
/// gateway path already wrote, instead of writing every collection a second time under a new id.
pub fn subgraph_escrow_tx_id(tx_hash: &str, log_index: i64) -> String {
    let n = (log_index + 1) as u32;
    let mut out = tx_hash.to_lowercase();
    for b in n.to_le_bytes() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// One collection, as the nest reports it with its fee event joined in where one exists.
struct Collection {
    tx_hash: String,
    log_index: i64,
    payer: String,
    receiver: String,
    tokens: String,
    block_timestamp: i64,
    allocation_id: Option<String>,
    fee_tokens: Option<String>,
}

fn build_sql(since: i64, after: Option<&Collection>) -> String {
    let keyset = match after {
        None => String::new(),
        Some(a) => format!(
            " AND (c.block_timestamp > {ts} OR (c.block_timestamp = {ts} AND \
             (c.tx_hash > '{tx}' OR (c.tx_hash = '{tx}' AND c.log_index > {li}))))",
            ts = a.block_timestamp,
            tx = a.tx_hash,
            li = a.log_index,
        ),
    };
    // A collection and its fee event share the transaction, the payer, the service provider and the
    // amount to the wei, so the pair is joined on all four; the rank only separates two identical
    // collections in one transaction. That leaves no way to pair a self-collection, or a collection
    // for some other data service, with a SubgraphService fee that is not its own.
    format!(
        "WITH fees AS (\
         SELECT tx_hash, LOWER(payer) AS payer, LOWER(\"serviceProvider\") AS receiver, \
         CAST(\"tokensCollected\" AS VARCHAR) AS tokens, \"allocationId\" AS allocation_id, \
         CAST(\"tokensCollected\" AS VARCHAR) AS fee_tokens, \
         ROW_NUMBER() OVER (PARTITION BY tx_hash, LOWER(payer), LOWER(\"serviceProvider\"), \
         CAST(\"tokensCollected\" AS VARCHAR) ORDER BY log_index) AS rn \
         FROM subgraph_service__query_fees_collected\
         ), esc AS (\
         SELECT tx_hash, log_index, payer, receiver, tokens, block_timestamp, \
         ROW_NUMBER() OVER (PARTITION BY tx_hash, LOWER(payer), LOWER(receiver), \
         CAST(tokens AS VARCHAR) ORDER BY log_index) AS rn \
         FROM escrow__escrow_collected WHERE LOWER(collector) = '{GRAPH_TALLY_COLLECTOR}'\
         ) \
         SELECT c.tx_hash, c.log_index, c.payer, c.receiver, CAST(c.tokens AS VARCHAR) AS tokens, \
         c.block_timestamp, f.allocation_id, f.fee_tokens \
         FROM escrow__escrow_collected c \
         LEFT JOIN esc e ON e.tx_hash = c.tx_hash AND e.log_index = c.log_index \
         LEFT JOIN fees f ON f.tx_hash = e.tx_hash AND f.payer = LOWER(e.payer) \
         AND f.receiver = LOWER(e.receiver) AND f.tokens = CAST(e.tokens AS VARCHAR) AND f.rn = e.rn \
         WHERE c.block_timestamp >= {since}{keyset} \
         ORDER BY c.block_timestamp, c.tx_hash, c.log_index LIMIT {PAGE}"
    )
}

/// Ingest query-fee collections into `rav_redemptions`.
///
/// `backfill` restarts from zero rather than the cursor. It is resumable: the cursor advances as
/// pages land, so an interrupted backfill picks up where it stopped rather than from the beginning.
pub async fn rav(
    pool: &Pool,
    gate: &NestGate,
    lane: Lane,
    backfill: bool,
) -> Result<Ingested, IngestError> {
    let stored = state::get(pool, "rav").await?.last_block.unwrap_or(0) as i64;
    let since = if backfill {
        0
    } else {
        (stored - OVERLAP_SECONDS).max(0)
    };

    let mut total = 0u64;
    let mut max_timestamp = stored;
    let mut after: Option<Collection> = None;

    loop {
        let answer = page(
            gate,
            NestId::Alloc,
            build_sql(since, after.as_ref()),
            lane,
            "escrow collections",
        )
        .await?;
        if answer.rows.is_empty() {
            break;
        }

        let mut batch: Vec<Collection> = Vec::with_capacity(answer.rows.len());
        for r in &answer.rows {
            let Some(tx_hash) = s(r, "tx_hash") else {
                continue;
            };
            // The keyset for the next page is built from these values, so a tx hash that is not a
            // hash has no business being interpolated into a statement. Nothing upstream is
            // expected to produce one; that is exactly why an unchecked assumption here would
            // survive unnoticed.
            if !is_hex_word(&tx_hash) {
                return Err(IngestError::Refused(format!(
                    "the nest returned {tx_hash:?} as a transaction hash, which is not one"
                )));
            }
            batch.push(Collection {
                tx_hash,
                log_index: i(r, "log_index").unwrap_or(0),
                payer: s(r, "payer").unwrap_or_default().to_lowercase(),
                receiver: s(r, "receiver").unwrap_or_default().to_lowercase(),
                tokens: s(r, "tokens").unwrap_or_else(|| "0".into()),
                block_timestamp: i(r, "block_timestamp").unwrap_or(0),
                allocation_id: s(r, "allocation_id").map(|a| a.to_lowercase()),
                fee_tokens: s(r, "fee_tokens"),
            });
        }

        check_pairing(&batch)?;

        for c in &batch {
            max_timestamp = max_timestamp.max(c.block_timestamp);
        }
        total += upsert(pool, &batch).await? as u64;

        let short = answer.rows.len() < PAGE;
        after = batch.pop();
        if short || after.is_none() {
            break;
        }
    }

    if max_timestamp > stored {
        state::set(pool, "rav", None, Some(max_timestamp as i32), None).await?;
    } else {
        state::touch(pool, "rav").await?;
    }
    Ok(Ingested { rows: total })
}

fn is_hex_word(s: &str) -> bool {
    s.len() == 66 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A collection paired with a fee event of a different amount means the join found the wrong
/// partner, and the allocation it carries belongs to somebody else's revenue. Refuse the run.
fn check_pairing(rows: &[Collection]) -> Result<(), IngestError> {
    let mut bad = 0usize;
    let mut first: Option<&Collection> = None;
    for c in rows {
        if let Some(fee) = &c.fee_tokens
            && fee != &c.tokens
        {
            bad += 1;
            first.get_or_insert(c);
        }
    }
    match first {
        None => Ok(()),
        Some(c) => Err(IngestError::Refused(format!(
            "{bad} escrow collection(s) paired with a fee event of a different amount (first: \
             {}-{} escrow {} against fee {}); the allocation would be wrong, so refusing rather \
             than guessing",
            c.tx_hash,
            c.log_index,
            c.tokens,
            c.fee_tokens.as_deref().unwrap_or("?"),
        ))),
    }
}

/// Write a page.
///
/// The deployment is resolved by joining `allocations` inside the statement rather than by a
/// separate lookup: one round trip instead of two, and no window in which the two could disagree.
/// A collection with no allocation, or one naming an allocation we have not ingested, keeps a null
/// deployment - which is what the incumbent does and what the panels already render.
async fn upsert(pool: &Pool, rows: &[Collection]) -> Result<usize, IngestError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = rows
        .iter()
        .map(|c| subgraph_escrow_tx_id(&c.tx_hash, c.log_index))
        .collect();
    let receivers: Vec<String> = rows.iter().map(|c| c.receiver.clone()).collect();
    let payers: Vec<String> = rows.iter().map(|c| c.payer.clone()).collect();
    let allocs: Vec<Option<String>> = rows.iter().map(|c| c.allocation_id.clone()).collect();
    let tokens: Vec<String> = rows.iter().map(|c| c.tokens.clone()).collect();
    let times: Vec<i64> = rows.iter().map(|c| c.block_timestamp).collect();

    // Deliberately not `query!`, and the reason is the amounts.
    //
    // `tokens` is a wei figure carried as a decimal string, because a wei amount does not fit an
    // `f64` and two collections differing in the last few digits are exactly the pair somebody is
    // trying to tell apart. Binding it as text and letting Postgres cast to `numeric` keeps every
    // digit. The compile-time macro would want either the `bigdecimal` feature or a `float8`
    // parameter, and the second of those quietly rounds the money. `allocation_id` is nullable per
    // element as well, which `text[]` cannot express to the macro.
    //
    // So this statement stays checked at run time, which is the honest outcome: pretending to
    // check it would cost precision to buy a green tick. #2.
    sqlx::query(
        "INSERT INTO rav_redemptions \
         (id, indexer_address, payer, allocation_id, deployment_id, tokens_grt, source, \
          collected_at, block, chain_id) \
         SELECT t.id, t.receiver, t.payer, t.allocation_id, a.deployment_id, t.tokens / 1e18, \
         'graphtally', to_timestamp(t.ts), NULL, 42161 \
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::numeric[], $6::bigint[]) \
         AS t(id, receiver, payer, allocation_id, tokens, ts) \
         LEFT JOIN allocations a ON a.id = t.allocation_id \
         ON CONFLICT (id) DO UPDATE SET \
         tokens_grt = EXCLUDED.tokens_grt, deployment_id = EXCLUDED.deployment_id, \
         allocation_id = EXCLUDED.allocation_id, payer = EXCLUDED.payer, \
         collected_at = EXCLUDED.collected_at",
    )
    .bind(&ids)
    .bind(&receivers)
    .bind(&payers)
    .bind(&allocs)
    .bind(&tokens)
    .bind(&times)
    .execute(pool)
    .await?;

    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(tx: &str, tokens: &str, fee: Option<&str>) -> Collection {
        Collection {
            tx_hash: tx.into(),
            log_index: 1,
            payer: "0xaa".into(),
            receiver: "0xbb".into(),
            tokens: tokens.into(),
            block_timestamp: 100,
            allocation_id: None,
            fee_tokens: fee.map(|f| f.into()),
        }
    }

    /// The one figure this whole module hangs on. A change here silently doubles every revenue row
    /// in the table, because the new ids collide with nothing and every upsert becomes an insert.
    #[test]
    fn the_id_is_the_hash_followed_by_the_log_index_plus_one_little_endian() {
        let tx = "0xaabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        // log index 0 becomes 1: 01 00 00 00.
        assert_eq!(subgraph_escrow_tx_id(tx, 0), format!("{tx}01000000"));
        // 255 becomes 256, which is where a big-endian encoding would differ visibly.
        assert_eq!(subgraph_escrow_tx_id(tx, 255), format!("{tx}00010000"));
        assert_eq!(subgraph_escrow_tx_id(tx, 1), format!("{tx}02000000"));
    }

    #[test]
    fn the_hash_is_lowercased_because_the_subgraph_writes_it_that_way() {
        let upper = "0xAABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899";
        assert!(subgraph_escrow_tx_id(upper, 0).starts_with("0xaabbccdd"));
    }

    #[test]
    fn a_matching_pair_passes_and_an_unpaired_collection_passes() {
        assert!(check_pairing(&[c("0x1", "100", Some("100"))]).is_ok());
        assert!(check_pairing(&[c("0x1", "100", None)]).is_ok());
    }

    /// The failure that would otherwise write somebody else's allocation onto a revenue row.
    #[test]
    fn a_pair_whose_amounts_differ_fails_the_run() {
        let e = check_pairing(&[c("0x1", "100", Some("101"))]).unwrap_err();
        assert!(e.to_string().contains("different amount"), "{e}");
    }

    #[test]
    fn a_delta_run_rescans_an_hour_and_a_backfill_starts_at_zero() {
        let sql = build_sql(1_700_000_000 - OVERLAP_SECONDS, None);
        assert!(sql.contains("c.block_timestamp >= 1699996400"));
        assert!(build_sql(0, None).contains("c.block_timestamp >= 0"));
    }

    /// Paging must break ties, or two collections in the same block at the same timestamp put the
    /// loop into a cycle that never terminates.
    #[test]
    fn the_keyset_breaks_ties_on_the_hash_and_then_the_log_index() {
        let after = Collection {
            log_index: 7,
            ..c("0xdead", "1", None)
        };
        let sql = build_sql(0, Some(&after));
        assert!(sql.contains("c.block_timestamp > 100"));
        assert!(sql.contains("c.tx_hash > '0xdead'"));
        assert!(sql.contains("c.log_index > 7"));
    }

    #[test]
    fn only_graphtally_collections_can_pick_up_an_allocation() {
        let sql = build_sql(0, None);
        assert!(sql.contains(&format!("LOWER(collector) = '{GRAPH_TALLY_COLLECTOR}'")));
    }

    #[test]
    fn a_transaction_hash_is_sixty_six_characters_of_hex() {
        let tx = "0xaabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        assert!(is_hex_word(tx));
        assert!(!is_hex_word("0xdead"));
        assert!(!is_hex_word(&format!("{}' OR 1=1 --", &tx[..54])));
        assert!(!is_hex_word(&tx.replace("aa", "zz")));
    }
}
