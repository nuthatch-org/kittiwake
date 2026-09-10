//! The HTTP surface. Each handler checks a budget, parses its parameters, and hands off to the
//! read layer; the shaping and the caching live there.
//!
//! Paths mirror the existing API exactly, so a route moves by pointing the frontend's rewrite here
//! instead of at the old handler, and moves back the same way.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use kittiwake_read::routes;
use kittiwake_types::ApiError;

use crate::{AppState, ErrorResponse, client_ip, envelope, envelope_root};

/// Seconds since the epoch, for the "since" windows.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Check the read budget for this caller, or refuse.
macro_rules! budget {
    ($st:expr, $peer:expr, $headers:expr) => {
        $st.limits.check(
            &$st.limits.read,
            client_ip($peer, &$headers, $st.edge_secret.as_deref()),
        )?
    };
}

/// The parameters every list route accepts.
#[derive(serde::Deserialize)]
pub struct ListQuery {
    #[serde(default = "d_first")]
    pub first: i64,
    #[serde(default)]
    pub skip: i64,
    #[serde(default)]
    pub order_by: Option<String>,
    #[serde(default)]
    pub order_direction: Option<String>,
}

fn d_first() -> i64 {
    100
}

impl ListQuery {
    fn descending(&self) -> bool {
        self.order_direction.as_deref() != Some("asc")
    }
    fn order(&self, default: &str) -> String {
        self.order_by.clone().unwrap_or_else(|| default.to_string())
    }
}

