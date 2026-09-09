//! Subgraph search, and the IPFS metadata it is built on.
//!
//! Three different questions wear one search box. A `0x…` address searches deployment manifests, a
//! `Qm…` prefix searches deployment hashes, and anything else searches display names. They resolve
//! to the same shape, and only the first two touch the chain's own data - a name lives in an IPFS
//! document and nowhere else.
//!
//! That is the constraint the whole module is arranged around. Names cannot be searched at the
//! source, because the source is IPFS and IPFS has no index; you can fetch a document by its hash
//! and that is all. So names are searched in the Postgres cache of documents already fetched, and
//! [`warm`] exists to keep that cache ahead of the questions. A subgraph published five minutes ago
//! is not findable by name until the warm job has been round, which is a real limitation and is
//! stated here rather than discovered.

use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt;
use kittiwake_db::Pool;
use kittiwake_ext::Ipfs;
use kittiwake_nest::{Lane, NestGate, QuerySpec};
use kittiwake_types::{ApiError, NestId};

use crate::shape::{deployment_ipfs_hash, ipfs_hash_to_bytes32};

/// How many IPFS fetches run at once. IPFS gateways are slow and rate-limited, and the point of
/// the cache is that this number rarely matters.
const FETCH_CONCURRENCY: usize = 10;

/// A bytes32 metadata hash to its CIDv0, or `None` for the zero hash a subgraph without metadata
/// carries. The zero hash is a real, common value meaning "no document", not a missing field, and
/// converting it would send us fetching a CID that cannot exist.
pub fn metadata_cid(hash: Option<&str>) -> Option<String> {
    let h = hash?.to_lowercase();
    let h = h.strip_prefix("0x").unwrap_or(&h);
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) || h.bytes().all(|b| b == b'0') {
        return None;
    }
    deployment_ipfs_hash(h)
}

/// A deployment id as it may be interpolated into a nest statement.
fn is_id(s: &str) -> bool {
    let h = s.strip_prefix("0x").unwrap_or(s);
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

fn quoted(ids: &[String]) -> String {
    ids.iter()
        .map(|i| format!("'{}'", i.to_lowercase()))
        .collect::<Vec<_>>()
        .join(", ")
}

async fn ask(
    gate: &NestGate,
    nest: NestId,
    sql: String,
    lane: Lane,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let mut spec = QuerySpec::cron(nest, sql);
    spec.lane = lane;
    Ok(gate.query(&spec).await?.rows)
}

/// How many ids may go into one `IN (...)` list.
///
/// The nest takes its SQL as a URL query parameter and refuses a request line over 16 KB with a
/// bare `400`. A bytes32 id costs 69 characters quoted, so 433 of them - which is what a search for
/// "uniswap" produces - builds a 34 KB URL and fails. At 100 the URL is about 8 KB, half the limit,
/// with room for the rest of the statement.
///
/// The measurement, so the next person changing this does not have to repeat it: 200 ids and 15.7 KB
/// is accepted, 433 and 33.9 KB is refused.
const IN_LIST_CHUNK: usize = 100;

/// Run a query once per chunk of ids and concatenate the rows.
///
/// Chunks go out concurrently but bounded, because they all draw on one nest and the point of the
/// gate is that we never ask it for more than it admits. The width comes from the gate rather than
/// from a constant: this was `buffer_unordered(4)` against a `gns` nest holding two permits, which
/// does not run four at a time, it queues two of them by construction and occasionally times out
/// waiting for a slot that was never going to exist. See #43.
async fn ask_chunked<F>(
    gate: &NestGate,
    nest: NestId,
    lane: Lane,
    ids: &[String],
    build: F,
) -> Result<Vec<serde_json::Value>, ApiError>
where
    F: Fn(&[String]) -> String,
{
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sqls: Vec<String> = ids.chunks(IN_LIST_CHUNK).map(&build).collect();
    let width = gate.lane_width(nest, lane);
    let results: Vec<Result<Vec<serde_json::Value>, ApiError>> =
        futures::stream::iter(sqls.into_iter().map(|sql| ask(gate, nest, sql, lane)))
            .buffer_unordered(width)
            .collect()
            .await;
    let mut rows = Vec::new();
    for r in results {
        rows.extend(r?);
    }
    Ok(rows)
}

/// The JSON documents behind these CIDs: from the cache where it has them, from IPFS where it does
/// not, and into the cache either way.
async fn json_docs(
    pool: &Pool,
    ipfs: &Ipfs,
    cids: &[String],
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    if cids.is_empty() {
        return out;
    }
    let now = chrono::Utc::now();
    let cached = kittiwake_ext::cached_docs(pool, cids)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(err = %e, "ipfs cache read failed; falling through to IPFS");
            Vec::new()
        });
    let mut settled = BTreeSet::new();
    for doc in &cached {
        if let Some(j) = &doc.json {
            out.insert(doc.cid.clone(), j.clone());
            settled.insert(doc.cid.clone());
        } else if doc.failed_recently(now) {
            // Known bad and recently tried. Skipping it is the point of recording the failure.
            settled.insert(doc.cid.clone());
        }
    }

    // The CIDs are cloned rather than borrowed: a stream of futures over borrowed items is not
    // `Send` for every lifetime, and axum needs the whole handler future to be.
    let missing: Vec<String> = cids
        .iter()
        .filter(|c| !settled.contains(*c))
        .cloned()
        .collect();
    let fetched: Vec<(String, Option<serde_json::Value>)> =
        futures::stream::iter(missing.into_iter().map(|cid| async move {
            let json = kittiwake_ext::fetch_json(pool, ipfs, &cid).await;
            (cid, json)
        }))
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect()
        .await;
    for (cid, json) in fetched {
        if let Some(j) = json {
            out.insert(cid, j);
        }
    }
    out
}

