//! The HTTP surface. Each handler checks a budget, parses its parameters, and hands off to the
//! read layer; the shaping and the caching live there.
//!
//! Paths mirror the existing API exactly, so a route moves by pointing the frontend's rewrite here
//! instead of at the old handler, and moves back the same way.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::Event as SseEvent;
use axum::response::{IntoResponse, Response, Sse};
use kittiwake_read::routes;
use kittiwake_types::ApiError;

use crate::{AppState, ErrorResponse, client_ip, envelope, envelope_root};

fn scuttle(st: &AppState) -> Result<&crate::Scuttle, ErrorResponse> {
    st.scuttle
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("Scuttlebutt is not configured".into()).into())
}

fn pool(st: &AppState) -> Result<&kittiwake_db::Pool, ErrorResponse> {
    st.pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("database is not configured".into()).into())
}

fn scuttle_message(row: kittiwake_db::ScuttleMessage) -> kittiwake_scuttle::Message {
    kittiwake_scuttle::Message {
        id: row.id,
        room: row.room,
        name: row.name,
        tripcode: row.tripcode,
        body: row.body,
        created_at: row.created_at.to_rfc3339(),
    }
}

/// Extract a named cookie without accepting an almost-match. Cookie attributes are not sent back
/// by a browser, but treating an arbitrary `sb_adminish=` value as one would still be a rather
/// needless way to make authentication exciting.
pub(crate) fn scuttle_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

/// The moderator check, taking only what it decides on. Keeping `AppState` out of it is what makes
/// the refusals testable, and a moderation surface exercised only in production is not exercised.
pub(crate) fn scuttle_admin_ok(
    secret: &kittiwake_dock::Secret,
    headers: &HeaderMap,
    mutation: bool,
    now: i64,
) -> bool {
    let signed_in = scuttle_cookie(headers, "sb_admin")
        .and_then(|token| kittiwake_dock::parse_session(secret, token, now).ok())
        .as_deref()
        == Some("admin");
    // A cookie travels on every same-site request, so a mutation additionally wants something only
    // a script of ours sends. A browser will not attach this header cross-origin without CORS.
    signed_in && (!mutation || headers.get("x-sb-admin").and_then(|v| v.to_str().ok()) == Some("1"))
}

fn scuttle_admin(st: &AppState, headers: &HeaderMap, mutation: bool) -> Result<(), ErrorResponse> {
    let room = scuttle(st)?;
    if scuttle_admin_ok(&room.admin_session_secret, headers, mutation, now()) {
        return Ok(());
    }
    Err(ApiError::BadRequest("invalid Scuttlebutt administrator session".into()).into())
}

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

#[derive(serde::Deserialize)]
pub struct ScuttleHistory {
    pub before: Option<i64>,
    pub limit: Option<i64>,
}