#[derive(serde::Deserialize)]
pub struct AddressQuery {
    pub address: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct CountQuery {
    #[serde(default = "d_count")]
    pub count: i64,
}
fn d_count() -> i64 {
    20
}

#[derive(serde::Deserialize)]
pub struct DaysQuery {
    #[serde(default = "d_days")]
    pub days: i64,
}
fn d_days() -> i64 {
    30
}

/// Twenty-six weeks, which is the window the stake-history panel plots and the number of points the
/// handler it replaces always returns. The `days` parameter still overrides it; this is only what
/// the panel gets when it asks for nothing.
fn d_stake_history_days() -> i64 {
    182
}

#[derive(serde::Deserialize)]
pub struct StakeHistoryQuery {
    #[serde(default = "d_stake_history_days")]
    pub days: i64,
}

/// An address from the path, lower-cased. The SQL layer refuses anything that is not one, but
/// normalising here means a caller using checksum case is served rather than rejected.
fn addr(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// A deployment id, from either spelling the reader might arrive with.
///
/// The nest stores deployments as a bytes32 hex id. Every link and label in the app is built from
/// the `Qm…` CIDv0, which is the same bytes wearing a multihash prefix and base58. So a route path
/// carries the CID and the query needs the hex.
///
/// Lowercasing is what a naive normaliser does, and it is exactly wrong here: base58 is
/// **case-sensitive**, so `to_lowercase` turns a valid CID into rubbish that then fails validation.
/// That shipped, and it 400'd `/api/indexing-status`, `/api/subgraph-curation` and
/// `/api/subgraph-history` for every caller using the form the frontend actually sends.
pub(crate) fn deployment_id(raw: &str) -> String {
    let t = raw.trim();
    kittiwake_read::shape::ipfs_hash_to_bytes32(t).unwrap_or_else(|| t.to_lowercase())
}

pub async fn indexers(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let read = routes::indexers(
        &st.read,
        routes::IndexersParams {
            first: q.first,
            skip: q.skip,
            order_by: q.order("stakedTokens"),
            descending: q.descending(),
        },
    )
    .await?;
    Ok(envelope(read))
}

pub async fn indexer(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(routes::indexer(&st.read, &addr(&address)).await?))
}

/// `/api/indexer-status/[address]`: what this indexer is actually serving, right now.
///
/// The nest says what it has allocated to; only the indexer's own status endpoint says whether it
/// is synced, and that is a live probe rather than a cached fold. The two are joined here: an
/// allocation with no matching status is one the indexer is paid for and is not serving, which is
/// the case this panel exists to surface.
pub async fn indexer_status(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = addr(&address);
    let read = routes::indexer_status(&st.read, &address).await?;

    let data = &*read.entry.value;
    let url = data
        .pointer("/indexer/url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let allocations = data
        .get("activeAllocations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // One probe for the whole indexer: its status endpoint answers for every deployment at once.
    let probe = if url.is_empty() {
        None
    } else {
        Some(st.ext.probe_indexer(&url).await)
    };
    let unreachable = probe.as_ref().map(|p| !p.reachable).unwrap_or(true);
    let statuses: std::collections::HashMap<String, &kittiwake_ext::IndexingStatus> = probe
        .as_ref()
        .map(|p| {
            p.statuses
                .iter()
                .map(|s| (s.deployment.clone(), s))
                .collect()
        })
        .unwrap_or_default();

    let (mut synced, mut syncing, mut failed, mut missing) = (0i64, 0i64, 0i64, 0i64);
    let deployments: Vec<serde_json::Value> = allocations
        .iter()
        .map(|a| {
            let dep = a
                .get("subgraph_deployment")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // The indexer answers in `Qm…`; the nest answers in hex. Join on the form the
            // indexer uses, or nothing matches and a healthy indexer reads as serving nothing.
            let ipfs = kittiwake_read::shape::ipfs_or_id(dep);
            let s = statuses.get(ipfs.as_str());
            // Only `failed` is a failure. `unhealthy` means the deployment has recorded errors
            // and is still serving; counting it as failed would report four working deployments
            // as broken, which is the sort of wrong that gets an indexer chased for nothing.
            match s {
                None => missing += 1,
                Some(s) if s.health == "failed" => failed += 1,
                Some(s) if s.synced => synced += 1,
                Some(_) => syncing += 1,
            }
            let progress = match s.and_then(|s| Some((s.latest_block?, s.chain_head_block?))) {
                Some((l, h)) if h > 0 => (l as f64 / h as f64) * 100.0,
                _ => 0.0,
            };
            let take = |k: &str, d: serde_json::Value| a.get(k).cloned().unwrap_or(d);
            serde_json::json!({
                "deploymentId": dep,
                "ipfsHash": ipfs,
                "displayName": serde_json::Value::Null,
                "allocatedTokens": take("allocated_tokens", serde_json::json!("0")),
                "signalledTokens": take("signalled_tokens", serde_json::json!("0")),
                "stakedTokens": take("deployment_staked_tokens", serde_json::json!("0")),
                // The nest sends this as a string on this view and a number on others.
                "createdAtEpoch": kittiwake_read::shape::i0(a, "created_at_epoch"),
                "health": s.map(|s| s.health.clone()).unwrap_or_else(|| "unknown".into()),
                // Unreachable is not unhealthy: we could not ask, which is a fact about us.
                "status": match s {
                    None => "unreachable",
                    Some(s) if s.health == "failed" => "failed",
                    Some(s) if s.synced => "synced",
                    Some(_) => "syncing",
                },
                "syncProgress": progress,
                "latestBlock": s.and_then(|s| s.latest_block).unwrap_or(0),
                "chainHeadBlock": s.and_then(|s| s.chain_head_block).unwrap_or(0),
                "blocksBehind": s.and_then(|s| s.blocks_behind).unwrap_or(0),
                "entityCount": "0",
                "fatalError": "",
                "network": "arbitrum-one",
            })
        })
        .collect();

    let body = serde_json::json!({
        "data": {
            "indexerAddress": address,
            "indexerUrl": url,
            "totalAllocations": deployments.len(),
            "syncedCount": synced,
            "syncingCount": syncing,
            "failedCount": failed,
            "unreachableCount": if unreachable { deployments.len() as i64 } else { missing },
            "deployments": deployments,
        },
        "source": "nuthatch",
        "provenance": &read.meta,
    });
    let mut resp = axum::Json(body).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

pub async fn apr_provenance(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let a = addr(&address);
    let read = routes::apr_provenance(&st.read, &a).await?;

    // The nest's own view of the pool, which the chain reading is checked against.
    let detail = read
        .entry
        .value
        .get("indexer")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let grt = |k: &str| {
        detail
            .get(k)
            .and_then(|v| v.as_str())
            .and_then(|x| x.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1e18
    };

    // The chain reading and the two Postgres reads are independent, so they go together. A pool
    // that reverts and a database that is absent each degrade their own half of the answer rather
    // than the whole of it: the trail is still worth showing without the reconciliation, and the
    // reconciliation without the trail.
    let chain = match &st.chain {
        Some(c) => c.delegation_pool(&a).await,
        None => None,
    };
    let reconcile = kittiwake_read::shape::pool_reconciliation(
        chain,
        grt("delegated_tokens"),
        grt("delegated_thawing_tokens"),
    );

    // An event the platform recorded without a timestamp cannot be placed on a timeline, so it is
    // dropped rather than dated to now.
    let mut trail = None;
    let events = match st.pool.as_ref() {
        Some(pool) => match kittiwake_db::provenance_rows(pool, &a).await {
            Ok(rows) => kittiwake_read::shape::provenance_events(
                &rows
                    .delegations
                    .into_iter()
                    .filter_map(|(t, d, g, ts)| Some((t?, Some(d), g, ts?.to_rfc3339())))
                    .collect::<Vec<_>>(),
                &rows
                    .params
                    .into_iter()
                    .filter_map(|(n, o, v, ts)| Some((n, o, v, ts?.to_rfc3339())))
                    .collect::<Vec<_>>(),
            ),
            Err(e) => {
                tracing::warn!(err = %e, "apr provenance trail unavailable");
                trail = Some("unavailable");
                serde_json::json!([])
            }
        },
        None => {
            trail = Some("no-database");
            serde_json::json!([])
        }
    };

    // A trail that could not be read is still an empty array, because lodestar's contract check
    // asserts `data.events` is one; the reason it is empty goes in the parity-ignored provenance.
    let mut provenance = serde_json::to_value(&read.meta).unwrap_or(serde_json::Value::Null);
    if let (Some(why), Some(obj)) = (trail, provenance.as_object_mut()) {
        obj.insert("trail".into(), serde_json::json!(why));
    }

    let mut resp = axum::Json(serde_json::json!({
        "data": { "reconcile": reconcile, "events": events },
        "provenance": provenance,
    }))
    .into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// Weekly points over the last two years, which is what the chart draws.
pub async fn indexer_stake_history(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<StakeHistoryQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let days = q.days.clamp(7, 730);
    let step = 86_400 * 7;
    let end = now();
    let start = end - days * 86_400;
    let cutoffs: Vec<i64> = (0..)
        .map(|i| start + i * step)
        .take_while(|t| *t <= end)
        .collect();
    Ok(envelope(
        routes::indexer_stake_history(&st.read, &addr(&address), &cutoffs).await?,
    ))
}

pub async fn curators(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(routes::curators(&st.read, q.first, q.skip).await?))
}

pub async fn epochs(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<CountQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(routes::epochs(&st.read, q.count).await?))
}

pub async fn network_stats(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(routes::network_stats(&st.read).await?))
}

/// `/api/grt-flow`: the supply, where it came from and where it went.
///
/// Its own handler, not an alias of `network_stats`. It was aliased until 2026-09-07 and therefore
/// answered 200 with the protocol row rather than the flow figures, which the panel could not read.
pub async fn grt_flow(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(routes::grt_flow(&st.read).await?))
}

pub async fn token_metrics(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<CountQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(routes::token_metrics(&st.read, q.count).await?))
}

/// Delegator or curator, chosen by `type`, defaulting to delegator as the old route does.
pub async fn portfolio(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<AddressQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = q
        .address
        .ok_or_else(|| ApiError::BadRequest("address is required".into()))?;
    let read = match q.r#type.as_deref() {
        Some("curator") => routes::curator_portfolio(&st.read, &addr(&address)).await?,
        _ => routes::delegator_portfolio(&st.read, &addr(&address)).await?,
    };
    Ok(envelope(read))
}