/// What the GNS nest knows about the subgraph that most recently published a deployment.
#[derive(Debug, Clone)]
pub struct DeploymentMeta {
    pub subgraph_id: String,
    pub metadata: Option<serde_json::Value>,
    pub version: Option<serde_json::Value>,
}

impl DeploymentMeta {
    pub fn display_name(&self) -> Option<&str> {
        self.metadata.as_ref()?.get("displayName")?.as_str()
    }
    pub fn description(&self) -> Option<&str> {
        self.metadata.as_ref()?.get("description")?.as_str()
    }
}

/// For each deployment, the subgraph that most recently published it and that subgraph's metadata.
///
/// "Most recently" is `ROW_NUMBER() ... ORDER BY created_at DESC`, not "the current version": a
/// deployment can be published by several subgraphs, and the newest publisher is the one whose name
/// a reader would recognise.
pub async fn subgraph_metadata_for_deployments(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    deployment_ids: &[String],
) -> Result<BTreeMap<String, DeploymentMeta>, ApiError> {
    let ids: Vec<String> = deployment_ids
        .iter()
        .map(|d| d.to_lowercase())
        .filter(|d| is_id(d))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let rows = ask_chunked(gate, NestId::Gns, lane, &ids, |chunk| {
        format!(
            "SELECT deployment_id, subgraph_id, subgraph_metadata, version_metadata FROM (\
             SELECT *, ROW_NUMBER() OVER (PARTITION BY LOWER(deployment_id) ORDER BY created_at \
             DESC) AS rn FROM deployment_subgraphs WHERE LOWER(deployment_id) IN ({})) WHERE rn = 1",
            quoted(chunk)
        )
    })
    .await?;

    // Every CID both columns want, fetched once. Two subgraphs sharing a metadata document - which
    // happens, forks keep their parent's - would otherwise be two fetches of the same bytes.
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for r in &rows {
        for k in ["subgraph_metadata", "version_metadata"] {
            if let Some(c) = metadata_cid(r.get(k).and_then(|v| v.as_str())) {
                wanted.insert(c);
            }
        }
    }
    let docs = json_docs(pool, ipfs, &wanted.into_iter().collect::<Vec<_>>()).await;

    let mut out = BTreeMap::new();
    for r in &rows {
        let Some(deployment_id) = r.get("deployment_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let pick = |k: &str| {
            metadata_cid(r.get(k).and_then(|v| v.as_str())).and_then(|c| docs.get(&c).cloned())
        };
        out.insert(
            deployment_id.to_lowercase(),
            DeploymentMeta {
                subgraph_id: r
                    .get("subgraph_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                metadata: pick("subgraph_metadata"),
                version: pick("version_metadata"),
            },
        );
    }
    Ok(out)
}

/// Search hits for a set of deployment ids: each one's newest subgraph and name, most signal first.
async fn hits_for_deployments(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    ids: &[String],
    limit: usize,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let ids: Vec<String> = ids
        .iter()
        .map(|i| i.to_lowercase())
        .filter(|i| is_id(i))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // Rank on the figures alone, then hydrate only what survives the truncation.
    //
    // These two used to run together under a `join!`, which read as the obvious thing and was the
    // whole of #43's seven seconds. The ordering key is `signalled_tokens` and nothing else; the
    // subgraph metadata decides only what a surviving row is *called*. So a search for "uniswap",
    // which matches 434 deployments and returns ten, was asking the GNS nest about all 434 and then
    // fetching up to 868 IPFS documents to name rows it was about to discard. The cost scaled with
    // the candidate count instead of the result count, which is why "graphprotocol" answered in
    // 0.17 s and "uniswap" took seven.
    //
    // Sequencing them costs one round trip against the alloc nest. Hydrating ten rows instead of
    // 434 saves rather more than that.
    let figures = ask_chunked(gate, NestId::Alloc, lane, &ids, |chunk| {
        format!(
            "SELECT id, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
             CAST(staked_tokens AS VARCHAR) AS staked_tokens FROM lodestar_deployments \
             WHERE id IN ({})",
            quoted(chunk)
        )
    })
    .await?;

    let mut by_id: BTreeMap<String, (String, String)> = BTreeMap::new();
    for f in &figures {
        let Some(id) = f.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        by_id.insert(
            id.to_lowercase(),
            (
                f.get("signalled_tokens")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0")
                    .to_string(),
                f.get("staked_tokens")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0")
                    .to_string(),
            ),
        );
    }

    // Signal is wei and does not fit an f64. Sorting on a parsed float would order two subgraphs
    // whose signal differs in the last few digits arbitrarily, which is exactly the pair a user is
    // trying to tell apart.
    let mut ordered = ids;
    ordered.sort_by_key(|id| {
        std::cmp::Reverse(
            by_id
                .get(id)
                .and_then(|(s, _)| s.parse::<u128>().ok())
                .unwrap_or(0),
        )
    });
    ordered.truncate(limit);

    // Only now, and only for the rows a reader will actually see.
    let meta = subgraph_metadata_for_deployments(pool, ipfs, gate, lane, &ordered).await?;

    Ok(ordered
        .into_iter()
        .map(|id| {
            let (signalled, staked) = by_id
                .get(&id)
                .cloned()
                .unwrap_or_else(|| ("0".into(), "0".into()));
            let m = meta.get(&id);
            let metadata = match m.and_then(|m| m.display_name()) {
                Some(name) => serde_json::json!({
                    "displayName": name,
                    "description": m.and_then(|m| m.description()),
                }),
                None => serde_json::Value::Null,
            };
            serde_json::json!({
                // The hit is identified by its subgraph where one published it, because that is what
                // a link in the app addresses; the deployment id is the fallback, never a guess.
                "id": m.map(|m| m.subgraph_id.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
                "metadata": metadata,
                "currentVersion": {
                    "subgraphDeployment": {
                        "ipfsHash": crate::shape::ipfs_or_id(&id),
                        "signalledTokens": signalled,
                        "stakedTokens": staked,
                    }
                },
            })
        })
        .collect())
}

/// Deployments whose CIDv0 starts with `prefix`.
///
/// Base58 has no prefix relation to the underlying bytes, so this cannot be a range scan: every
/// deployment carrying signal or stake is converted and filtered. That is a few thousand
/// conversions, which is cheap, and it is why the id list is worth caching upstream.
pub async fn by_hash_prefix(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    prefix: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let rows = ask(
        gate,
        NestId::Alloc,
        "SELECT id FROM lodestar_deployments WHERE signalled_tokens > 0 OR staked_tokens > 0"
            .into(),
        lane,
    )
    .await?;
    let matching: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get("id")?.as_str())
        .filter(|id| deployment_ipfs_hash(id).is_some_and(|h| h.starts_with(prefix)))
        .map(|id| id.to_lowercase())
        .collect();
    hits_for_deployments(pool, ipfs, gate, lane, &matching, limit).await
}

/// Subgraphs whose cached display name contains `q`.
///
/// Deprecated subgraphs are skipped: they still resolve and would otherwise crowd out the live one
/// under the same name.
pub async fn by_name(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    q: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let cids: Vec<(String,)> = sqlx::query_as(
        "SELECT cid FROM ipfs_metadata WHERE json->>'displayName' ILIKE $1 LIMIT 500",
    )
    .bind(format!("%{q}%"))
    .fetch_all(pool)
    .await
    .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    let hashes: Vec<String> = cids
        .iter()
        .filter_map(|(c,)| ipfs_hash_to_bytes32(c))
        .collect();
    if hashes.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<String> = ask_chunked(gate, NestId::Gns, lane, &hashes, |chunk| {
        format!(
            "SELECT current_deployment_id FROM subgraph_current WHERE NOT deprecated \
             AND LOWER(subgraph_metadata) IN ({})",
            quoted(chunk)
        )
    })
    .await?
    .iter()
    .filter_map(|r| Some(r.get("current_deployment_id")?.as_str()?.to_lowercase()))
    .collect();
    hits_for_deployments(pool, ipfs, gate, lane, &ids, limit).await
}

/// Deployments whose cached manifest mentions `address` - what `manifest_contains_nocase` did on
/// the gateway path.
pub async fn by_manifest_address(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    address: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let cids: Vec<(String,)> =
        sqlx::query_as("SELECT cid FROM ipfs_metadata WHERE text ILIKE $1 LIMIT 200")
            .bind(format!("%{address}%"))
            .fetch_all(pool)
            .await
            .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;
    let ids: Vec<String> = cids
        .iter()
        .filter_map(|(c,)| ipfs_hash_to_bytes32(c))
        .collect();
    hits_for_deployments(pool, ipfs, gate, lane, &ids, limit).await
}

/// The search box: which of the three questions this is, and its answer.
pub async fn search(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    q: &str,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let q = q.trim();
    if q.len() < 2 || q.len() > 100 {
        return Ok(Vec::new());
    }
    // The allowlist the incumbent applies. The identifiers reaching a nest statement are checked
    // again where they are built, so this is the outer of two guards rather than the only one.
    if !q
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b" -_.".contains(&b))
    {
        return Ok(Vec::new());
    }

    if q.len() == 42 && q.starts_with("0x") && q[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return by_manifest_address(pool, ipfs, gate, lane, q, 20).await;
    }
    if q.starts_with("Qm") && q.len() >= 8 {
        return by_hash_prefix(pool, ipfs, gate, lane, q, 10).await;
    }
    by_name(pool, ipfs, gate, lane, q, 10).await
}