/// The durable room history, newest first. The stream only tells a subscriber what happened after
/// it joined; history is what lets it recover from a reconnect or a reported broadcast gap.
pub async fn scuttle_messages(
    State(st): State<AppState>,
    Query(q): Query<ScuttleHistory>,
) -> Result<Response, ErrorResponse> {
    scuttle(&st)?;
    let rows =
        kittiwake_db::scuttle_history(pool(&st)?, q.before, q.limit.unwrap_or(50).clamp(1, 100))
            .await
            .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({ "messages": rows })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct ScuttlePost {
    pub name: Option<String>,
    pub body: String,
}

/// Store first, then fan out. Sending first makes a transient database failure visible in the
/// stream but absent from a reconnecting reader's history, which is precisely the sort of little
/// contradiction a chat room never forgets.
pub async fn scuttle_post(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ScuttlePost>,
) -> Result<Response, ErrorResponse> {
    let room = scuttle(&st)?;
    let caller = client_ip(peer, &headers, st.edge_secret.as_deref());
    st.limits.check(&st.limits.mutation, caller)?;
    let ip_hash = kittiwake_scuttle::hash_ip(&room.ip_pepper, &caller.to_string());
    if room.flood.check(&ip_hash).reason().is_some() {
        return Err(ApiError::RateLimited { retry_after: 2 }.into());
    }
    let identity = kittiwake_scuttle::parse_name(&room.salt, body.name.as_deref());
    let cleaned = kittiwake_scuttle::clean(&body.body, &[])
        .ok_or_else(|| ApiError::BadRequest("message is empty".into()))?;
    if cleaned.chars().count() > kittiwake_scuttle::MAX_BODY {
        return Err(ApiError::BadRequest(format!(
            "message is longer than {} characters",
            kittiwake_scuttle::MAX_BODY
        ))
        .into());
    }
    if kittiwake_db::scuttle_is_banned(pool(&st)?, &ip_hash, identity.tripcode.as_deref())
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
    {
        return Err(ApiError::BadRequest("you are banned from this room".into()).into());
    }
    let saved = kittiwake_db::scuttle_post(
        pool(&st)?,
        identity.name.as_deref(),
        identity.tripcode.as_deref(),
        &cleaned,
        &ip_hash,
    )
    .await
    .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let message = scuttle_message(saved);
    room.room.publish(kittiwake_scuttle::Event::Message {
        message: Box::new(message.clone()),
    });
    let mut response = axum::Json(serde_json::json!({ "message": message })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

/// The live half of the room. A lagged subscriber receives an explicit `gap` event and can refill
/// from `GET messages`; silently disconnecting it is a lie by omission.
pub async fn scuttle_stream(State(st): State<AppState>) -> Result<Response, ErrorResponse> {
    let subscriber = scuttle(&st)?.room.subscribe();
    let stream = futures::stream::unfold(subscriber, |mut subscriber| async move {
        let event = subscriber.next().await?;
        let name = match event {
            kittiwake_scuttle::Event::Message { .. } => "message",
            kittiwake_scuttle::Event::Delete { .. } => "delete",
            kittiwake_scuttle::Event::Gap { .. } => "gap",
        };
        let data = serde_json::to_string(&event).expect("Scuttlebutt events serialize");
        Some((
            Ok::<_, std::convert::Infallible>(SseEvent::default().event(name).data(data)),
            subscriber,
        ))
    });
    let mut response = Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct ScuttleDelete {
    pub reason: Option<String>,
}

pub async fn scuttle_delete(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ScuttleDelete>,
) -> Result<Response, ErrorResponse> {
    scuttle_admin(&st, &headers, true)?;
    if !kittiwake_db::scuttle_delete(pool(&st)?, id, body.reason.as_deref())
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
    {
        return Err(ApiError::NotFound.into());
    }
    scuttle(&st)?
        .room
        .publish(kittiwake_scuttle::Event::Delete { id });
    Ok(axum::Json(serde_json::json!({ "deleted": id })).into_response())
}

pub async fn scuttle_admin_messages(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ScuttleHistory>,
) -> Result<Response, ErrorResponse> {
    scuttle_admin(&st, &headers, false)?;
    scuttle_messages(State(st), Query(q)).await
}

#[derive(serde::Deserialize)]
pub struct ScuttleLogin {
    pub password: String,
}

/// Log in by minting the same stateless HMAC cookie as the incumbent. This is deliberately not a
/// second auth format: its payload is `admin:<issued-at>` and its signature uses SESSION_SECRET,
/// so a moderator's session survives moving the route.
pub async fn scuttle_admin_login(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<ScuttleLogin>,
) -> Result<Response, ErrorResponse> {
    let room = scuttle(&st)?;
    let equal: bool =
        subtle::ConstantTimeEq::ct_eq(body.password.as_bytes(), room.admin_secret.as_bytes())
            .into();
    if body.password.len() != room.admin_secret.len() || !equal {
        return Err(
            ApiError::BadRequest("invalid Scuttlebutt administrator password".into()).into(),
        );
    }
    let token = kittiwake_dock::create_session(&room.admin_session_secret, "admin", now());
    let mut response = axum::Json(serde_json::json!({ "admin": true })).into_response();
    let cookie = format!(
        "sb_admin={token}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
        kittiwake_dock::SESSION_TTL_SECS
    );
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&cookie).expect("session cookie is a valid header"),
    );
    Ok(response)
}

pub async fn scuttle_bans(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    scuttle_admin(&st, &headers, false)?;
    let bans = kittiwake_db::scuttle_bans(pool(&st)?)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    Ok(axum::Json(serde_json::json!({ "bans": bans })).into_response())
}

#[derive(serde::Deserialize)]
pub struct ScuttleBanRequest {
    pub ip_hash: Option<String>,
    pub tripcode: Option<String>,
    pub reason: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn scuttle_ban(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ScuttleBanRequest>,
) -> Result<Response, ErrorResponse> {
    scuttle_admin(&st, &headers, true)?;
    if body.ip_hash.is_none() && body.tripcode.is_none() {
        return Err(ApiError::BadRequest("a ban needs an IP hash or tripcode".into()).into());
    }
    let ban = kittiwake_db::scuttle_ban(
        pool(&st)?,
        body.ip_hash.as_deref(),
        body.tripcode.as_deref(),
        body.reason.as_deref(),
        body.expires_at,
    )
    .await
    .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    Ok(axum::Json(serde_json::json!({ "ban": ban })).into_response())
}

// ── The Dock ─────────────────────────────────────────────────────────────────
//
// Session-scoped, owner-scoped, and the only part of this service that writes on behalf of a
// person rather than a job. #16

fn dock(st: &AppState) -> Result<&crate::Dock, ErrorResponse> {
    st.dock
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("the Dock is not configured".into()).into())
}

/// The address a request's session vouches for, or nothing.
pub(crate) fn studio_address(
    secret: &kittiwake_dock::Secret,
    headers: &HeaderMap,
    now: i64,
) -> Option<String> {
    let token = scuttle_cookie(headers, kittiwake_dock::COOKIE_NAME)?;
    kittiwake_dock::parse_session(secret, token, now).ok()
}

/// The address, or a refusal. Every owner-scoped route starts here.
fn require_owner(st: &AppState, headers: &HeaderMap) -> Result<String, ErrorResponse> {
    studio_address(&dock(st)?.session_secret, headers, now())
        .ok_or_else(|| ApiError::BadRequest("not signed in".into()).into())
}

/// `GET /api/studio/auth`: who this request is, if anyone.
pub async fn studio_session(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let address = st
        .dock
        .as_ref()
        .and_then(|dock| studio_address(&dock.session_secret, &headers, now()));
    let mut response = axum::Json(serde_json::json!({ "address": address })).into_response();
    crate::no_store(response.headers_mut());
    response
}

#[derive(serde::Deserialize)]
pub struct StudioSignIn {
    pub address: String,
    pub message: String,
    pub signature: String,
}

/// `POST /api/studio/auth`: prove an address with a signature over a dated message.
pub async fn studio_sign_in(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<StudioSignIn>,
) -> Result<Response, ErrorResponse> {
    let dock = dock(&st)?;
    // A signature check is cheap for us and not free, and this is the one Dock route an unsigned-in
    // caller can reach, so it draws on the mutation budget.
    st.limits.check(
        &st.limits.mutation,
        client_ip(peer, &headers, st.edge_secret.as_deref()),
    )?;
    kittiwake_dock::verify_sign_in(&body.address, &body.message, &body.signature, now())
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let address = body.address.to_lowercase();
    let token = kittiwake_dock::create_session(&dock.session_secret, &address, now());
    let mut response = axum::Json(serde_json::json!({ "address": address })).into_response();
    set_studio_cookie(&mut response, &token, kittiwake_dock::SESSION_TTL_SECS);
    Ok(response)
}

/// `DELETE /api/studio/auth`: sign out.
pub async fn studio_sign_out() -> Response {
    let mut response = axum::Json(serde_json::json!({ "ok": true })).into_response();
    set_studio_cookie(&mut response, "", 0);
    response
}

fn set_studio_cookie(response: &mut Response, token: &str, max_age: i64) {
    let cookie = format!(
        "{}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax",
        kittiwake_dock::COOKIE_NAME
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, value);
    }
    crate::no_store(response.headers_mut());
}

/// `GET /api/studio/deploy-key`: whether one exists, never what it is.
pub async fn studio_deploy_key(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    let found = kittiwake_db::deploy_key_status(pool(&st)?, &owner)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({
        "hasKey": found.is_some(),
        "createdAt": found.as_ref().map(|k| k.created_at),
        "lastUsedAt": found.as_ref().and_then(|k| k.last_used_at),
    }))
    .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

/// `POST /api/studio/deploy-key`: mint one, and show the plaintext exactly once.
pub async fn studio_new_deploy_key(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    let key = kittiwake_dock::generate_deploy_key();
    kittiwake_db::upsert_deploy_key(pool(&st)?, &owner, &kittiwake_dock::hash_key(&key))
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({ "key": key })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

pub async fn studio_subgraphs(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    let subgraphs = kittiwake_db::list_subgraphs(pool(&st)?, &owner)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({ "subgraphs": subgraphs })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct NewSubgraph {
    #[serde(default)]
    pub slug: String,
    pub display_name: Option<String>,
}

/// A slug is `org/name`, lowercase, and it is the address the frontend routes on.
pub(crate) fn valid_slug(slug: &str) -> bool {
    let Some((org, name)) = slug.split_once('/') else {
        return false;
    };
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    part(org) && part(name)
}

pub async fn studio_create_subgraph(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NewSubgraph>,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    if !valid_slug(&body.slug) {
        return Err(ApiError::BadRequest(
            "slug must be \"org/name\" using lowercase letters, numbers and hyphens".into(),
        )
        .into());
    }
    if kittiwake_db::subgraph_by_slug(pool(&st)?, &body.slug)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
        .is_some()
    {
        return Err(ApiError::BadRequest("that slug is already taken".into()).into());
    }
    let subgraph =
        kittiwake_db::create_subgraph(pool(&st)?, &owner, &body.slug, body.display_name.as_deref())
            .await
            .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({ "subgraph": subgraph })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct SubgraphPatchBody {
    pub published_subgraph_id: Option<String>,
    pub version_label: Option<String>,
    pub last_published_deployment_id: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

pub async fn studio_patch_subgraph(
    State(st): State<AppState>,
    Path(id): Path<i32>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<SubgraphPatchBody>,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    // Same precedence the incumbent branches on: a published id wins, then a version label, then
    // the display metadata.
    let patch = if let Some(published) = body.published_subgraph_id.as_deref() {
        kittiwake_db::SubgraphPatch::Published {
            subgraph_id: published,
            version_label: body.version_label.as_deref(),
            last_deployment: body.last_published_deployment_id.as_deref(),
        }
    } else if body.version_label.is_some() {
        kittiwake_db::SubgraphPatch::Version {
            version_label: body.version_label.as_deref(),
            last_deployment: body.last_published_deployment_id.as_deref(),
        }
    } else {
        kittiwake_db::SubgraphPatch::Meta {
            display_name: body.display_name.as_deref(),
            description: body.description.as_deref(),
        }
    };
    if !kittiwake_db::patch_subgraph(pool(&st)?, id, &owner, patch)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
    {
        return Err(ApiError::NotFound.into());
    }
    Ok(axum::Json(serde_json::json!({ "ok": true })).into_response())
}

pub async fn studio_delete_subgraph(
    State(st): State<AppState>,
    Path(id): Path<i32>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    if !kittiwake_db::delete_subgraph(pool(&st)?, id, &owner)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
    {
        return Err(ApiError::NotFound.into());
    }
    Ok(axum::Json(serde_json::json!({ "ok": true })).into_response())
}

#[derive(serde::Deserialize)]
pub struct BountyFilter {
    pub deployment: Option<String>,
}

/// `GET /api/studio/bounties`: the open board, or one deployment's. Public.
pub async fn studio_bounties(
    State(st): State<AppState>,
    Query(q): Query<BountyFilter>,
) -> Result<Response, ErrorResponse> {
    let bounties = kittiwake_db::list_bounties(pool(&st)?, q.deployment.as_deref())
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({ "bounties": bounties })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct NewBountyBody {
    #[serde(default)]
    pub deployment_id: String,
    #[serde(default)]
    pub amount_grt: serde_json::Value,
    pub slug: Option<String>,
    pub message: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub chain_bounty_id: Option<serde_json::Value>,
    pub post_tx_hash: Option<String>,
}

pub async fn studio_create_bounty(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NewBountyBody>,
) -> Result<Response, ErrorResponse> {
    let owner = require_owner(&st, &headers)?;
    let amount = bounty_amount(&body.amount_grt)
        .ok_or_else(|| ApiError::BadRequest("amount_grt must be a positive number".into()))?;
    if body.deployment_id.is_empty() {
        return Err(ApiError::BadRequest("deployment_id is required".into()).into());
    }

    // A slug only attaches the bounty to a subgraph when the caller owns it. Someone else's slug is
    // ignored rather than refused, exactly as the incumbent does.
    let subgraph_id = match body.slug.as_deref() {
        Some(slug) => kittiwake_db::subgraph_by_slug(pool(&st)?, slug)
            .await
            .map_err(|e| ApiError::Unavailable(e.to_string()))?
            .filter(|s| s.owner_address == owner)
            .map(|s| s.id),
        None => None,
    };

    let chain_bounty_id = body.chain_bounty_id.as_ref().map(json_to_text);
    let bounty = kittiwake_db::create_bounty(
        pool(&st)?,
        kittiwake_db::NewBounty {
            deployment_id: &body.deployment_id,
            subgraph_id,
            developer_address: &owner,
            amount_grt: &amount,
            message: body.message.as_deref(),
            expires_at: body.expires_at,
            chain_bounty_id: chain_bounty_id.as_deref(),
            post_tx_hash: body.post_tx_hash.as_deref(),
        },
    )
    .await
    .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let mut response = axum::Json(serde_json::json!({ "bounty": bounty })).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

/// The amount as the column stores it: text, and only if it is a positive finite number.
pub(crate) fn bounty_amount(value: &serde_json::Value) -> Option<String> {
    let text = match value {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let parsed: f64 = text.parse().ok()?;
    (parsed.is_finite() && parsed > 0.0).then_some(text)
}

fn json_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `PATCH /api/studio/bounties/{id}`: bring one bounty's cached status up to the chain's.
///
/// Deliberately unauthenticated. The only status ever written is the one the BountyBoard reports,
/// so there is nothing for a caller to forge; requiring a session instead meant indexers, who
/// rarely have one, could not correct the cache at all.
pub async fn studio_reconcile_bounty(
    State(st): State<AppState>,
    Path(id): Path<i32>,
) -> Result<Response, ErrorResponse> {
    let bounty = kittiwake_db::bounty(pool(&st)?, id)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    let Some(chain_id) = bounty.chain_bounty_id.as_deref() else {
        return Err(
            ApiError::BadRequest("this bounty has no on-chain id to reconcile".into()).into(),
        );
    };
    let chain_id: u64 = chain_id
        .parse()
        .map_err(|_| ApiError::BadRequest("this bounty's on-chain id is not a number".into()))?;
    let dock = dock(&st)?;
    let (Some(board), Some(chain)) = (dock.bounty_board.as_deref(), st.read.chain.as_ref()) else {
        return Err(ApiError::Unavailable(
            "the BountyBoard or its RPC is not configured, so there is nothing to reconcile against"
                .into(),
        )
        .into());
    };
    let on_chain = chain.bounty(board, chain_id).await?;
    let desired = kittiwake_ingest::bounties::desired_status(&on_chain, now());
    let (status, claimed_by) = desired.clone();
    if kittiwake_ingest::bounties::needs_update(
        &bounty.status,
        bounty.claimed_by.as_deref(),
        desired,
    ) {
        kittiwake_db::update_bounty_status(pool(&st)?, id, status.as_str(), claimed_by.as_deref())
            .await
            .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    }
    Ok(axum::Json(serde_json::json!({
        "ok": true,
        "status": status.as_str(),
        "claimedBy": claimed_by,
    }))
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct MetadataBody {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version_label")]
    pub version_label: String,
}

fn default_version_label() -> String {
    "v0.0.1".into()
}

/// `POST /api/studio/metadata`: pin subgraph and version metadata, and return both forms of hash.
///
/// The bytes32 form is what GNS takes; the CIDv0 is what everything human-facing shows. Returning
/// both means the publish wizard never converts one into the other itself.
pub async fn studio_metadata(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<MetadataBody>,
) -> Result<Response, ErrorResponse> {
    require_owner(&st, &headers)?;
    let ipfs = &dock(&st)?.ipfs;
    let subgraph_meta = serde_json::json!({
        "description": body.description,
        "displayName": body.display_name,
        "image": "",
        "codeRepository": "",
        "website": "",
    });
    let version_meta = serde_json::json!({ "label": body.version_label, "description": "" });
    let (subgraph, version) =
        tokio::join!(ipfs.add_json(&subgraph_meta), ipfs.add_json(&version_meta));
    let subgraph = subgraph.map_err(ApiError::Unavailable)?;
    let version = version.map_err(ApiError::Unavailable)?;
    let bytes32 = |cid: &str| {
        kittiwake_read::shape::ipfs_hash_to_bytes32(cid)
            .ok_or_else(|| ApiError::Internal(format!("IPFS returned {cid}, which is not a CIDv0")))
    };
    let mut response = axum::Json(serde_json::json!({
        "subgraphMetaHash": subgraph,
        "versionMetaHash": version,
        "subgraphMetaBytes32": bytes32(&subgraph)?,
        "versionMetaBytes32": bytes32(&version)?,
    }))
    .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

/// The owner a bearer deploy key belongs to. Used by the two routes `graph deploy` calls, which
/// carry a key rather than a session because a CLI has no cookie jar.
async fn deploy_key_owner(st: &AppState, headers: &HeaderMap) -> Result<String, ErrorResponse> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ErrorResponse::from(ApiError::BadRequest("missing deploy key".into())))?;
    kittiwake_db::owner_by_key_hash(pool(st)?, &kittiwake_dock::hash_key(presented))
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("invalid deploy key".into()).into())
}

#[derive(serde::Deserialize)]
pub struct NodeCall {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub id: serde_json::Value,
}

/// `POST /api/studio/node`: enough of graph-node's JSON-RPC for `graph deploy` to finish.
///
/// There is no graph-node behind this. Subgraphs go to the decentralised network through the
/// publish flow, so all this does is check the key, check the slug belongs to its bearer, and
/// record the deployment hash the CLI just uploaded.
pub async fn studio_node(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NodeCall>,
) -> Result<Response, ErrorResponse> {
    let owner = deploy_key_owner(&st, &headers).await?;
    if body.jsonrpc != "2.0" {
        return Ok(rpc_error(-32600, "invalid JSON-RPC request"));
    }
    if !matches!(body.method.as_str(), "subgraph_create" | "subgraph_deploy") {
        return Ok(rpc_error(
            -32601,
            &format!("method not allowed: {}", body.method),
        ));
    }
    let name = body
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if name.is_empty() {
        return Ok(rpc_error(-32602, "missing subgraph name"));
    }
    let Some(subgraph) = kittiwake_db::subgraph_by_slug(pool(&st)?, name)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?
    else {
        return Ok(rpc_error(
            -32602,
            &format!("subgraph \"{name}\" not registered"),
        ));
    };
    if subgraph.owner_address != owner {
        return Ok(rpc_error(-32602, "not your subgraph"));
    }

    if body.method == "subgraph_create" {
        return Ok(axum::Json(
            serde_json::json!({ "jsonrpc": "2.0", "result": null, "id": body.id }),
        )
        .into_response());
    }

    if let Some(hash) = body.params.get("ipfs_hash").and_then(|v| v.as_str()) {
        // A recording failure must not fail the deploy: the artefacts are already on IPFS and the
        // CLI has nothing useful to do with the error.
        if let Err(e) = kittiwake_db::update_subgraph_deployment(
            pool(&st)?,
            name,
            hash,
            body.params.get("network").and_then(|v| v.as_str()),
        )
        .await
        {
            tracing::warn!(slug = name, error = %e, "could not record the deployed hash");
        }
    }
    let explorer = format!("https://thegraph.com/explorer/subgraphs/{name}");
    Ok(axum::Json(serde_json::json!({
        "jsonrpc": "2.0",
        "result": {
            "errors": [],
            "warnings": [],
            "playground": explorer,
            "queries": explorer,
            "subscriptions": explorer,
        },
        "id": body.id,
    }))
    .into_response())
}

fn rpc_error(code: i64, message: &str) -> Response {
    let mut response = axum::Json(serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message },
        "id": serde_json::Value::Null,
    }))
    .into_response();
    *response.status_mut() = axum::http::StatusCode::BAD_REQUEST;
    response
}

/// `GET|POST /api/studio/ipfs/{*path}`: the IPFS API, behind a deploy key.
///
/// `graph deploy` uploads the WASM and the manifest here before it calls `subgraph_deploy`. The
/// key is checked and then dropped: it is ours and means nothing to IPFS, and forwarding it would
/// hand a credential to a third party for no reason.
pub async fn studio_ipfs(
    State(st): State<AppState>,
    Path(path): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    method: axum::http::Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ErrorResponse> {
    deploy_key_owner(&st, &headers).await?;
    let endpoint = dock(&st)?.ipfs.endpoint();
    // `graph deploy` puts real arguments here - `pin=true`, `cid-version` - so dropping the query
    // would upload the artefacts and then not pin them.
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    let target = format!("{endpoint}/{path}{query}");

    let client = reqwest::Client::new();
    let mut request = client.request(method, &target);
    // Content-Type carries the multipart boundary, so it has to survive; nothing else does.
    if let Some(content_type) = headers.get(axum::http::header::CONTENT_TYPE) {
        request = request.header(axum::http::header::CONTENT_TYPE, content_type);
    }
    let upstream = request
        .body(body)
        .send()
        .await
        .map_err(|e| ApiError::Unavailable(format!("cannot reach IPFS: {e}")))?;

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| ApiError::Unavailable(format!("IPFS answered unreadably: {e}")))?;
    let mut response = Response::new(axum::body::Body::from(bytes));
    *response.status_mut() = axum::http::StatusCode::from_u16(status.as_u16())
        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, content_type);
    }
    crate::no_store(response.headers_mut());
    Ok(response)
}