pub async fn rewards_history(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<AddressQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = q
        .address
        .ok_or_else(|| ApiError::BadRequest("address is required".into()))?;
    let read = routes::rewards_history(&st.read, &addr(&address)).await?;

    // The positions come from the nest; the daily exchange rates that turn them into a value over
    // time live in Lodestar's own Postgres, written by the snapshot cron. Neither half is the
    // series on its own, which is why this is composed here rather than in `routes`: `ReadCtx` has
    // no database, deliberately, and giving it one so this route could reach it would hand every
    // nest read a dependency only this one needs.
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    let positions: Vec<(String, f64, f64)> = read
        .entry
        .value
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let indexer = r.get("indexer")?.as_str()?.to_lowercase();
                    let wei = |k: &str| {
                        r.get(k)
                            .and_then(|v| v.as_str())
                            .and_then(|x| x.parse::<f64>().ok())
                            .unwrap_or(0.0)
                            / 1e18
                    };
                    Some((indexer, wei("staked_tokens"), wei("share_amount")))
                })
                .collect()
        })
        .unwrap_or_default();

    // Ninety days, as the handler this replaces defaults to.
    let since = chrono::Utc::now() - chrono::Duration::days(90);
    let indexers: Vec<String> = positions.iter().map(|(i, _, _)| i.clone()).collect();
    let rates = kittiwake_db::delegation_exchange_rates(pool, &indexers, since)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?
        .into_iter()
        .map(|(i, d, r)| (i, d.to_string(), r))
        .collect::<Vec<_>>();

    let body = kittiwake_read::shape::rewards_history(&positions, &rates);
    // At the root, not under `data`: that is the shape the panel reads.
    let mut resp = axum::Json(body).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