/// Display names for deployments, keyed by the `Qm…` hash the caller asked with.
///
/// Every hash asked for appears in the answer, mapped to `None` where there is no name. A caller
/// merging this into a table needs to tell "no name" from "not asked about", and dropping the
/// unnamed ones would collapse the two.
pub async fn display_names_for_deployments(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    hashes: &[String],
) -> Result<BTreeMap<String, Option<String>>, ApiError> {
    let mut by_id: BTreeMap<String, String> = BTreeMap::new();
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    for h in hashes {
        out.insert(h.clone(), None);
        if let Some(id) = ipfs_hash_to_bytes32(h) {
            by_id.insert(id, h.clone());
        }
    }
    if by_id.is_empty() {
        return Ok(out);
    }
    let ids: Vec<String> = by_id.keys().cloned().collect();
    let meta = subgraph_metadata_for_deployments(pool, ipfs, gate, lane, &ids).await?;
    for (id, hash) in &by_id {
        if let Some(name) = meta.get(id).and_then(|m| m.display_name()) {
            out.insert(hash.clone(), Some(name.to_string()));
        }
    }
    Ok(out)
}

/// Fill in `displayName` and `categories` on a list of deployments.
///
/// The names live in IPFS documents keyed off the GNS nest, which is a different nest and a
/// different store from the one that produced the list, so this cannot be part of the shaping - it
/// needs a database and a second nest read. `shape::deployment` therefore leaves both fields
/// null-and-empty and this fills them in afterwards.
///
/// It is a no-op that logs nothing when the metadata is unavailable, because a deployment with no
/// name is an ordinary thing and a list that fails because a name is missing would be worse than a
/// list with a gap in it.
pub async fn enrich_deployments(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    data: &mut serde_json::Value,
) {
    let Some(items) = data.as_array_mut() else {
        return;
    };
    let ids: Vec<String> = items
        .iter()
        .filter_map(|d| Some(d.get("id")?.as_str()?.to_lowercase()))
        .collect();
    if ids.is_empty() {
        return;
    }
    let meta = match subgraph_metadata_for_deployments(pool, ipfs, gate, lane, &ids).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(err = %e, "deployment metadata unavailable; the list keeps its gaps");
            return;
        }
    };
    for item in items {
        let Some(id) = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase())
        else {
            continue;
        };
        let Some(m) = meta.get(&id) else { continue };
        if let Some(name) = m.display_name() {
            item["displayName"] = serde_json::Value::String(name.to_string());
        }
        if let Some(cats) = m
            .metadata
            .as_ref()
            .and_then(|d| d.get("categories"))
            .cloned()
            && cats.is_array()
        {
            item["categories"] = cats;
        }
    }
}

