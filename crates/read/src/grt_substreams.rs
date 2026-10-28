//! Ethereum GRT transfers read twice, once from GraphOps Substreams and once from our
//! `supply-mainnet` nest, and compared transfer by transfer over the same block window.
//!
//! The nest follows the chain over RPC, Substreams over Firehose, so the two share no code path
//! between the node and the number. Agreement is exact or it is not agreement: same set of
//! (block, tx, log index), same from, to and value, same block hashes.

use std::collections::BTreeMap;

use serde::Serialize;

pub const GRT_MAINNET: &str = "0xc944e90c64b2c07662a292be6244bdf05cda44a7";
pub const TRANSFER: &str = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const ZERO: &str = "0x0000000000000000000000000000000000000000";
/// Differences listed per kind. The counts are always whole.
const SHOWN: usize = 10;

/// About a day of Ethereum blocks.
pub const WINDOW_BLOCKS: u64 = 7_200;
/// How far behind the nest's tip the window ends, so both sides are looking at final blocks.
pub const FINALITY_MARGIN: u64 = 128;

pub fn query() -> String {
    format!("evt_addr:{GRT_MAINNET} && evt_sig:0x{TRANSFER}")
}

pub fn nest_sql(from: u64, to: u64) -> String {
    format!(
        "SELECT block_number, block_hash, tx_hash, log_index, \"from\", \"to\", \
         CAST(CAST(value AS HUGEINT) AS VARCHAR) AS value \
         FROM graph_token__transfer WHERE block_number >= {from} AND block_number < {to} \
         ORDER BY block_number, log_index"
    )
}

/// `[from, to)` ending [`FINALITY_MARGIN`] behind `tip`.
pub fn window(tip: u64) -> (u64, u64) {
    let to = tip.saturating_sub(FINALITY_MARGIN);
    (to.saturating_sub(WINDOW_BLOCKS), to)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub block: u64,
    pub block_hash: String,
    pub tx_hash: String,
    pub log_index: u32,
    pub from: String,
    pub to: String,
    pub value: u128,
}

fn bare(s: &str) -> String {
    s.trim_start_matches("0x").to_ascii_lowercase()
}

fn address(s: &str) -> String {
    format!("0x{}", bare(s))
}

impl Transfer {
    /// From a raw log. `None` when it is not a well-formed ERC-20 Transfer.
    pub fn from_log(
        block: u64,
        block_hash: &str,
        tx_hash: &str,
        log_index: u32,
        topics: &[Vec<u8>],
        data: &[u8],
    ) -> Option<Self> {
        let [t0, t1, t2] = topics else { return None };
        if t0.len() != 32 || t1.len() != 32 || t2.len() != 32 || data.len() != 32 {
            return None;
        }
        if hex::encode(t0) != TRANSFER || data[..16].iter().any(|b| *b != 0) {
            return None;
        }
        Some(Self {
            block,
            block_hash: bare(block_hash),
            tx_hash: bare(tx_hash),
            log_index,
            from: format!("0x{}", hex::encode(&t1[12..])),
            to: format!("0x{}", hex::encode(&t2[12..])),
            value: u128::from_be_bytes(data[16..].try_into().ok()?),
        })
    }

    /// From a nest row of [`nest_sql`].
    pub fn from_row(row: &serde_json::Value) -> Option<Self> {
        let s = |k: &str| row.get(k).and_then(|v| v.as_str());
        let n = |k: &str| {
            row.get(k)
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        };
        Some(Self {
            block: n("block_number")?,
            block_hash: bare(s("block_hash")?),
            tx_hash: bare(s("tx_hash")?),
            log_index: u32::try_from(n("log_index")?).ok()?,
            from: address(s("from")?),
            to: address(s("to")?),
            value: s("value")?.parse().ok()?,
        })
    }