#[derive(serde::Deserialize)]
pub struct ProvisionsQuery {
    pub indexer: Option<String>,
    pub service: Option<String>,
    #[serde(default = "d_first")]
    pub first: i64,
    #[serde(default)]
    pub skip: i64,
}

pub async fn provisions(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ProvisionsQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let read = match (&q.indexer, &q.service) {
        (Some(i), _) => routes::provisions_by_indexer(&st.read, &addr(i)).await?,
        (_, Some(s)) => routes::provisions_by_service(&st.read, &addr(s), q.first, q.skip).await?,
        _ => {
            return Err(
                ApiError::BadRequest("one of indexer or service is required".into()).into(),
            );
        }
    };
    Ok(envelope(read))
}

#[derive(serde::Deserialize)]
pub struct ReceiverQuery {
    pub receiver: Option<String>,
}

pub async fn payments(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ReceiverQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let receiver = q.receiver.map(|r| addr(&r));
    Ok(envelope(
        routes::payments(&st.read, receiver.as_deref()).await?,
    ))
}

#[derive(serde::Deserialize)]
pub struct PoiQuery {
    pub deployment: Option<String>,
    #[serde(default = "d_poi_first")]
    pub first: i64,
}
fn d_poi_first() -> i64 {
    1000
}

pub async fn poi(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<PoiQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let dep = q.deployment.map(|d| addr(&d));
    Ok(envelope(
        routes::poi(&st.read, dep.as_deref(), q.first).await?,
    ))
}

#[derive(serde::Deserialize)]
pub struct DelegationEventsQuery {
    pub indexer: Option<String>,
    #[serde(default = "d_first")]
    pub first: i64,
    #[serde(default = "d_days")]
    pub days: i64,
}

pub async fn delegation_events(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DelegationEventsQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let since = now() - q.days.clamp(1, 3650) * 86_400;
    let indexer = q.indexer.map(|i| addr(&i));
    Ok(envelope(
        routes::delegation_events(&st.read, indexer.as_deref(), q.first, since).await?,
    ))
}

pub async fn delegation_flows(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DaysQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let since = now() - q.days.clamp(1, 3650) * 86_400;
    Ok(envelope(routes::delegation_flows(&st.read, since).await?))
}

pub async fn developer_activity(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DaysQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let cutoff = now() - q.days.clamp(7, 3650) * 86_400;
    Ok(envelope(
        routes::developer_activity(&st.read, cutoff).await?,
    ))
}

pub async fn subgraph_deployments(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let read = routes::subgraph_deployments(
        &st.read,
        q.first,
        q.skip,
        &q.order("signalledTokens"),
        q.descending(),
    )
    .await?;
    Ok(envelope(named(&st, read).await))
}

/// Put the names on a deployment list, when there is a database to read them from.
///
/// Without this the list carries `displayName: null` for everything. That looked like parity for a
/// while because the incumbent was returning null too - it was storing every metadata document
/// double-encoded (lodestar#100) - so both were blank and the diff was clean. Fixing theirs is what
/// exposed this one, which is a decent argument for comparing against something that works.
async fn named(st: &AppState, mut read: kittiwake_read::Read) -> kittiwake_read::Read {
    let Some(pool) = st.pool.as_ref() else {
        return read;
    };
    let mut data = (*read.entry.value).clone();
    kittiwake_read::search::enrich_deployments(
        pool,
        &st.ipfs,
        &st.read.gate,
        kittiwake_nest::Lane::Read,
        &mut data,
    )
    .await;
    // A fresh entry rather than a mutation: the cached one is shared, and enriching it in place
    // would put names into the cache keyed by a query that did not ask for them - and, worse, make
    // the cached value depend on whether a database happened to be configured on the run that
    // filled it. The provenance and age carry over unchanged, because they still describe where
    // the figures came from.
    read.entry = std::sync::Arc::new(kittiwake_cache::Entry {
        value: std::sync::Arc::new(data),
        provenance: read.entry.provenance.clone(),
        fetched_at: read.entry.fetched_at,
    });
    read
}

pub async fn subgraph_fees_30d(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let since = now() - 30 * 86_400;
    let read = routes::subgraph_fees_30d(&st.read, since).await?;
    Ok(envelope(named(&st, read).await))
}

pub async fn subgraph_curation(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        routes::subgraph_curation(&st.read, &deployment_id(&hash)).await?,
    ))
}