/// What one warm run did.
#[derive(Debug, Clone, Copy, Default)]
pub struct Warmed {
    pub fetched: usize,
    pub remaining: usize,
}

/// What the last warm run left undone, so a search can answer "not yet" rather than "no".
///
/// `-1` until a run has finished, which is not the same as zero: a process that restarted a minute
/// ago knows nothing about the backlog, and reporting nought would be a confident wrong answer.
static BACKLOG: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

/// `None` when no warm run has completed in this process.
pub fn warm_backlog() -> Option<u64> {
    match BACKLOG.load(std::sync::atomic::Ordering::Relaxed) {
        n if n < 0 => None,
        n => Some(n as u64),
    }
}

fn record(warmed: &Warmed) {
    BACKLOG.store(
        warmed.remaining as i64,
        std::sync::atomic::Ordering::Relaxed,
    );
    metrics::gauge!("ipfs_warm_remaining").set(warmed.remaining as f64);
    metrics::counter!("ipfs_warm_fetched_total").increment(warmed.fetched as u64);
}

/// Fill the IPFS cache that search reads: every live subgraph's metadata document, and every
/// signalled or staked deployment's manifest.
///
/// `budget` fetches per run, so the job stays inside its slot and the remainder is picked up next
/// time. `remaining` is the honest count of what is still missing, which is the figure that says
/// whether the budget is large enough.
pub async fn warm(
    pool: &Pool,
    ipfs: &Ipfs,
    gate: &NestGate,
    lane: Lane,
    budget: usize,
) -> Result<Warmed, ApiError> {
    let (current, deployments) = tokio::join!(
        ask(
            gate,
            NestId::Gns,
            "SELECT subgraph_metadata FROM subgraph_current WHERE NOT deprecated \
             AND subgraph_metadata IS NOT NULL"
                .into(),
            lane,
        ),
        ask(
            gate,
            NestId::Alloc,
            "SELECT id FROM lodestar_deployments WHERE signalled_tokens > 0 OR staked_tokens > 0"
                .into(),
            lane,
        ),
    );

    let want_json: BTreeSet<String> = current?
        .iter()
        .filter_map(|r| metadata_cid(r.get("subgraph_metadata").and_then(|v| v.as_str())))
        .collect();
    let want_text: BTreeSet<String> = deployments?
        .iter()
        .filter_map(|r| deployment_ipfs_hash(r.get("id")?.as_str()?))
        .collect();
    if want_json.is_empty() && want_text.is_empty() {
        let warmed = Warmed::default();
        record(&warmed);
        return Ok(warmed);
    }

    let all: Vec<String> = want_json.iter().chain(want_text.iter()).cloned().collect();
    let now = chrono::Utc::now();
    let have = kittiwake_ext::cached_docs(pool, &all)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    // A CID is wanted as JSON or as text, never both, so the two "already have it" sets are built
    // against different columns. Sharing one set would let a cached manifest satisfy a request for
    // a metadata document, and the subgraph would silently never get a name.
    let mut have_json = BTreeSet::new();
    let mut have_text = BTreeSet::new();
    for d in &have {
        if d.json.is_some() || d.failed_recently(now) {
            have_json.insert(d.cid.clone());
        }
        if d.text.is_some() || d.failed_recently(now) {
            have_text.insert(d.cid.clone());
        }
    }

    let todo_json: Vec<String> = want_json.difference(&have_json).cloned().collect();
    let todo_text: Vec<String> = want_text.difference(&have_text).cloned().collect();
    let total = todo_json.len() + todo_text.len();

    let take_json = todo_json.len().min(budget);
    let take_text = todo_text.len().min(budget - take_json);

    let json_futures = todo_json[..take_json]
        .iter()
        .cloned()
        .map(|c| async move { kittiwake_ext::fetch_json(pool, ipfs, &c).await.map(|_| ()) });
    futures::stream::iter(json_futures)
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let text_futures = todo_text[..take_text]
        .iter()
        .cloned()
        .map(|c| async move { kittiwake_ext::fetch_text(pool, ipfs, &c).await.map(|_| ()) });
    futures::stream::iter(text_futures)
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let fetched = take_json + take_text;
    let warmed = Warmed {
        fetched,
        remaining: total - fetched,
    };
    record(&warmed);
    Ok(warmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #43. The GNS nest and the IPFS documents behind it must be asked about the rows a reader
    /// will see, not about every candidate the ranking considered.
    ///
    /// A name search matching 434 deployments returns ten. Hydrating all 434 before truncating was
    /// up to 868 IPFS document lookups for ten names, and it is the reason a cold `?q=uniswap` cost
    /// seven seconds while `?q=graphprotocol`, which matches few, cost 0.17. The assertion is on
    /// the statement the nest actually receives, because that is the thing that was wrong; a test
    /// of the ordering alone would have passed throughout.
    #[tokio::test]
    async fn only_the_rows_that_survive_truncation_are_hydrated() {
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

        const CANDIDATES: usize = 250;
        const LIMIT: usize = 10;

        struct Nest(Arc<Mutex<Vec<(String, String)>>>);

        impl Respond for Nest {
            fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
                let path = req.url.path().to_string();
                if path.ends_with("/ready") {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                        "sealed_through": 1, "stalled": false
                    }));
                }
                let q = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "q")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                self.0.lock().unwrap().push((path.clone(), q.clone()));

                // The alloc nest returns a signal figure for every id it was asked about, so the
                // ranking has something to order by. The GNS nest returns nothing, which keeps the
                // IPFS half out of the test - and out of the pool, which is not connected.
                // Signal derived from the id itself, so the ranking is global rather than per
                // chunk. Deriving it from the row's position within its chunk makes each chunk's
                // own tail win, which is a property of the fixture and not of the code.
                let rows: Vec<serde_json::Value> = if path.starts_with("/alloc") {
                    ids_in(&q)
                        .into_iter()
                        .map(|id| {
                            let signal = u64::from_str_radix(&id[48..], 16).unwrap_or(0);
                            serde_json::json!({
                                "id": id,
                                "signalled_tokens": signal.to_string(),
                                "staked_tokens": "0",
                            })
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "count": rows.len(), "rows": rows,
                    "provenance": {"as_of": 1, "sealed_through": 1, "source": "mock"}
                }))
            }
        }

        /// The quoted ids in an `IN (...)` list, which is what the fan-out width and the
        /// truncation both show up in.
        fn ids_in(sql: &str) -> Vec<String> {
            sql.split('\'')
                .skip(1)
                .step_by(2)
                .filter(|s| is_id(s))
                .map(|s| s.to_string())
                .collect()
        }

        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Nest(seen.clone()))
            .mount(&server)
            .await;

        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in kittiwake_types::NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let gate = NestGate::new(cfg).unwrap();

        // Never connected to: the GNS nest answers with no rows, so no document is ever wanted and
        // `json_docs` returns before it reads the cache.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("a lazy pool needs no server");
        let ipfs = Ipfs::new("http://127.0.0.1:1");

        let ids: Vec<String> = (0..CANDIDATES).map(|n| format!("{n:064x}")).collect();
        let hits = hits_for_deployments(&pool, &ipfs, &gate, Lane::Read, &ids, LIMIT)
            .await
            .expect("the mock nest answers");
        assert_eq!(hits.len(), LIMIT);

        let seen = seen.lock().unwrap();
        let asked = |prefix: &str| -> Vec<String> {
            seen.iter()
                .filter(|(p, _)| p.starts_with(prefix))
                .flat_map(|(_, q)| ids_in(q))
                .collect()
        };

        // Ranking needs every candidate, so the alloc nest sees all of them.
        assert_eq!(
            asked("/alloc").len(),
            CANDIDATES,
            "the ranking must consider every candidate"
        );

        // Naming needs only the survivors. This is the assertion the old shape failed.
        let named = asked("/gns");
        assert_eq!(
            named.len(),
            LIMIT,
            "the gns nest was asked about {} deployments to name {LIMIT}",
            named.len()
        );

        // And they are the right ten: the mock ranks by position, so the highest ids win.
        let mut expected: Vec<String> = ids[CANDIDATES - LIMIT..].to_vec();
        expected.sort();
        let mut named = named;
        named.sort();
        assert_eq!(named, expected);
    }

    /// A fan-out asks the gate how wide the lane is rather than guessing. The inner lane is one
    /// narrower so a background job cannot take the last permit from a waiting reader, and it never
    /// reaches zero.
    #[test]
    fn a_lane_is_as_wide_as_the_nest_admits_and_the_inner_one_is_narrower() {
        let mut cfg = kittiwake_nest::NestConfig::new("http://example.invalid");
        cfg.permits.insert(NestId::Alloc, 4);
        cfg.permits.insert(NestId::Gns, 2);
        cfg.permits.insert(NestId::Dips, 1);
        let gate = NestGate::new(cfg).unwrap();

        assert_eq!(gate.lane_width(NestId::Alloc, Lane::Read), 4);
        assert_eq!(gate.lane_width(NestId::Alloc, Lane::Cron), 3);
        // The nest #43 is about: four slots were being asked of a gate holding two.
        assert_eq!(gate.lane_width(NestId::Gns, Lane::Read), 2);
        assert_eq!(gate.lane_width(NestId::Gns, Lane::Cron), 1);
        // A one-permit nest still has to be usable by the jobs that feed it.
        assert_eq!(gate.lane_width(NestId::Dips, Lane::Cron), 1);
    }

    #[test]
    fn the_zero_metadata_hash_is_no_document_rather_than_a_cid() {
        assert_eq!(metadata_cid(Some(&format!("0x{}", "0".repeat(64)))), None);
        assert_eq!(metadata_cid(Some(&"0".repeat(64))), None);
        assert_eq!(metadata_cid(None), None);
        assert_eq!(metadata_cid(Some("0xdeadbeef")), None);
    }

    #[test]
    fn a_real_metadata_hash_becomes_its_cid() {
        let cid = metadata_cid(Some(
            "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff",
        ));
        assert_eq!(
            cid.as_deref(),
            Some("Qmd5j2CeMs5NBUASQWjGbFmFpxGWhwKbgrTBZwV8hj65QJ")
        );
    }

    /// The ids reaching a nest statement come from a caller, so an id that is not one must be
    /// dropped before it is quoted rather than after.
    #[test]
    fn only_a_bytes32_id_is_quotable() {
        assert!(is_id(
            "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff"
        ));
        assert!(!is_id("0xdead"));
        assert!(!is_id("' OR 1=1 --"));
        assert!(!is_id(""));
    }

    /// The failure this constant exists for: a search that matches many subgraphs must not build a
    /// URL the nest refuses. 433 was the real number, from "uniswap".
    #[test]
    fn a_large_id_list_is_chunked_below_the_nests_url_limit() {
        let ids: Vec<String> = (0..433).map(|i| format!("0x{i:064x}")).collect();
        let chunks: Vec<_> = ids.chunks(IN_LIST_CHUNK).collect();
        assert_eq!(chunks.len(), 5);
        for c in chunks {
            // The `IN (...)` list plus a generous allowance for the rest of the statement, against
            // the 16 KB the nest accepts.
            assert!(
                quoted(c).len() + 500 < 16_384,
                "a chunk builds a URL the nest would refuse"
            );
        }
    }

    #[test]
    fn quoting_lowercases_and_separates() {
        assert_eq!(quoted(&["0xAB".into(), "0xCD".into()]), "'0xab', '0xcd'");
    }
}

#[cfg(test)]
mod backlog_tests {
    use super::*;

    /// The only test that touches `BACKLOG`, so the initial read is safe to assert.
    ///
    /// The distinction is the whole point of the field: a process a minute old knows nothing about
    /// the backlog, and answering nought would tell a searcher that everything is warmed when the
    /// truth is that nobody has looked. #8.
    #[test]
    fn the_backlog_is_unknown_until_a_run_finishes_and_not_zero() {
        assert_eq!(warm_backlog(), None, "a fresh process must not claim zero");
        record(&Warmed {
            fetched: 3,
            remaining: 7,
        });
        assert_eq!(warm_backlog(), Some(7));
        record(&Warmed {
            fetched: 7,
            remaining: 0,
        });
        assert_eq!(warm_backlog(), Some(0), "zero after a run is a real answer");
    }
}