    fn key(&self) -> (u64, String, u32) {
        (self.block, self.tx_hash.clone(), self.log_index)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Side {
    pub transfers: usize,
    pub blocks: usize,
    /// GRT wei, as a string: it does not fit a JSON number.
    pub value_wei: String,
    pub value_grt: f64,
    pub mints: usize,
    pub burns: usize,
}

fn side(ts: &[Transfer]) -> Side {
    let value: u128 = ts.iter().map(|t| t.value).sum();
    let mut blocks: Vec<u64> = ts.iter().map(|t| t.block).collect();
    blocks.dedup();
    Side {
        transfers: ts.len(),
        blocks: blocks.len(),
        value_wei: value.to_string(),
        value_grt: value as f64 / 1e18,
        mints: ts.iter().filter(|t| t.from == ZERO).count(),
        burns: ts.iter().filter(|t| t.to == ZERO).count(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Agree,
    Disagree,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Difference {
    pub block: u64,
    pub tx_hash: String,
    pub log_index: u32,
    pub what: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub from_block: u64,
    pub to_block: u64,
    pub substreams: Side,
    pub nest: Side,
    pub verdict: Verdict,
    pub differences: usize,
    /// At most [`SHOWN`] of each kind.
    pub examples: Vec<Difference>,
}

pub fn reconcile(from_block: u64, to_block: u64, ss: &[Transfer], nest: &[Transfer]) -> Check {
    let in_window = |t: &&Transfer| t.block >= from_block && t.block < to_block;
    let ss: Vec<Transfer> = ss.iter().filter(in_window).cloned().collect();
    let nest: Vec<Transfer> = nest.iter().filter(in_window).cloned().collect();
    let a: BTreeMap<_, _> = ss.iter().map(|t| (t.key(), t)).collect();
    let b: BTreeMap<_, _> = nest.iter().map(|t| (t.key(), t)).collect();

    let mut kinds: BTreeMap<&'static str, Vec<Difference>> = BTreeMap::new();
    let mut note = |t: &Transfer, what: &'static str| {
        kinds.entry(what).or_default().push(Difference {
            block: t.block,
            tx_hash: t.tx_hash.clone(),
            log_index: t.log_index,
            what,
        });
    };
    for (k, t) in &a {
        match b.get(k) {
            None => note(t, "only in substreams"),
            Some(n) if n.block_hash != t.block_hash => note(t, "block hash differs"),
            Some(n) if (&n.from, &n.to, n.value) != (&t.from, &t.to, t.value) => {
                note(t, "transfer differs")
            }
            Some(_) => {}
        }
    }
    for (k, t) in &b {
        if !a.contains_key(k) {
            note(t, "only in nest");
        }
    }
    // Two rows with the same key on one side collapse into one map entry.
    let duplicates = (ss.len() - a.len()) + (nest.len() - b.len());
    let differences = kinds.values().map(Vec::len).sum::<usize>() + duplicates;
    Check {
        from_block,
        to_block,
        substreams: side(&ss),
        nest: side(&nest),
        verdict: if differences == 0 {
            Verdict::Agree
        } else {
            Verdict::Disagree
        },
        differences,
        examples: kinds
            .into_values()
            .flat_map(|v| v.into_iter().take(SHOWN))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(block: u64, log: u32, value: u128) -> Transfer {
        Transfer {
            block,
            block_hash: format!("h{block}"),
            tx_hash: format!("tx{block}"),
            log_index: log,
            from: "0x0000000000000000000000000000000000000001".into(),
            to: "0x0000000000000000000000000000000000000002".into(),
            value,
        }
    }

    #[test]
    fn identical_sides_agree() {
        let xs = vec![t(10, 0, 5), t(10, 1, 6), t(11, 0, 7)];
        let c = reconcile(10, 12, &xs, &xs);
        assert_eq!(c.verdict, Verdict::Agree);
        assert_eq!(c.substreams, c.nest);
        assert_eq!((c.substreams.transfers, c.substreams.blocks), (3, 2));
        assert_eq!(c.substreams.value_wei, "18");
    }

    #[test]
    fn a_missing_a_changed_and_a_reorged_transfer_are_each_named() {
        let ss = vec![t(10, 0, 5), t(11, 0, 7), t(12, 0, 9)];
        let changed = t(11, 0, 8);
        let mut reorged = t(12, 0, 9);
        reorged.block_hash = "other".into();
        let nest = vec![changed, reorged, t(13, 0, 1)];
        let c = reconcile(10, 14, &ss, &nest);
        assert_eq!(c.verdict, Verdict::Disagree);
        let whats: Vec<_> = c.examples.iter().map(|d| (d.block, d.what)).collect();
        assert!(whats.contains(&(10, "only in substreams")));
        assert!(whats.contains(&(11, "transfer differs")));
        assert!(whats.contains(&(12, "block hash differs")));
        assert!(whats.contains(&(13, "only in nest")));
        assert_eq!(c.differences, 4);
    }

    #[test]
    fn outside_the_window_is_ignored() {
        let c = reconcile(
            10,
            11,
            &[t(9, 0, 1), t(10, 0, 2)],
            &[t(10, 0, 2), t(11, 0, 3)],
        );
        assert_eq!(c.verdict, Verdict::Agree);
        assert_eq!(c.nest.transfers, 1);
    }

    #[test]
    fn a_duplicated_row_is_a_disagreement() {
        let c = reconcile(10, 11, &[t(10, 0, 2)], &[t(10, 0, 2), t(10, 0, 2)]);
        assert_eq!(c.verdict, Verdict::Disagree);
    }

    #[test]
    fn mints_and_burns_are_counted_by_the_zero_address() {
        let mut mint = t(10, 0, 1);
        mint.from = ZERO.into();
        let mut burn = t(10, 1, 1);
        burn.to = ZERO.into();
        let s = side(&[mint, burn, t(10, 2, 1)]);
        assert_eq!((s.mints, s.burns), (1, 1));
    }

    #[test]
    fn decodes_a_transfer_log_and_rejects_others() {
        let topic = |b: u8| {
            let mut v = vec![0u8; 32];
            v[31] = b;
            v
        };
        let mut data = vec![0u8; 32];
        data[31] = 42;
        let t0 = hex::decode(TRANSFER).unwrap();
        let x = Transfer::from_log(1, "0xAB", "CD", 3, &[t0.clone(), topic(1), topic(2)], &data)
            .unwrap();
        assert_eq!(
            (x.value, x.block_hash.as_str(), x.tx_hash.as_str()),
            (42, "ab", "cd")
        );
        assert_eq!(x.to, "0x0000000000000000000000000000000000000002");
        assert!(Transfer::from_log(1, "", "", 0, &[topic(9), topic(1), topic(2)], &data).is_none());
        assert!(Transfer::from_log(1, "", "", 0, &[t0, topic(1)], &data).is_none());
    }

    #[test]
    fn reads_a_nest_row() {
        let row = serde_json::json!({
            "block_number": 26282976, "block_hash": "0xAA", "tx_hash": "0xBB", "log_index": 123,
            "from": "0x9642B23ED1E01DF1092B92641051881A322F5D4E",
            "to": "0x347011395f8f5fd5b0f616076e110b0566d8e63b",
            "value": "3450000000000000000000"
        });
        let x = Transfer::from_row(&row).unwrap();
        assert_eq!(
            (x.block, x.log_index, x.value),
            (26282976, 123, 3_450_000_000_000_000_000_000)
        );
        assert_eq!(x.from, "0x9642b23ed1e01df1092b92641051881a322f5d4e");
        assert_eq!(x.tx_hash, "bb");
    }

    #[test]
    fn the_window_ends_behind_the_tip() {
        assert_eq!(window(100_000), (100_000 - 128 - 7_200, 100_000 - 128));
        assert_eq!(window(10), (0, 0));
    }
}