pub async fn subgraph_history(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        routes::subgraph_history(&st.read, &deployment_id(&hash)).await?,
    ))
}

/// `/api/indexing-status/<deployment>`: who is serving this deployment, and can anyone.
///
/// Two nest reads for the allocated set, then two probes against every indexer that published a
/// URL. The rules that turn those into an answer are in `kittiwake_read::indexing_status`, tested
/// on their own; this is the wiring. #23.
pub async fn indexing_status(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, ErrorResponse> {
    use kittiwake_read::indexing_status as is;

    budget!(st, peer, headers);
    let dep = deployment_id(&hash);
    let read = routes::deployment_status(&st.read, &dep).await?;

    let deployment = read
        .entry
        .value
        .get("deployment")
        .cloned()
        .unwrap_or_default();
    let rows = read
        .entry
        .value
        .get("allocations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let text = |v: &serde_json::Value, k: &str| -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(str::to_string)
    };
    let allocations: Vec<is::Allocated> = rows
        .iter()
        .filter_map(|r| {
            Some(is::Allocated {
                indexer_id: text(r, "indexer")?,
                indexer_name: None,
                url: text(r, "url").filter(|u| !u.trim().is_empty()),
                allocated_tokens: text(r, "allocated_tokens").unwrap_or_else(|| "0".into()),
            })
        })
        .collect();

    let ipfs_hash = kittiwake_read::shape::ipfs_or_id(&dep);
    // No receipt: the TAP key is not on this box yet, so the probe is receipt-less and reads
    // "the gate is up" rather than "the thing behind it works". That is the weaker of the two
    // readings and the Next handler falls back to exactly the same one when no signer is
    // configured, so this is the current behaviour rather than a regression. #11.
    let indexers = is::probe_all(&st.ext, &allocations, &ipfs_hash, None).await;

    let servability = is::assess_servability(&indexers, None);
    // One round, because the round store is not here yet. `apply_persistence` renders a lone dead
    // round as `rechecking`, which is the safe direction and what the handler being replaced does
    // whenever its own store is unavailable. It can never render `dead` from one round.
    let round = is::Round {
        probed_at: chrono::Utc::now().to_rfc3339(),
        serving_operators: servability.effective_serving_operators,
        serving_indexers: servability.serving_indexer_count,
        gateway_verdict: None,
    };
    let rendered = is::apply_persistence(&[round], is::DEFAULT_DEAD_ROUNDS);

    let body = is::shape(
        &is::Deployment {
            id: dep.clone(),
            ipfs_hash,
            display_name: None,
            signalled_tokens: text(&deployment, "signalled_tokens").unwrap_or_else(|| "0".into()),
            staked_tokens: text(&deployment, "staked_tokens").unwrap_or_else(|| "0".into()),
            total_allocations: deployment
                .get("allocations")
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .unwrap_or(allocations.len() as i64),
        },
        &indexers,
        &servability,
        &rendered,
    );
    let mut out = read;
    // The provenance and age carry over: they describe where the allocated set came from, which is
    // still true. The probes are ours and are dated by `probedAt` inside the payload.
    out.entry = std::sync::Arc::new(kittiwake_cache::Entry {
        value: std::sync::Arc::new(body),
        provenance: out.entry.provenance.clone(),
        fetched_at: out.entry.fetched_at,
    });
    Ok(envelope(out))
}

#[derive(serde::Deserialize)]
pub struct DipsQuery {
    #[serde(default)]
    pub network: Option<String>,
}

pub async fn dips(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DipsQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    // Mainnet has produced no agreement events; sepolia is where the lifecycle has rows, which is
    // what anything reading it should be developed against.
    let sepolia = q.network.as_deref() == Some("sepolia");
    Ok(envelope(routes::dips(&st.read, sepolia).await?))
}

// ---------------------------------------------------------------------------------------------
// Outside sources
// ---------------------------------------------------------------------------------------------

/// `/api/price`. Cached hard, because a price API is rate-limited and a page of protocol figures
/// must never be late because of one. When the source is down this reports the failure rather than
/// serving an old price as a current one.
pub async fn price(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let read = st
        .read
        .cached_external(
            "price",
            std::time::Duration::from_secs(60),
            kittiwake_types::ServedFrom::Chain,
            {
                let ext = st.ext.clone();
                std::sync::Arc::new(move || {
                    let ext = ext.clone();
                    Box::pin(async move {
                        let (usd, change) = ext.grt_price().await?;
                        // `price`, not `usd`: the callers were written against the incumbent.
                        Ok(serde_json::json!({ "price": usd, "change24h": change }))
                    })
                })
            },
        )
        .await?;
    Ok(envelope_root(read))
}

/// `/api/tvl`.
pub async fn tvl(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let read = st
        .read
        .cached_external(
            "tvl",
            std::time::Duration::from_secs(600),
            kittiwake_types::ServedFrom::Chain,
            {
                let ext = st.ext.clone();
                std::sync::Arc::new(move || {
                    let ext = ext.clone();
                    Box::pin(async move {
                        let tvl = ext.arbitrum_tvl().await?;
                        Ok(serde_json::json!({ "tvl": tvl }))
                    })
                })
            },
        )
        .await?;
    Ok(envelope_root(read))
}

// ---------------------------------------------------------------------------------------------
// Chain reads and live probes
// ---------------------------------------------------------------------------------------------

/// `/api/reo`: the Rewards Eligibility Oracle's verdict on one indexer.
///
/// Cached for five minutes. This is a contract read, so it is true at one block and says nothing
/// about any other; the age of the cache entry is the whole of what can honestly be said about how
/// current it is, and that is what the envelope reports.
pub async fn reo(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<AddressQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = q
        .address
        .ok_or_else(|| ApiError::BadRequest("address is required".into()))?;
    let address = addr(&address);
    let chain = st
        .chain
        .clone()
        .ok_or_else(|| ApiError::Unavailable("no Arbitrum RPC is configured".into()))?;

    let route_key = format!("reo:{address}");
    let read = st
        .read
        .cached_external_keyed(
            "reo",
            &route_key,
            std::time::Duration::from_secs(300),
            kittiwake_types::ServedFrom::Chain,
            {
                std::sync::Arc::new(move || {
                    let chain = chain.clone();
                    let address = address.clone();
                    Box::pin(async move {
                        let e = chain.eligibility(&address).await?;
                        // Under `status`, in the vocabulary the panel reads. `daysRemaining` is a
                        // float because a partial day is the useful part of the answer.
                        let expires_at = match (e.renewed_at, e.period_seconds) {
                            (Some(r), Some(p)) if r > 0 => serde_json::json!(r + p),
                            _ => serde_json::Value::Null,
                        };
                        Ok(serde_json::json!({ "status": {
                            "address": address,
                            "available": true,
                            "isEligible": e.eligible,
                            "status": if e.eligible { "eligible" } else { "ineligible" },
                            "source": "oracle",
                            "renewalTimestamp": e.renewed_at,
                            "eligibilityPeriod": e.period_seconds,
                            "expiresAt": expires_at,
                            "daysRemaining": e.expires_in.map(|s| s as f64 / 86_400.0),
                            // Ours to add: the oracle's own currency, which the incumbent cannot say.
                            "oracleUpdatedAt": e.oracle_updated_at,
                            "oracleStale": e.oracle_stale,
                            "enforced": e.enforced,
                        }}))
                    })
                })
            },
        )
        .await?;
    Ok(envelope_root(read))
}

#[derive(serde::Deserialize)]
pub struct NodeHealthQuery {
    pub url: String,
}

/// `/api/indexer-node-health`: ask an indexer's own status endpoint what it is serving.
///
/// The URL is supplied by the caller and, in the ordinary path, comes from the on-chain service
/// registry, which anyone may write to. It goes through the SSRF guard before any request is made.
/// Not cached by route: the point of this one is that it is live.
pub async fn indexer_node_health(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<NodeHealthQuery>,
) -> Result<Response, ErrorResponse> {
    st.limits.check(
        &st.limits.mutation,
        client_ip(peer, &headers, st.edge_secret.as_deref()),
    )?;
    let result = st.ext.probe_indexer(&q.url).await;
    let mut resp = axum::Json(serde_json::json!({ "data": result })).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// `/api/indexers-enriched`: the directory with scores, read from the rows the refresh job wrote.
///
/// Postgres rather than the nest, deliberately. The score folds six nest reads and an oracle call
/// per indexer; recomputing that per page view would spend the nest's whole permit budget on one
/// table. The freshness of the answer is the freshness of the job, which is what `lastUpdated`
/// on each row reports.
pub async fn indexers_enriched(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    let rows = kittiwake_db::enriched_indexers(pool, q.first.clamp(1, 1000))
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    let mut resp = axum::Json(serde_json::json!({
        "data": rows,
        "provenance": { "served_from": "postgres", "stale": false, "age_seconds": 0 },
    }))
    .into_response();
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-kittiwake-served-from"),
        axum::http::HeaderValue::from_static("postgres"),
    );
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// `/api/horizon/activity`: the newest events on HorizonStaking.
///
/// Served straight from what the activity job last wrote. If the job has not run yet the answer is
/// an empty feed with its age, not a fabricated one and not an error: an empty list is honest, and
/// the age says how much to trust it.
pub async fn horizon_activity(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let entry = st
        .read
        .store
        .get(kittiwake_read::shape::HORIZON_ACTIVITY_KEY)
        .await;
    let (data, meta) = match entry {
        Some(e) => {
            let meta = e.meta(kittiwake_types::ServedFrom::Cache, false);
            ((*e.value).clone(), meta)
        }
        None => (
            serde_json::json!([]),
            kittiwake_types::Meta {
                provenance: Default::default(),
                served_from: kittiwake_types::ServedFrom::Cache,
                stale: true,
                age_seconds: 0,
            },
        ),
    };
    let mut resp =
        axum::Json(serde_json::json!({ "data": data, "source": "nuthatch", "provenance": &meta }))
            .into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// `/api/chain-lag`: per-chain lag and liveness, as the chain-health job last computed it.
///
/// `data` is null before the job's first run rather than an empty set of chains, because "we have
/// not looked" and "every chain is fine" are different answers and the panel renders them
/// differently.
pub async fn chain_lag(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    live_or(
        &st,
        kittiwake_read::shape::CHAIN_LAG_KEY,
        serde_json::Value::Null,
    )
    .await
}

/// `/api/dropped-chains`: indexer address to the chains it has stopped serving since the last run.
///
/// Empty here means "nobody dropped anything", which is the ordinary case and is not the same as
/// the chain-lag null above: an absent map and an empty map say the same thing to the caller.
pub async fn dropped_chains(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    live_or(
        &st,
        kittiwake_read::shape::DROPPED_CHAINS_KEY,
        serde_json::json!({}),
    )
    .await
}

/// Serve whatever a live job last wrote.
///
/// Three answers in order of preference: this process's store, which is what serves every request
/// in the ordinary case; the durable copy in Postgres, which covers the window between a restart
/// and the job's next run; and `absent` marked stale, which is what an honestly empty answer looks
/// like. A thirty-minute job would otherwise leave a panel blank for thirty minutes after every
/// deploy, and a blank panel is indistinguishable from a chain nobody is serving.
async fn live_or(
    st: &AppState,
    key: &str,
    absent: serde_json::Value,
) -> Result<Response, ErrorResponse> {
    let (data, meta) = match st.read.store.get(key).await {
        Some(e) => {
            let meta = e.meta(kittiwake_types::ServedFrom::Cache, false);
            ((*e.value).clone(), meta)
        }
        None => match kept(st, key).await {
            Some(pair) => pair,
            None => (
                absent,
                kittiwake_types::Meta {
                    provenance: Default::default(),
                    served_from: kittiwake_types::ServedFrom::Cache,
                    stale: true,
                    age_seconds: 0,
                },
            ),
        },
    };
    let mut resp =
        axum::Json(serde_json::json!({ "data": data, "provenance": &meta })).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// `/api/subgraph-search`: one box, three questions.
///
/// A `0x…` address searches deployment manifests, a `Qm…` prefix searches deployment hashes, and
/// anything else searches display names. Names live only in IPFS documents, so that last one reads
/// the cache the warm job fills; a subgraph published minutes ago is not findable by name until
/// that job has been round.
pub async fn subgraph_search(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?
        .clone();

    // A name search costs ten nest queries across two nests to return ten rows, and a search box
    // asks the same question on every keystroke and every back-button. The key is the trimmed,
    // lowercased query, so it is caller-controlled; the store is a bounded LRU and the read budget
    // already caps how fast anyone can mint keys.
    let query = q.q.unwrap_or_default();
    let key = query.trim().to_lowercase();
    let (ipfs, gate) = (st.ipfs.clone(), st.read.gate.clone());
    let read = st
        .read
        .cached_external_keyed(
            "subgraph-search",
            &key,
            std::time::Duration::from_secs(60),
            kittiwake_types::ServedFrom::Nest,
            std::sync::Arc::new(move || {
                let (pool, ipfs, gate, query) =
                    (pool.clone(), ipfs.clone(), gate.clone(), query.clone());
                Box::pin(async move {
                    let hits = kittiwake_read::search::search(
                        &pool,
                        &ipfs,
                        &gate,
                        kittiwake_nest::Lane::Read,
                        &query,
                    )
                    .await?;
                    Ok(serde_json::json!(hits))
                })
            }),
        )
        .await?;
    let hits = (*read.entry.value).clone();
    // Names live in IPFS documents somebody has to have fetched first, so an empty result can mean
    // "no such subgraph" or "not warmed yet", and only this figure separates them. Null until a
    // warm run has finished in this process.
    let mut resp = axum::Json(serde_json::json!({
        "data": hits,
        "source": "nuthatch",
        "warmBacklog": kittiwake_read::search::warm_backlog(),
    }))
    .into_response();
    // In a header rather than the body: the body is compared against the incumbent's and this is
    // ours to say.
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-kittiwake-served-from"),
        axum::http::HeaderValue::from_static(read.meta.served_from.as_str()),
    );
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

#[derive(serde::Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct NamesBody {
    #[serde(default)]
    pub hashes: Vec<String>,
}

/// `/api/subgraph-names`: deployment hashes to display names, in a batch.
///
/// Foghorn's needs-attention surface is the caller: the deployments it reports are often unsignalled
/// and so never appear in any top-N list that would have carried a name already.
///
/// Only named deployments appear in the answer. That is the incumbent's shape, and it is what the
/// caller merges into its own rows.
pub async fn subgraph_names(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NamesBody>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    // Capped at five hundred, as the incumbent caps it: this fans out to one IPFS fetch per
    // uncached document, and an uncapped list is a way to ask us to spend an afternoon on it.
    let mut seen = std::collections::BTreeSet::new();
    let hashes: Vec<String> = body
        .hashes
        .into_iter()
        .filter(|h| h.starts_with("Qm") || h.starts_with("baf"))
        .filter(|h| seen.insert(h.clone()))
        .take(500)
        .collect();
    if hashes.is_empty() {
        let mut resp = axum::Json(serde_json::json!({ "data": {} })).into_response();
        crate::no_store(resp.headers_mut());
        return Ok(resp);
    }

    let names = kittiwake_read::search::display_names_for_deployments(
        pool,
        &st.ipfs,
        &st.read.gate,
        kittiwake_nest::Lane::Read,
        &hashes,
    )
    .await?;
    let map: serde_json::Map<String, serde_json::Value> = names
        .into_iter()
        .filter_map(|(h, n)| Some((h, serde_json::Value::String(n?))))
        .collect();

    let mut resp =
        axum::Json(serde_json::json!({ "data": map, "source": "nuthatch" })).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// The durable copy of a live answer, with its real age.
///
/// Always marked stale, whatever the age: it is by definition older than the job's last intent to
/// run, and a reader deciding whether to trust a figure should be told so rather than left to infer
/// it from a timestamp.
async fn kept(st: &AppState, key: &str) -> Option<(serde_json::Value, kittiwake_types::Meta)> {
    let pool = st.pool.as_ref()?;
    let (value, computed_at) = kittiwake_db::get_live_answer(pool, key)
        .await
        .map_err(|e| tracing::warn!(key, err = %e, "could not read the kept live answer"))
        .ok()??;
    let age = (chrono::Utc::now() - computed_at).num_seconds().max(0) as u64;
    Some((
        value,
        kittiwake_types::Meta {
            provenance: Default::default(),
            served_from: kittiwake_types::ServedFrom::Postgres,
            stale: true,
            age_seconds: age,
        },
    ))
}

/// `/api/support`: the graph-support archive, as the twice-daily mirror last left it.
///
/// The dashboard used to call GitHub for this from its own serverless function, which put a
/// credential in the frontend's environment and a rate limit in the page load. The mirror removes
/// both: the repository is public, two requests a day sit well inside what GitHub allows
/// unauthenticated, and the page reads a table.
///
/// An empty table is a failure rather than an empty archive. That repository has never had zero
/// issues, so nothing here means the job has not run yet or has never succeeded, and answering
/// `{"issues": []}` would render as "no issues" on a page whose whole purpose is that the answers
/// exist and are findable.
pub async fn support(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    let issues = kittiwake_db::graph_support_issues(pool)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    if issues.is_empty() {
        return Err(
            ApiError::Unavailable("the graph-support mirror has not run yet".into()).into(),
        );
    }

    // The freshness of the rows, not of this request. Read from the table so it describes the
    // data being served rather than a cursor that may have moved while the write failed.
    let fetched_at = kittiwake_db::graph_support_ingested_at(pool)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    let mut resp = axum::Json(serde_json::json!({
        "issues": issues,
        "fetchedAt": fetched_at,
    }))
    .into_response();
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-kittiwake-served-from"),
        axum::http::HeaderValue::from_static("postgres"),
    );
    crate::no_store(resp.headers_mut());
    Ok(resp)
}
