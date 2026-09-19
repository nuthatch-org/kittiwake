//! The HTTP surface. Each handler checks a budget, parses its parameters, and hands off to the
//! read layer; the shaping and the caching live there.
//!
//! Paths mirror the existing API exactly, so a route moves by pointing the frontend's rewrite here
//! instead of at the old handler, and moves back the same way.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use kittiwake_read::routes;
use kittiwake_types::ApiError;

use crate::{AppState, ErrorResponse, client_ip, envelope, envelope_root};

fn dock(st: &AppState) -> Result<&crate::Studio, ErrorResponse> {
    st.studio
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("the Dock is not configured".into()).into())
}

/// The address a request's session vouches for, taking only what it decides on.
///
/// Split out from [`session`] so the refusals are testable without an `AppState`: the Dock's whole
/// authorisation is this cookie, and every owner-scoped route is exactly as strong as it is.
pub(crate) fn session_address(
    secret: &kittiwake_dock::Secret,
    headers: &HeaderMap,
    now: i64,
) -> Option<String> {
    let token = headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|all| {
            all.split(';')
                .map(str::trim)
                .find_map(|part| part.strip_prefix("studio_session="))
        })?;
    kittiwake_dock::parse_session(secret, token, now).ok()
}

fn session(st: &AppState, headers: &HeaderMap) -> Result<String, ErrorResponse> {
    session_address(&dock(st)?.secret, headers, now()).ok_or_else(|| ApiError::Unauthorized.into())
}

fn pool(st: &AppState) -> Result<&kittiwake_db::Pool, ErrorResponse> {
    st.pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()).into())
}

fn db_err(e: kittiwake_db::DbError) -> ErrorResponse {
    ApiError::Unavailable(format!("database: {e}")).into()
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
pub struct TrendsQuery {
    #[serde(default = "d_days")]
    pub days: i64,
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
    Ok(envelope(named_indexers(&st, read).await))
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

/// Put the verified indexer names from Postgres on the accounts. A failed read leaves them null:
/// a name is decoration on the figures, and must not turn the route into a 502.
async fn named_indexers(st: &AppState, mut read: kittiwake_read::Read) -> kittiwake_read::Read {
    let Some(pool) = st.pool.as_ref() else {
        return read;
    };
    let ids = kittiwake_read::shape::indexer_account_ids(&read.entry.value);
    if ids.is_empty() {
        return read;
    }
    let names = match kittiwake_db::indexer_names(pool, &ids).await {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!("indexer names unreadable, serving without them: {e}");
            return read;
        }
    };
    let mut data = (*read.entry.value).clone();
    kittiwake_read::shape::put_indexer_names(&mut data, &names);
    read.entry = std::sync::Arc::new(kittiwake_cache::Entry {
        value: std::sync::Arc::new(data),
        provenance: read.entry.provenance.clone(),
        fetched_at: read.entry.fetched_at,
    });
    read
}

/// `/api/indexer-status/[address]`: what this indexer is actually serving, right now.
///
/// The nest says what it has allocated to; only the indexer's own status endpoint says whether it
/// is synced, and that is a probe of the node, served with its age rather than waited for. See
/// [`kittiwake_read::status_probe`]. The two are joined here: an
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
    let node = if url.is_empty() {
        None
    } else {
        Some(st.probes.observe(&url).await)
    };
    let probe = node
        .as_ref()
        .and_then(|n| n.checked.as_deref())
        .map(|c| &c.result);
    let unreachable = probe.map(|p| !p.reachable).unwrap_or(true);
    let statuses: std::collections::HashMap<String, &kittiwake_ext::IndexingStatus> = probe
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
            "node": node.as_ref().map(|n| n.json()),
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

#[derive(serde::Deserialize)]
pub struct QosQuery {
    #[serde(default = "d_qos_days")]
    pub days: i64,
}

/// Ninety days, the window the Query Performance card always drew.
fn d_qos_days() -> i64 {
    90
}

/// Whole UTC days, today included, so the newest point is the day in progress.
pub async fn indexer_qos(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<QosQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let days = q.days.clamp(1, 90);
    let today = now().div_euclid(86_400);
    Ok(envelope(
        kittiwake_read::qos::indexer_qos(
            &st.read,
            qos_store(&st),
            &addr(&address),
            today - (days - 1),
            today,
        )
        .await?,
    ))
}

/// Closed QoS days live in Postgres; without it the QoS routes answer unavailable.
fn qos_store(st: &AppState) -> Option<&dyn kittiwake_read::qos::QosStore> {
    st.pool
        .as_ref()
        .map(|p| p as &dyn kittiwake_read::qos::QosStore)
}

pub async fn indexer_qos_score(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        kittiwake_read::qos::indexer_qos_score(
            &st.read,
            qos_store(&st),
            &addr(&address),
            now().div_euclid(86_400),
        )
        .await?,
    ))
}

pub async fn indexer_qos_deployments(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        kittiwake_read::qos::indexer_qos_deployments(
            &st.read,
            qos_store(&st),
            &addr(&address),
            now().div_euclid(86_400),
        )
        .await?,
    ))
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

/// Whole UTC days, today included, so the newest bar is the day in progress.
pub async fn indexer_trends(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<TrendsQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let days = q.days.clamp(1, 90);
    let since = (now() / 86_400 - (days - 1)) * 86_400;
    Ok(envelope(
        routes::indexer_trends(&st.read, &addr(&address), since).await?,
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
    // `deployment_id`, not `addr`: the latter lowercases, base58 is case-sensitive, and this is
    // the fourth route to make that mistake. Every POI deployment page 400'd on it.
    let dep = q.deployment.map(|d| deployment_id(&d));
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

/// One deployment's directory row, by IPFS hash.
pub async fn subgraph_deployment(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let read = routes::subgraph_deployment(&st.read, &hash).await?;
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
    let read = routes::subgraph_fees_30d(&st.read, thirty_days_back(now())).await?;
    Ok(envelope(named(&st, read).await))
}

/// The start of a thirty-day window, moved on by the hour. `since` is part of the cache key, and
/// to the second it was a new key on every request: `/api/subgraph-fees-30d` asked the nest every
/// time (x-kittiwake-age 0 twice running, 2026-09-19).
fn thirty_days_back(now: i64) -> i64 {
    now - now.rem_euclid(3600) - 30 * 86_400
}

/// `/api/subgraph-directory`: the deployments list, filtered, sorted and paged over the whole set.
pub async fn subgraph_directory(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let q = kittiwake_read::directory::Query::parse(&params).map_err(ApiError::BadRequest)?;
    let now = now();
    let read = routes::subgraph_directory(&st.read, thirty_days_back(now)).await?;
    let set = &*read.entry.value;
    let page = kittiwake_read::directory::select(set, &q, now);
    let mut resp = axum::Json(serde_json::json!({
        "data": page.rows,
        "total": page.total,
        "unanalysed": set.get("unanalysed"),
        "facets": set.get("facets"),
        "source": "nuthatch",
        "provenance": &read.meta,
    }))
    .into_response();
    crate::provenance_headers(resp.headers_mut(), &read.meta);
    crate::no_store(resp.headers_mut());
    Ok(resp)
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
    // Paid when this deployment has a payer, and receipt-less when it does not. Unpaid reads
    // "the gate is up" rather than "the thing behind it works", which is the weaker of the two
    // readings and the one the Next handler also falls back to with no signer configured. #11.
    let indexers = is::probe_all(&st.ext, &allocations, &ipfs_hash, st.tap_payer.as_deref()).await;

    let servability = is::assess_servability(&indexers, None);
    let probed_at = chrono::Utc::now();
    let this_round = is::Round {
        probed_at: probed_at.to_rfc3339(),
        serving_operators: servability.effective_serving_operators,
        serving_indexers: servability.serving_indexer_count,
        gateway_verdict: None,
    };

    // Best-effort throughout: "effectively dead" is derived from the last K rounds, and a store
    // that is down must not take the status page with it. With no history `apply_persistence`
    // renders a dead round as `rechecking`, which is the safe direction - it can never say `dead`
    // on one round - so a failure here costs certainty rather than correctness.
    let history = match st.pool.as_ref() {
        None => vec![this_round.clone()],
        Some(pool) => {
            let verdict_json = serde_json::to_value(&servability)
                .ok()
                .map(|mut v| {
                    if let Some(obj) = v.as_object_mut() {
                        // The per-indexer evidence, which is what tells an edge block from a
                        // timeout once the round is history. The probe and who it was of, not the
                        // whole status row: the sync figures are already the answer this round
                        // returned, and the question a stored round has to settle later is what
                        // each probe actually saw. lodestar#62.
                        let probes: Vec<serde_json::Value> = indexers
                            .iter()
                            .filter_map(|i| {
                                let d = i.serve_probe_detail.as_ref()?;
                                let mut v = serde_json::to_value(d).ok()?;
                                let o = v.as_object_mut()?;
                                o.insert("indexerId".into(), i.indexer_id.clone().into());
                                o.insert("url".into(), i.url.clone().into());
                                Some(v)
                            })
                            .collect();
                        obj.insert("probes".into(), probes.into());
                    }
                    v
                })
                .unwrap_or(serde_json::Value::Null);

            if let Err(e) = kittiwake_db::record_servability_round(
                pool,
                kittiwake_db::ServabilityRound {
                    deployment_hash: &dep,
                    probed_at,
                    serving_operators: servability.effective_serving_operators as i32,
                    serving_indexers: servability.serving_indexer_count as i32,
                    gateway_verdict: None,
                    verdict_json,
                },
            )
            .await
            {
                tracing::warn!(error = %e, deployment = %dep, "could not record the probe round");
            }

            // Read back rather than appending in memory, so the history is the stored one: if the
            // write above failed, this round is legitimately absent and the verdict should be the
            // less certain one rather than one built on a row that does not exist.
            match kittiwake_db::recent_servability_rounds(
                pool,
                &dep,
                is::DEFAULT_DEAD_ROUNDS as i64,
            )
            .await
            {
                Ok(rows) if !rows.is_empty() => rows
                    .into_iter()
                    .map(|r| is::Round {
                        probed_at: r.probed_at.to_rfc3339(),
                        serving_operators: r.serving_operators.max(0) as usize,
                        serving_indexers: r.serving_indexers.max(0) as usize,
                        gateway_verdict: r.gateway_verdict.and_then(|v| {
                            serde_json::from_value(serde_json::Value::String(v)).ok()
                        }),
                    })
                    .collect(),
                Ok(_) => vec![this_round.clone()],
                Err(e) => {
                    tracing::warn!(error = %e, deployment = %dep, "could not read the round history");
                    vec![this_round.clone()]
                }
            }
        }
    };
    let rendered = is::apply_persistence(&history, is::DEFAULT_DEAD_ROUNDS);

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

// ---------------------------------------------------------------------------------------------
// The Dock
// ---------------------------------------------------------------------------------------------

pub async fn studio_auth_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let address = session(&st, &headers).ok();
    let mut r = axum::Json(serde_json::json!({"address": address})).into_response();
    crate::no_store(r.headers_mut());
    r
}

pub async fn studio_auth_post(
    State(st): State<AppState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Response, ErrorResponse> {
    let address = body
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest("Missing address, message, or signature".into()))?;
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest("Missing address, message, or signature".into()))?;
    let signature = body
        .get("signature")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest("Missing address, message, or signature".into()))?;
    kittiwake_dock::verify_sign_in(address, message, signature, now())
        .map_err(|_| ApiError::Unauthorized)?;
    let token = kittiwake_dock::create_session(&dock(&st)?.secret, address, now());
    let mut r = axum::Json(serde_json::json!({"address": address.to_lowercase()})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "studio_session={token}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
            kittiwake_dock::SESSION_TTL_SECS
        ))
        .unwrap(),
    );
    crate::no_store(r.headers_mut());
    Ok(r)
}

pub async fn studio_auth_delete() -> Response {
    let mut r = axum::Json(serde_json::json!({"ok": true})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "studio_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax",
        ),
    );
    crate::no_store(r.headers_mut());
    r
}

pub async fn studio_subgraphs_get(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let rows = kittiwake_db::list_studio_subgraphs(pool(&st)?, &owner)
        .await
        .map_err(db_err)?;
    Ok(axum::Json(serde_json::json!({"subgraphs": rows})).into_response())
}

pub async fn studio_subgraphs_post(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let slug = body
        .get("slug")
        .and_then(|v| v.as_str())
        .filter(|s| valid_slug(s))
        .ok_or_else(|| {
            ApiError::BadRequest(
                "slug must be org/name using lowercase letters, numbers, and hyphens".into(),
            )
        })?;
    if kittiwake_db::studio_subgraph_by_slug(pool(&st)?, slug)
        .await
        .map_err(db_err)?
        .is_some()
    {
        return Err(ApiError::BadRequest("Slug already taken".into()).into());
    }
    let display = body.get("displayName").and_then(|v| v.as_str());
    let row = kittiwake_db::create_studio_subgraph(pool(&st)?, &owner, slug, display)
        .await
        .map_err(db_err)?;
    Ok((
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({"subgraph": row})),
    )
        .into_response())
}

pub async fn studio_subgraph_patch(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let id = id
        .parse::<i32>()
        .map_err(|_| ApiError::BadRequest("Invalid id".into()))?;
    kittiwake_db::patch_studio_subgraph(
        pool(&st)?,
        kittiwake_db::StudioSubgraphPatch {
            id,
            owner: &owner,
            display_name: body.get("display_name").and_then(|v| v.as_str()),
            description: body.get("description").and_then(|v| v.as_str()),
            published_id: body.get("published_subgraph_id").and_then(|v| v.as_str()),
            version_label: body.get("version_label").and_then(|v| v.as_str()),
            last_deployment: body
                .get("last_published_deployment_id")
                .and_then(|v| v.as_str()),
        },
    )
    .await
    .map_err(db_err)?;
    Ok(axum::Json(serde_json::json!({"ok":true})).into_response())
}
pub async fn studio_subgraph_delete(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let id = id
        .parse::<i32>()
        .map_err(|_| ApiError::BadRequest("Invalid id".into()))?;
    kittiwake_db::delete_studio_subgraph(pool(&st)?, id, &owner)
        .await
        .map_err(db_err)?;
    Ok(axum::Json(serde_json::json!({"ok":true})).into_response())
}

/// A bounty amount: positive, and a number that exists.
///
/// The finite check is the point. `"inf"` and `"1e400"` both parse to `f64::INFINITY`, which is
/// greater than zero, and the column is text - so the value reaches Postgres intact and the board
/// query orders by `amount_grt::numeric`, where Postgres 16 accepts Infinity quite happily. One
/// signed-in caller could pin a bounty to the top of the public board for ever.
pub(crate) fn positive_amount(s: &str) -> bool {
    s.parse::<f64>().is_ok_and(|n| n.is_finite() && n > 0.0)
}

pub(crate) fn valid_slug(s: &str) -> bool {
    let mut p = s.split('/');
    let ok = |x: &str| {
        !x.is_empty()
            && x.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    p.next().is_some_and(ok) && p.next().is_some_and(ok) && p.next().is_none()
}

pub async fn studio_deploy_key_get(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let key = kittiwake_db::deploy_key(pool(&st)?, &owner)
        .await
        .map_err(db_err)?;
    Ok(axum::Json(serde_json::json!({"hasKey":key.is_some(),"createdAt":key.as_ref().map(|k|k.created_at),"lastUsedAt":key.as_ref().and_then(|k|k.last_used_at)})).into_response())
}
pub async fn studio_deploy_key_post(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let key = kittiwake_dock::generate_deploy_key();
    kittiwake_db::replace_deploy_key(pool(&st)?, &owner, &kittiwake_dock::hash_key(&key))
        .await
        .map_err(db_err)?;
    Ok(axum::Json(serde_json::json!({"key":key})).into_response())
}

pub async fn studio_metadata_post(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Response, ErrorResponse> {
    session(&st, &headers)?;
    let display = body
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let label = body
        .get("versionLabel")
        .and_then(|v| v.as_str())
        .unwrap_or("v0.0.1");
    let subgraph_doc = serde_json::json!({"description":description,"displayName":display,"image":"","codeRepository":"","website":""});
    let version_doc = serde_json::json!({"label":label,"description":""});
    let (sub, version) = tokio::try_join!(
        st.ipfs.add_json(&subgraph_doc),
        st.ipfs.add_json(&version_doc)
    )
    .map_err(|e| ApiError::Unavailable(format!("IPFS upload: {e}")))?;
    let sub_bytes = kittiwake_read::shape::ipfs_hash_to_bytes32(&sub)
        .ok_or_else(|| ApiError::Internal("IPFS returned an invalid CID".into()))?;
    let version_bytes = kittiwake_read::shape::ipfs_hash_to_bytes32(&version)
        .ok_or_else(|| ApiError::Internal("IPFS returned an invalid CID".into()))?;
    Ok(axum::Json(serde_json::json!({"subgraphMetaHash":sub,"versionMetaHash":version,"subgraphMetaBytes32":sub_bytes,"versionMetaBytes32":version_bytes})).into_response())
}

fn bearer(headers: &HeaderMap) -> Result<&str, ErrorResponse> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or(ApiError::Unauthorized.into())
}

/// The deliberately small graph-node dialect `graph deploy` speaks to.  There is no private
/// graph-node here: the deploy is published to the network by the wallet flow, and this endpoint
/// only authenticates the CLI and remembers the CID it uploaded.
pub async fn studio_node_post(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Response, ErrorResponse> {
    let owner =
        kittiwake_db::deploy_key_owner(pool(&st)?, &kittiwake_dock::hash_key(bearer(&headers)?))
            .await
            .map_err(db_err)?
            .ok_or(ApiError::Unauthorized)?;
    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
    if body.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Ok(rpc_error(-32600, "Invalid JSON-RPC request"));
    }
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(method, "subgraph_create" | "subgraph_deploy") {
        return Ok(rpc_error(-32601, &format!("Method not allowed: {method}")));
    }
    let params = body.get("params").unwrap_or(&serde_json::Value::Null);
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Ok(rpc_error(-32602, "Missing subgraph name"));
    }
    let sub = kittiwake_db::studio_subgraph_by_slug(pool(&st)?, name)
        .await
        .map_err(db_err)?;
    let Some(sub) = sub else {
        return Ok(rpc_error(
            -32602,
            &format!("Subgraph {name:?} not registered"),
        ));
    };
    if sub.owner_address != owner {
        return Ok(rpc_error(-32602, "Not your subgraph"));
    }
    if method == "subgraph_deploy" {
        if let Some(cid) = params
            .get("ipfs_hash")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            kittiwake_db::update_studio_deployment(
                pool(&st)?,
                name,
                cid,
                params.get("network").and_then(|v| v.as_str()),
            )
            .await
            .map_err(db_err)?;
        }
        let base = format!("https://thegraph.com/explorer/subgraphs/{name}");
        return Ok(axum::Json(serde_json::json!({"jsonrpc":"2.0","result":{"errors":[],"warnings":[],"playground":base,"queries":base,"subscriptions":base},"id":id})).into_response());
    }
    Ok(axum::Json(serde_json::json!({"jsonrpc":"2.0","result":null,"id":id})).into_response())
}
fn rpc_error(code: i32, message: &str) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(
            serde_json::json!({"jsonrpc":"2.0","error":{"code":code,"message":message},"id":null}),
        ),
    )
        .into_response()
}

pub async fn studio_ipfs_proxy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    body: bytes::Bytes,
) -> Result<Response, ErrorResponse> {
    kittiwake_db::deploy_key_owner(pool(&st)?, &kittiwake_dock::hash_key(bearer(&headers)?))
        .await
        .map_err(db_err)?
        .ok_or(ApiError::Unauthorized)?;
    let (status, bytes, content_type) = st
        .ipfs
        .proxy(
            reqwest::Method::from_bytes(method.as_str().as_bytes())
                .map_err(|_| ApiError::BadRequest("unsupported IPFS method".into()))?,
            &path,
            uri.query(),
            body,
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
        )
        .await
        .map_err(|e| ApiError::Unavailable(format!("IPFS: {e}")))?;
    let mut response = (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
        bytes,
    )
        .into_response();
    if let Some(ct) = content_type.and_then(|v| HeaderValue::from_str(&v).ok()) {
        response.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct BountyQuery {
    pub deployment: Option<String>,
}
pub async fn studio_bounties_get(
    State(st): State<AppState>,
    Query(q): Query<BountyQuery>,
) -> Result<Response, ErrorResponse> {
    let b = kittiwake_db::list_bounties(pool(&st)?, q.deployment.as_deref())
        .await
        .map_err(db_err)?;
    Ok(axum::Json(serde_json::json!({"bounties":b})).into_response())
}
pub async fn studio_bounties_post(
    State(st): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Response, ErrorResponse> {
    let owner = session(&st, &headers)?;
    let deployment = body
        .get("deployment_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("deployment_id and amount_grt are required".into()))?;
    let amount = body
        .get("amount_grt")
        .and_then(|v| v.as_str())
        .filter(|s| positive_amount(s))
        .ok_or_else(|| ApiError::BadRequest("amount_grt must be a positive number".into()))?;
    let subgraph_id = match body.get("slug").and_then(|v| v.as_str()) {
        Some(s) => kittiwake_db::studio_subgraph_by_slug(pool(&st)?, s)
            .await
            .map_err(db_err)?
            .filter(|x| x.owner_address == owner)
            .map(|x| x.id),
        None => None,
    };
    let expires = body
        .get("expires_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());
    let b = kittiwake_db::create_bounty(
        pool(&st)?,
        kittiwake_db::NewBounty {
            deployment,
            subgraph_id,
            developer: &owner,
            amount,
            message: body.get("message").and_then(|v| v.as_str()),
            expires_at: expires,
            chain_id: body.get("chain_bounty_id").and_then(|v| v.as_str()),
            tx: body.get("post_tx_hash").and_then(|v| v.as_str()),
        },
    )
    .await
    .map_err(db_err)?;
    Ok((
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({"bounty":b})),
    )
        .into_response())
}

fn bounty_status(state: &kittiwake_chain::ChainBounty, now: u64) -> (&'static str, Option<String>) {
    let winner = state.winner.to_lowercase();
    let won = winner != "0x0000000000000000000000000000000000000000";
    if state.settled {
        return if won {
            ("claimed", Some(winner))
        } else if state.expires_at > 0 && now > state.expires_at {
            ("expired", None)
        } else {
            ("cancelled", None)
        };
    }
    if state.expires_at > 0 && now > state.expires_at {
        ("expired", None)
    } else {
        ("open", None)
    }
}
pub async fn studio_bounty_patch(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ErrorResponse> {
    let id = id
        .parse::<i32>()
        .map_err(|_| ApiError::BadRequest("Invalid id".into()))?;
    let bounty = kittiwake_db::bounty(pool(&st)?, id)
        .await
        .map_err(db_err)?
        .ok_or(ApiError::NotFound)?;
    let chain = st
        .chain
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no Arbitrum RPC is configured".into()))?;
    let board = dock(&st)?
        .bounty_board_address
        .as_deref()
        .ok_or_else(|| ApiError::Unavailable("BountyBoard is not configured".into()))?;
    let chain_id = bounty
        .chain_bounty_id
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("Bounty has no on-chain id to reconcile".into()))?
        .parse()
        .map_err(|_| ApiError::BadRequest("Bounty has an invalid on-chain id".into()))?;
    let wanted = chain.bounty(board, chain_id).await?;
    let (status, claimed_by) = bounty_status(&wanted, now() as u64);
    if bounty.status != status || bounty.claimed_by.as_deref() != claimed_by.as_deref() {
        kittiwake_db::update_bounty_status(pool(&st)?, id, status, claimed_by.as_deref())
            .await
            .map_err(db_err)?;
    }
    Ok(
        axum::Json(serde_json::json!({"ok":true,"status":status,"claimedBy":claimed_by}))
            .into_response(),
    )
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

/// `/api/support/{number}`: one thread, with its opening post and every reply.
///
/// The archive's whole value is in the write-ups, and those are frequently in the comments rather
/// than the opening post: somebody reports a symptom and the mechanism gets worked out underneath
/// it. A page that showed only titles and labels would be an index to a thing it never rendered.
///
/// A number this mirror has never seen answers 404 rather than an empty thread. The distinction
/// matters: an issue that exists with nothing said on it, and an issue that does not exist here,
/// are different answers and should not look the same.
pub async fn support_issue(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(number): Path<i32>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    let issue = kittiwake_db::graph_support_issue(pool, number)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?
        .ok_or(ApiError::NotFound)?;

    let comments = kittiwake_db::graph_support_thread(pool, number)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    let fetched_at = kittiwake_db::graph_support_ingested_at(pool)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    let mut resp = axum::Json(serde_json::json!({
        "issue": issue,
        "comments": comments,
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

// ── The long tail: Postgres reads ────────────────────────────────────────────
//
// Four routes that read tables this service already writes. They carry no provenance envelope,
// because their source is our own Postgres rather than a sealed nest, and saying "sealed through"
// about a row a cron wrote here would be a claim nobody can check. #67

/// An Ethereum address, or nothing. Every route below is keyed by one and none of them should
/// reach Postgres with something that is not.
pub(crate) fn eth_address(raw: &str) -> Option<String> {
    let lower = raw.trim().to_lowercase();
    let hex = lower.strip_prefix("0x")?;
    (hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(lower)
}

fn bad_address() -> ErrorResponse {
    ApiError::BadRequest("expected a 0x-prefixed 20-byte address".into()).into()
}

pub async fn indexer_disputes(
    State(st): State<AppState>,
    Path(address): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = eth_address(&address).ok_or_else(bad_address)?;
    let disputes = kittiwake_db::indexer_disputes(pool(&st)?, &address)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    Ok(axum::Json(serde_json::json!({ "disputes": disputes })).into_response())
}

pub async fn parameter_history(
    State(st): State<AppState>,
    Path(address): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = eth_address(&address).ok_or_else(bad_address)?;
    let data = kittiwake_db::parameter_history(pool(&st)?, &address)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    Ok(axum::Json(serde_json::json!({ "data": data })).into_response())
}

#[derive(serde::Deserialize)]
pub struct RevenueQuery {
    pub window: Option<u32>,
    #[serde(rename = "byDeployment")]
    pub by_deployment: Option<String>,
}

async fn read_revenue(
    st: &AppState,
    address: &str,
    window_days: u32,
    by_deployment: bool,
) -> Result<
    (
        kittiwake_rewards::pnl::IndexerRevenue,
        kittiwake_types::Meta,
    ),
    ErrorResponse,
> {
    let since = kittiwake_rewards::pnl::window_start(now(), window_days);
    let read = routes::indexer_revenue(&st.read, address, since, by_deployment).await?;
    let revenue =
        kittiwake_rewards::pnl::revenue_from_nest(address, window_days, &read.entry.value);
    Ok((revenue, read.meta))
}

pub async fn indexer_revenue(
    State(st): State<AppState>,
    Path(address): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<RevenueQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = eth_address(&address).ok_or_else(bad_address)?;
    let window = kittiwake_rewards::pnl::revenue_window(q.window);
    let data = read_revenue(
        &st,
        &address,
        window,
        q.by_deployment.as_deref() == Some("1"),
    )
    .await?;
    let (data, meta) = data;
    let mut resp =
        axum::Json(serde_json::json!({ "data": data, "provenance": &meta })).into_response();
    crate::provenance_headers(resp.headers_mut(), &meta);
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// The cost side is request input and is never stored.
///
/// There is no per-indexer configuration behind this and there deliberately is not: persisting a
/// cost model keyed by an address nobody proved they own would be an unauthenticated write surface.
/// An operator's figures live in their own browser until the Dock has somewhere to put them.
pub async fn indexer_pnl(
    State(st): State<AppState>,
    Path(address): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(raw): Query<std::collections::BTreeMap<String, String>>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = eth_address(&address).ok_or_else(bad_address)?;
    let window =
        kittiwake_rewards::pnl::revenue_window(raw.get("window").and_then(|w| w.parse().ok()));
    let cost = cost_model_from(&raw);
    // The P&L view always answers in GRT; a price only adds the dollar columns.
    let grt_price = raw
        .get("grtPrice")
        .and_then(|p| p.parse::<f64>().ok())
        .filter(|p| p.is_finite() && *p >= 0.0);

    let (revenue, meta) = read_revenue(&st, &address, window, true).await?;
    let pnl = kittiwake_rewards::pnl::compute_pnl(&revenue, &cost, grt_price, window);
    let mut response = axum::Json(serde_json::json!({
        "data": {
            "pnl": pnl,
            "costModel": cost,
            "defaultChainCosts": kittiwake_rewards::pnl::default_chain_costs_map(),
        },
        "provenance": &meta,
    }))
    .into_response();
    crate::provenance_headers(response.headers_mut(), &meta);
    crate::no_store(response.headers_mut());
    Ok(response)
}

/// `?chains=a,b&cost_a=1800&overhead=300`, as the incumbent spells it.
fn cost_model_from(
    raw: &std::collections::BTreeMap<String, String>,
) -> kittiwake_rewards::pnl::CostModel {
    let chains: Vec<String> = raw
        .get("chains")
        .map(|c| {
            c.split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let overrides = raw
        .iter()
        .filter_map(|(key, value)| {
            let chain = key.strip_prefix("cost_")?;
            let price: f64 = value.parse().ok()?;
            (price.is_finite() && price >= 0.0).then(|| (chain.to_string(), price))
        })
        .collect();
    let overhead = raw
        .get("overhead")
        .and_then(|o| o.parse::<f64>().ok())
        .filter(|o| o.is_finite() && *o >= 0.0);
    kittiwake_rewards::pnl::resolve_cost_model(&chains, &overrides, overhead)
}

// ── The long tail: IPFS reads ────────────────────────────────────────────────

/// A CIDv0, or nothing. Base58 is case-sensitive, so this refuses rather than normalises.
pub(crate) fn cid_v0(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    (trimmed.len() == 46
        && trimmed.starts_with("Qm")
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() && !matches!(b, b'0' | b'O' | b'I' | b'l')))
    .then_some(trimmed)
}

/// The schema hash a manifest points at.
///
/// The manifest is YAML and the reference is `/ipfs/Qm…` under `schema:`. Parsing the whole
/// document to reach one field would be a manifest parser, which is #67's other half and a day's
/// work; finding the first CID before `dataSources` is what the incumbent does and is enough.
/// The CID of the GraphQL schema a manifest names.
///
/// Anchored on the `schema:` key wherever in the document it sits. Looking only at the text before
/// `dataSources` is the incumbent's *fallback* rather than its rule, and porting the fallback alone
/// answered 404 for every manifest with its keys in alphabetical order - which is what `graph
/// build` emits, so `dataSources` comes first and `schema:` sits a hundred lines below it. That was
/// live on `/api/subgraph-schema` until the route was probed against a real deployment.
pub(crate) fn schema_cid(manifest: &str) -> Option<&str> {
    let first_cid = |text: &str| -> Option<usize> {
        text.match_indices("Qm")
            .find(|(i, _)| text.get(*i..*i + 46).and_then(cid_v0).is_some())
            .map(|(i, _)| i)
    };

    // The key on a line of its own, which is what a real manifest has. Anchored, so the word
    // appearing inside an entity name or a file path earlier in the document cannot claim the
    // search: the incumbent's regex takes the first "schema" anywhere and would find the wrong
    // CID for a subgraph with a `schemaVersion` field above its schema.
    let anchored = manifest
        .match_indices("schema:")
        .find(|(i, _)| {
            manifest[..*i]
                .chars()
                .rev()
                .take_while(|c| *c != '\n')
                .all(char::is_whitespace)
        })
        .map(|(i, _)| i);

    for from in [anchored, manifest.find("schema")].into_iter().flatten() {
        let text = &manifest[from..];
        if let Some(i) = first_cid(text) {
            return cid_v0(&text[i..i + 46]);
        }
    }

    // Nothing named `schema` at all: the head of the document, where a hand-written manifest tends
    // to put it.
    let head = manifest.split("dataSources").next().unwrap_or(manifest);
    first_cid(head).and_then(|i| cid_v0(&head[i..i + 46]))
}

/// `GET /api/subgraph-schema/{hash}`: the GraphQL schema a deployment was built from.
///
/// Two IPFS reads, both content-addressed and therefore cached for ever rather than for a while.
pub async fn subgraph_schema(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let deployment = cid_v0(&hash)
        .ok_or_else(|| ErrorResponse::from(ApiError::BadRequest("expected a CIDv0 hash".into())))?;
    let pool = pool(&st)?;

    let manifest = kittiwake_ext::read_through_text(pool, &st.ipfs, deployment)
        .await
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::Unavailable(
                "that manifest could not be read from IPFS".into(),
            ))
        })?;
    // A hash that resolves to something that is not a manifest is the caller's mistake, and saying
    // so beats reporting that no schema was found in a document that never had one.
    if !manifest.contains("dataSources") && !manifest.contains("specVersion") {
        return Err(ApiError::BadRequest("that hash is not a subgraph manifest".into()).into());
    }
    let schema_cid = schema_cid(&manifest).ok_or(ApiError::NotFound)?;
    let schema_text = kittiwake_ext::read_through_text(pool, &st.ipfs, schema_cid)
        .await
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::Unavailable(
                "the manifest names a schema that could not be read from IPFS".into(),
            ))
        })?;

    Ok(axum::Json(serde_json::json!({
        "data": { "schemaText": schema_text, "schemaHash": schema_cid }
    }))
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct DipsAgreementsQuery {
    pub indexer: Option<String>,
    /// Which chain's lifecycle to read. A caller's choice here rather than an environment
    /// variable, unlike the incumbent, because the two nests are both configured and a testnet
    /// answer that arrives labelled as one is not a lie.
    pub sepolia: Option<String>,
}

/// `GET /api/dips/agreements`: the DIPS agreement lifecycle.
pub async fn dips_agreements(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DipsAgreementsQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let indexer = match q.indexer.as_deref() {
        Some(raw) => Some(eth_address(raw).ok_or_else(bad_address)?),
        None => None,
    };
    let mut read = routes::dips_agreements(&st.read, q.sepolia.as_deref() == Some("1")).await?;
    if let Some(indexer) = indexer {
        // After the cache, so one fold serves every caller and a per-indexer view costs the nest
        // nothing. A *new* entry rather than a mutation: `read.entry` is the shared cached one, and
        // narrowing it in place would hand the next caller this indexer's filtered answer.
        read.entry = std::sync::Arc::new(kittiwake_cache::Entry {
            value: std::sync::Arc::new(kittiwake_read::dips_agreements::narrow(
                &read.entry.value,
                &indexer,
            )),
            provenance: read.entry.provenance.clone(),
            fetched_at: read.entry.fetched_at,
        });
    }
    Ok(envelope(read))
}

/// `GET /api/subgraph-versions/{hash}`: every version of the subgraph that published a deployment.
///
/// The nest half is cached; the labels are filled here from the IPFS document cache in one bulk
/// read. The incumbent fetched each version's metadata document separately, which for a subgraph
/// with forty versions is forty round trips to IPFS behind one request.
pub async fn subgraph_versions(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let cid = cid_v0(&hash)
        .ok_or_else(|| ErrorResponse::from(ApiError::BadRequest("expected a CIDv0 hash".into())))?;
    let deployment = kittiwake_read::shape::ipfs_hash_to_bytes32(cid)
        .ok_or_else(|| ErrorResponse::from(ApiError::BadRequest("expected a CIDv0 hash".into())))?;

    let read = routes::subgraph_versions(&st.read, &deployment).await?;
    let mut value = (*read.entry.value).clone();
    if let Some(pool) = st.pool.as_ref() {
        label_versions(pool, &mut value).await;
    }
    // A new entry rather than a mutation: `read.entry` is shared, and writing labels into it would
    // be writing into the cache from a request.
    let read = kittiwake_read::Read {
        entry: std::sync::Arc::new(kittiwake_cache::Entry {
            value: std::sync::Arc::new(value),
            provenance: read.entry.provenance.clone(),
            fetched_at: read.entry.fetched_at,
        }),
        meta: read.meta,
    };
    Ok(envelope(read))
}

/// Replace each version's `versionMetadata` with the `label` from its IPFS document.
///
/// A document the cache has not seen leaves the label null rather than blocking the response on a
/// fetch: the warm job is what fills that cache, and a version list is more useful with a missing
/// label than it is late.
async fn label_versions(pool: &kittiwake_db::Pool, value: &mut serde_json::Value) {
    let Some(versions) = value.get_mut("versions").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let cids: Vec<String> = versions
        .iter()
        .filter_map(|v| {
            kittiwake_read::search::metadata_cid(v.get("versionMetadata").and_then(|m| m.as_str()))
        })
        .collect();
    let docs = kittiwake_ext::cached_docs(pool, &cids)
        .await
        .unwrap_or_default();

    for version in versions.iter_mut() {
        let label = kittiwake_read::search::metadata_cid(
            version.get("versionMetadata").and_then(|m| m.as_str()),
        )
        .and_then(|cid| docs.iter().find(|d| d.cid == cid))
        .and_then(|doc| doc.json.as_ref())
        .and_then(|json| json.get("label"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
        if let Some(object) = version.as_object_mut() {
            object.remove("versionMetadata");
            object.insert("label".into(), label);
        }
    }
}

#[derive(serde::Deserialize)]
pub struct EnsQuery {
    pub address: Option<String>,
}

/// `GET /api/ens?address=0x…`: the address's primary ENS name, or null.
///
/// Cached a day per address, which is the incumbent's key and cadence: a primary name changes
/// rarely and a mainnet round trip on every avatar render would not be worth it.
pub async fn ens(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<EnsQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let raw = q
        .address
        .as_deref()
        .ok_or_else(|| ErrorResponse::from(ApiError::BadRequest("address required".into())))?;
    // Anything that is not an address has no name, and saying so is cheaper and truer than an
    // error: the caller asked about something ENS has never heard of.
    let Some(address) = eth_address(raw) else {
        return Ok(axum::Json(serde_json::json!({ "ensName": null })).into_response());
    };
    let chain = st
        .read
        .chain
        .as_ref()
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::Unavailable(
                "no mainnet RPC is configured, so ENS cannot be read".into(),
            ))
        })?
        .clone();

    let read = st
        .read
        .cached_external_keyed(
            "ens",
            &address,
            std::time::Duration::from_secs(86_400),
            kittiwake_types::ServedFrom::Chain,
            {
                let address = address.clone();
                std::sync::Arc::new(move || {
                    let chain = chain.clone();
                    let address = address.clone();
                    Box::pin(async move {
                        Ok(serde_json::json!({ "ensName": chain.ens_name(&address).await? }))
                    })
                })
            },
        )
        .await?;
    Ok(envelope_root(read))
}

#[derive(serde::Deserialize)]
pub struct ManifestQuery {
    pub hash: Option<String>,
}

/// `GET /api/manifest?hash=Qm…`: how hard a subgraph is to sync, and why.
///
/// The manifest is content-addressed, so the read goes through the document cache and the scoring
/// is pure: two callers asking about the same deployment cost one IPFS fetch between them, ever.
pub async fn manifest(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ManifestQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let cid = q
        .hash
        .as_deref()
        .and_then(cid_v0)
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::BadRequest(
                "hash must be a CIDv0 deployment id".into(),
            ))
        })?
        .to_string();

    let yaml = kittiwake_ext::read_through_text(pool(&st)?, &st.ipfs, &cid)
        .await
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::Unavailable(
                "that manifest could not be read from IPFS".into(),
            ))
        })?;
    // A hash that resolves to something else is the caller's mistake, and saying so beats scoring
    // an arbitrary document as though it were a subgraph.
    if !yaml.contains("dataSources") && !yaml.contains("specVersion") {
        return Err(ApiError::BadRequest("that hash is not a subgraph manifest".into()).into());
    }
    let analysis = kittiwake_read::manifest::analyse(&yaml)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(axum::Json(serde_json::json!({ "data": analysis })).into_response())
}

/// `/api/support/templates`: the six issue forms, as the repository's own YAML.
///
/// Served so the dashboard's compose form is generated from the templates rather than from a copy
/// of them. A copy is correct exactly until somebody edits a template, and nothing would say so.
pub async fn support_templates(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    let templates = kittiwake_db::graph_support_templates(pool)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    // Empty means the mirror has not run rather than that the repository has no templates, and a
    // compose page rendering no options would look like a decision instead of a fault.
    if templates.is_empty() {
        return Err(
            ApiError::Unavailable("the graph-support mirror has not run yet".into()).into(),
        );
    }

    let mut resp = axum::Json(serde_json::json!({ "templates": templates })).into_response();
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-kittiwake-served-from"),
        axum::http::HeaderValue::from_static("postgres"),
    );
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

// ── Foghorn ──────────────────────────────────────────────────────────────────

/// A path segment we are willing to paste into the upstream URL.
///
/// The segments are joined into a URL and the HTTP client normalises `..` before sending, so an
/// encoded `%2e%2e` would walk back out of the `/v1/` prefix and reach anything else the internal
/// service exposes. Refusing the segment is cheaper than trusting the normalisation.
pub(crate) fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Forward Foghorn's answer, keeping its status even when the body is not JSON.
///
/// The parse used to sit inside the error path, so an unknown path - which Foghorn answers with a
/// bodyless 404 - threw and was reported as the service being unreachable. An indexer chased that
/// as an outage on our side on 2026-08-15. A 404 is an answer; only failing to reach the service
/// is a 502.
pub(crate) fn foghorn_answer(status: u16, body: String) -> Response {
    let code =
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    let mut response = match serde_json::from_str::<serde_json::Value>(&body) {
        _ if body.is_empty() => axum::Json(serde_json::json!({
            "error": if (200..300).contains(&status) {
                "Foghorn returned an empty response".to_string()
            } else {
                format!("Foghorn returned {status}")
            }
        }))
        .into_response(),
        Ok(json) => axum::Json(json).into_response(),
        Err(_) => axum::Json(serde_json::json!({
            "error": format!("Foghorn returned a non-JSON {status} response")
        }))
        .into_response(),
    };
    *response.status_mut() = code;
    crate::no_store(response.headers_mut());
    response
}

async fn foghorn_call(
    st: &AppState,
    path: &str,
    query: Option<String>,
    body: Option<String>,
) -> Result<Response, ErrorResponse> {
    let base = st.foghorn_api_url.as_deref().ok_or_else(|| {
        ErrorResponse::from(ApiError::Unavailable(
            "the Foghorn API is not configured".into(),
        ))
    })?;
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    let target = format!("{}/v1/{path}{query}", base.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let request = match &body {
        Some(body) => client
            .post(&target)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(body.clone()),
        None => client.get(&target),
    };
    let upstream = request
        .header(axum::http::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| ErrorResponse::from(ApiError::Unavailable("Foghorn is unreachable".into())))?;
    let status = upstream.status().as_u16();
    let text = upstream.text().await.unwrap_or_default();
    Ok(foghorn_answer(status, text))
}

/// `GET /api/foghorn/{*path}`: the QoS pipeline, proxied.
pub async fn foghorn_get(
    State(st): State<AppState>,
    Path(path): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    if !path.split('/').all(safe_segment) {
        return Err(ApiError::BadRequest("invalid path".into()).into());
    }
    foghorn_call(&st, &path, query, None).await
}

/// `POST /api/foghorn/qos/graphql`, and nothing else.
///
/// Deliberately narrow. GraphQL is the only POST surface Foghorn exposes, and forwarding arbitrary
/// POSTs to an internal service is not a thing to do by accident.
pub async fn foghorn_post(
    State(st): State<AppState>,
    Path(path): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    if path != "qos/graphql" {
        return Err(ApiError::BadRequest("not a POST endpoint".into()).into());
    }
    foghorn_call(&st, &path, query, Some(body)).await
}

// ── Presenting a proof of indexing ───────────────────────────────────────────

/// An allocation id, in whatever case it was written.
///
/// Deliberately not normalised: the value is interpolated into a GraphQL mutation and sent to
/// somebody's indexer agent, so it goes as the caller wrote it. What matters is that it cannot
/// contain a quote.
pub(crate) fn allocation_id(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix("0x")?;
    (hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(trimmed)
}

/// Which credential goes to which agent.
///
/// **The server's token never travels to a caller-supplied URL.** The SSRF check rejects private
/// addresses; it does not reject an attacker's perfectly public one, so falling back to our own
/// credential for a nominated host would hand this box's management authority to whoever asked.
/// A caller pointing at their own agent brings their own token.
pub(crate) fn agent_token<'a>(
    caller_url: Option<&str>,
    caller_token: Option<&'a str>,
    server_token: Option<&'a str>,
) -> Option<&'a str> {
    match caller_url {
        Some(_) => caller_token,
        None => caller_token.or(server_token),
    }
}

#[derive(serde::Deserialize)]
pub struct PresentPoi {
    #[serde(default)]
    pub deployment_id: String,
    #[serde(default)]
    pub allocation_id: String,
    pub agent_url: Option<String>,
    pub agent_token: Option<String>,
}

/// `POST /api/indexer/present-poi`: queue a `presentPOI` action on an indexer agent.
///
/// Both identifiers are interpolated into the mutation below, so both are validated to character
/// sets that cannot contain a quote. Anything looser and a caller could close the string and queue
/// actions of their own choosing on somebody's agent.
pub async fn present_poi(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<PresentPoi>,
) -> Result<Response, ErrorResponse> {
    let caller = client_ip(peer, &headers, st.edge_secret.as_deref());
    st.limits.check(&st.limits.mutation, caller)?;

    let (Some(deployment), Some(allocation)) = (
        cid_v0(&body.deployment_id),
        allocation_id(&body.allocation_id),
    ) else {
        return Err(ApiError::BadRequest(
            "deploymentId must be an IPFS CIDv0 and allocationId a 0x-prefixed 20-byte address"
                .into(),
        )
        .into());
    };

    let agent = st.indexer_agent.as_ref();
    let target = match body.agent_url.as_deref() {
        Some(url) => {
            kittiwake_ssrf::check_resolved(url).await.map_err(|_| {
                ErrorResponse::from(ApiError::BadRequest(
                    "agentUrl must be a public http or https URL".into(),
                ))
            })?;
            url.to_string()
        }
        None => agent.map(|a| a.url.to_string()).ok_or_else(|| {
            ErrorResponse::from(ApiError::Unavailable(
                "no indexer agent is configured here and none was given".into(),
            ))
        })?,
    };
    let token = agent_token(
        body.agent_url.as_deref(),
        body.agent_token.as_deref(),
        agent.and_then(|a| a.token.as_deref()),
    );

    let mutation = format!(
        r#"mutation {{
    queueActions(actions: [{{
      type: presentPOI,
      deploymentID: "{deployment}",
      allocationID: "{allocation}",
      protocolNetwork: "eip155:42161",
      status: approved,
      priority: 0,
      isLegacy: false,
      source: "lodestar-dashboard",
      reason: "bounty claim"
    }}]) {{ id type status failureReason }}
  }}"#
    );

    let client = reqwest::Client::new();
    let mut request = client
        .post(&target)
        .timeout(std::time::Duration::from_secs(10))
        .json(&serde_json::json!({ "query": mutation }));
    if let Some(token) = token {
        use base64::Engine;
        request = request.header(
            axum::http::header::AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(token)
            ),
        );
    }

    let upstream = request.send().await.map_err(|e| {
        ErrorResponse::from(ApiError::Unavailable(format!(
            "could not reach the indexer agent: {e}"
        )))
    })?;
    let answer: serde_json::Value = upstream.json().await.map_err(|e| {
        ErrorResponse::from(ApiError::Unavailable(format!(
            "the indexer agent answered unreadably: {e}"
        )))
    })?;
    let mut response = axum::Json(answer).into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

// ── The data-service playground ──────────────────────────────────────────────

/// How one provider is asked for a sample answer.
///
/// Server-authoritative: the caller sends a slug and nothing else. Every URL below is a constant,
/// so no part of a request reaches a hostname, and the SSRF question does not arise.
enum Sample {
    /// Paid through the Lodestar gateway, which signs on our behalf; only the consumer is named.
    JsonRpc {
        url: &'static str,
        consumer: &'static str,
        body: &'static str,
    },
    /// Gated by a TAP receipt we sign per request with an ephemeral key.
    RestTap {
        url: &'static str,
        data_service: &'static str,
        service_provider: &'static str,
    },
    /// Substreams is gRPC; a shim on the provider's box runs the CLI and answers JSON.
    Shim { url: &'static str },
}

fn sample_for(slug: &str) -> Option<Sample> {
    Some(match slug {
        "dispatch" => Sample::JsonRpc {
            url: "https://rpc.cargopete.com/rpc/42161",
            consumer: "0xB70781305939A39e74Aa918416Df1b893e1Bd904",
            body: r#"{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}"#,
        },
        "camp-data-service" => Sample::RestTap {
            url: "https://camp.89.167.109.4.sslip.io/v1/status",
            data_service: "0x8ED612666ad1853AdB050f4c4c54082decA605b8",
            service_provider: "0xCfBB471617Ae3F6fDa2D9F142115d59deAB50C5b",
        },
        "seahorn" => Sample::RestTap {
            url: "https://seahorn.89.167.109.4.sslip.io/buys?limit=3&order=slot.desc",
            data_service: "0xdDE3F913cb6D1332Bc018Eb63647020a87dD7B37",
            service_provider: "0xCfBB471617Ae3F6fDa2D9F142115d59deAB50C5b",
        },
        "sdsce" => Sample::Shim {
            url: "https://substreams.89.167.109.4.sslip.io/sample",
        },
        "mainline-firehose" => Sample::Shim {
            url: "https://mainline.89.167.109.4.sslip.io/sample",
        },
        _ => return None,
    })
}

/// The slugs this playground can answer for, for the error that says so.
pub(crate) const SAMPLE_SLUGS: &[&str] = &[
    "dispatch",
    "camp-data-service",
    "seahorn",
    "sdsce",
    "mainline-firehose",
];

#[derive(serde::Deserialize)]
pub struct SampleRequest {
    #[serde(default)]
    pub slug: String,
}

/// `POST /api/data-services/query`: run one real sample query against a provider.
///
/// Server-side so the browser is spared CORS and so a receipt can be signed without a key ever
/// reaching a client.
pub async fn data_service_query(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<SampleRequest>,
) -> Result<Response, ErrorResponse> {
    let caller = client_ip(peer, &headers, st.edge_secret.as_deref());
    st.limits.check(&st.limits.mutation, caller)?;

    let Some(sample) = sample_for(&body.slug) else {
        return Err(ApiError::BadRequest(format!(
            "no live playground for {:?}; try one of: {}",
            body.slug,
            SAMPLE_SLUGS.join(", ")
        ))
        .into());
    };

    // Substreams goes through a shim that runs a CLI, which is slow by nature; the rest should not
    // be given that long to fail.
    let timeout = match sample {
        Sample::Shim { .. } => std::time::Duration::from_secs(50),
        _ => std::time::Duration::from_secs(15),
    };
    let client = reqwest::Client::new();
    let started = std::time::Instant::now();

    let (url, request) = match sample {
        Sample::JsonRpc {
            url,
            consumer,
            body,
        } => (
            url,
            client
                .post(url)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .header("X-Consumer-Address", consumer)
                .body(body),
        ),
        Sample::Shim { url } => (url, client.get(url)),
        Sample::RestTap {
            url,
            data_service,
            service_provider,
        } => {
            let receipt = sample_receipt(data_service, service_provider)
                .map_err(|e| ApiError::Internal(format!("could not sign a receipt: {e}")))?;
            (url, client.get(url).header("TAP-Receipt", receipt))
        }
    };

    let outcome = request.timeout(timeout).send().await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let upstream = match outcome {
        Ok(upstream) => upstream,
        Err(e) => {
            let mut response = axum::Json(serde_json::json!({
                "ok": false,
                "error": if e.is_timeout() { "request timed out".to_string() } else { e.to_string() },
                "durationMs": duration_ms,
            }))
            .into_response();
            *response.status_mut() = axum::http::StatusCode::BAD_GATEWAY;
            crate::no_store(response.headers_mut());
            return Ok(response);
        }
    };

    let status = upstream.status().as_u16();
    let ok = upstream.status().is_success();
    let text = upstream.text().await.unwrap_or_default();
    // A provider that answers with prose rather than JSON is still answering; the body goes back as
    // whatever it was rather than becoming an error about parsing.
    let result = serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|_| serde_json::Value::String(text));

    let mut response = axum::Json(serde_json::json!({
        "ok": ok,
        "status": status,
        "durationMs": duration_ms,
        "endpoint": url,
        "result": result,
    }))
    .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

fn sample_receipt(data_service: &str, service_provider: &str) -> Result<String, String> {
    let parse = |s: &str| s.parse().map_err(|_| format!("{s} is not an address"));
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos() as u64;
    let signed = kittiwake_chain::tap::sign_receipt(
        parse(data_service)?,
        parse(service_provider)?,
        now_ns,
        rand_nonce(),
    )?;
    serde_json::to_string(&signed).map_err(|e| e.to_string())
}

/// A nonce that does not repeat within a run. It only has to be unique per receipt.
fn rand_nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

// ── Quality capture ──────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct FoghornIndexer {
    indexer_address: String,
    #[serde(default)]
    composite: Option<f64>,
    #[serde(default)]
    rated: bool,
}

#[derive(serde::Deserialize)]
struct FoghornIndexers {
    #[serde(default)]
    indexers: Vec<FoghornIndexer>,
}

/// `GET /api/qos/capture`: how allocated stake sits across quality bands.
///
/// The unscored band is large and that is the honest state of our coverage: the Lodestar Oracle
/// grades only what it has probed, so most allocated GRT lands in "not measured" rather than being
/// filled in from somebody else's telemetry.
pub async fn qos_capture(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = pool(&st)?.clone();
    let base = st
        .foghorn_api_url
        .as_deref()
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::Unavailable(
                "the Foghorn API is not configured, so nothing can be scored".into(),
            ))
        })?
        .trim_end_matches('/')
        .to_string();

    let read = st
        .read
        .cached_external(
            "qos-capture",
            std::time::Duration::from_secs(900),
            kittiwake_types::ServedFrom::Postgres,
            std::sync::Arc::new(move || {
                let pool = pool.clone();
                let base = base.clone();
                Box::pin(async move {
                    let scores = reqwest::Client::new()
                        .get(format!("{base}/v1/indexers?limit=500"))
                        .header(axum::http::header::ACCEPT, "application/json")
                        .timeout(std::time::Duration::from_secs(15))
                        .send()
                        .await
                        .map_err(|e| ApiError::Unavailable(format!("Foghorn: {e}")))?;
                    if !scores.status().is_success() {
                        return Err(ApiError::Unavailable(format!(
                            "Foghorn returned {}",
                            scores.status()
                        )));
                    }
                    let scores: FoghornIndexers = scores
                        .json()
                        .await
                        .map_err(|e| ApiError::Unavailable(format!("Foghorn: {e}")))?;
                    let allocations = kittiwake_db::allocating_indexers(&pool)
                        .await
                        .map_err(|e| ApiError::Unavailable(e.to_string()))?;

                    // Only a *rated* indexer carries a score. An unrated one is not a zero, it is
                    // one we have not measured, and collapsing those is the exact failure this
                    // whole view exists to complain about.
                    let scored: std::collections::BTreeMap<String, Option<f64>> = scores
                        .indexers
                        .into_iter()
                        .map(|i| {
                            (
                                i.indexer_address.to_lowercase(),
                                i.rated.then_some(i.composite).flatten(),
                            )
                        })
                        .collect();

                    let rows: Vec<kittiwake_read::concentration::Allocation> = allocations
                        .iter()
                        .map(|(address, grt)| kittiwake_read::concentration::Allocation {
                            allocated_grt: *grt,
                            q_score: scored.get(&address.to_lowercase()).copied().flatten(),
                        })
                        .collect();
                    let measured = rows.iter().filter(|r| r.q_score.is_some()).count();

                    Ok(serde_json::json!({
                        "concentration": kittiwake_read::concentration::compute(&rows, 6),
                        "coverage": {
                            "allocated_indexers": rows.len(),
                            "measured_indexers": measured,
                            "note": "Scored by the Lodestar Oracle, which grades only what it has \
                                     probed. Unscored means not measured, never measured and found \
                                     wanting.",
                        }
                    }))
                })
            }),
        )
        .await?;
    Ok(envelope(read))
}

// ── The service census ───────────────────────────────────────────────────────

use kittiwake_read::census;

/// Call one provider's advertised endpoint and read the answer.
///
/// **The endpoint is a string somebody put on chain**, so it goes through the SSRF guard before it
/// goes anywhere near a socket. The incumbent fetched it unchecked, which made a public route into
/// an SSRF with a status code and a latency for an oracle; see nightswatchhq/lodestar#149.
async fn probe_provider(provider: &census::Provider, kind: census::Probe) -> census::ProviderProbe {
    let mut out = census::ProviderProbe {
        address: provider.address.clone(),
        endpoint: provider.endpoint.clone(),
        verdict: census::Verdict::NoEndpoint,
        http_status: None,
        latency_ms: None,
    };
    if provider.endpoint.is_empty() {
        return out;
    }
    if kittiwake_ssrf::check_resolved(&provider.endpoint)
        .await
        .is_err()
    {
        out.verdict = census::Verdict::Refused;
        return out;
    }

    let started = std::time::Instant::now();
    let answer = reqwest::Client::new()
        .get(&provider.endpoint)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await;
    out.latency_ms = Some(started.elapsed().as_millis() as u64);
    match answer {
        Ok(response) => {
            let status = response.status().as_u16();
            out.http_status = Some(status);
            out.verdict = census::verdict_of(status, kind);
        }
        Err(e) if e.is_timeout() => {
            out.verdict = census::Verdict::Timeout;
            out.http_status = None;
        }
        Err(_) => {
            out.verdict = census::Verdict::Unreachable;
            out.http_status = None;
        }
    }
    out
}

/// `GET /api/service-census`: registry versus reality across every data service.
///
/// One service failing does not fail the census. A census returning nothing because one RPC call
/// timed out tells a reader less than one saying which service it could not read.
pub async fn service_census(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let chain = st.read.chain.clone().ok_or_else(|| {
        ErrorResponse::from(ApiError::Unavailable(
            "no Arbitrum RPC is configured, so no registry can be read".into(),
        ))
    })?;

    let read = st
        .read
        .cached_external(
            "service-census",
            std::time::Duration::from_secs(300),
            kittiwake_types::ServedFrom::Chain,
            std::sync::Arc::new(move || {
                let chain = chain.clone();
                Box::pin(async move {
                    let mut services = Vec::new();
                    for service in census::SERVICES {
                        services.push(match chain.provider_registry(service.address).await {
                            Ok(logs) => {
                                let events: Vec<census::RegistryEvent> = logs
                                    .into_iter()
                                    .map(|l| census::RegistryEvent {
                                        provider: l.provider,
                                        endpoint: l.endpoint,
                                        block_number: l.block_number,
                                        log_index: l.log_index,
                                        registered: l.registered,
                                    })
                                    .collect();
                                let mut probes = Vec::new();
                                for provider in census::current_providers(&events) {
                                    probes.push(probe_provider(&provider, service.probe).await);
                                }
                                let requirements =
                                    chain.provision_ranges(service.address).await.map(|r| {
                                        census::interpret(
                                            r.min_tokens,
                                            r.max_tokens,
                                            r.min_thawing,
                                            r.max_thawing,
                                            r.min_cut_ppm,
                                            r.max_cut_ppm,
                                        )
                                    });
                                census::summarise(service, probes, requirements)
                            }
                            // Named rather than reported as zero providers, so nobody reads "we
                            // could not look" as "there is nobody there".
                            Err(e) => census::ServiceCensus {
                                id: service.id,
                                name: service.name,
                                address: service.address,
                                registered: 0,
                                serving: 0,
                                lying: 0,
                                providers: Vec::new(),
                                requirements: None,
                                error: Some(e.to_string()),
                            },
                        });
                    }

                    let benchmark =
                        chain
                            .provision_ranges(census::SUBGRAPH_SERVICE)
                            .await
                            .map(|r| {
                                census::interpret(
                                    r.min_tokens,
                                    r.max_tokens,
                                    r.min_thawing,
                                    r.max_thawing,
                                    r.min_cut_ppm,
                                    r.max_cut_ppm,
                                )
                            });
                    Ok(serde_json::json!({
                        "headline": census::headline(&services),
                        "benchmark": benchmark,
                        "services": services,
                    }))
                })
            }),
        )
        .await?;
    Ok(envelope(read))
}

#[derive(serde::Deserialize)]
pub struct PreflightQuery {
    pub address: Option<String>,
    pub service: Option<String>,
}

/// `GET /api/operator-preflight?address=0x…&service=…`: what would happen if this address tried to
/// run this service.
///
/// Read-only by construction: no wallet, no signature, no gas. That is the point rather than a
/// limitation, because it lets somebody price the job before deciding they care, for an address
/// they have not funded or do not control.
pub async fn operator_preflight(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<PreflightQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let address = q.address.as_deref().and_then(eth_address).ok_or_else(|| {
        ErrorResponse::from(ApiError::BadRequest(
            "give an address= that is a valid address".into(),
        ))
    })?;
    let wanted = q.service.as_deref().unwrap_or_default();
    let service = census::SERVICES
        .iter()
        .find(|s| s.id == wanted)
        .ok_or_else(|| {
            let names: Vec<&str> = census::SERVICES.iter().map(|s| s.id).collect();
            ErrorResponse::from(ApiError::BadRequest(format!(
                "unknown service; one of: {}",
                names.join(", ")
            )))
        })?;

    let chain = st.read.chain.as_ref().ok_or_else(|| {
        ErrorResponse::from(ApiError::Unavailable(
            "no Arbitrum RPC is configured, so nothing can be checked".into(),
        ))
    })?;

    let reads = chain.preflight_reads(&address, service.address).await?;
    let requirements = chain.provision_ranges(service.address).await.map(|r| {
        census::interpret(
            r.min_tokens,
            r.max_tokens,
            r.min_thawing,
            r.max_thawing,
            r.min_cut_ppm,
            r.max_cut_ppm,
        )
    });
    // Whether the registry lists this address. Read here rather than taken from the census cache,
    // so a preflight is never answered from a picture of the registry that has since moved.
    let registered = match chain.provider_registry(service.address).await {
        Ok(logs) => {
            let events: Vec<census::RegistryEvent> = logs
                .into_iter()
                .map(|l| census::RegistryEvent {
                    provider: l.provider,
                    endpoint: l.endpoint,
                    block_number: l.block_number,
                    log_index: l.log_index,
                    registered: l.registered,
                })
                .collect();
            census::current_providers(&events)
                .iter()
                .any(|p| p.address == address)
        }
        Err(_) => false,
    };

    let steps = kittiwake_read::preflight::preflight(&kittiwake_read::preflight::Reads {
        grt_balance: reads.grt_balance,
        stake: reads.stake,
        idle_stake: reads.idle_stake,
        provision_tokens: reads.provision_tokens,
        provision_thawing_seconds: reads.provision_thawing_seconds,
        registered,
        service_implementation: kittiwake_read::preflight::implementation_from_slot(Some(
            &reads.implementation_slot,
        )),
        requirements,
    });

    let mut response = axum::Json(serde_json::json!({
        "data": {
            "address": address,
            "service": {
                "id": service.id,
                "name": service.name,
                "address": service.address,
            },
            "verdict": kittiwake_read::preflight::verdict(&steps),
            "steps": steps,
        }
    }))
    .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

/// The four sliders and how many candidates to return. Taken as strings rather than numbers so a
/// mistyped value falls back to the default instead of failing the whole request: the incumbent
/// runs every one of these through `Number()`, which never throws.
#[derive(serde::Deserialize)]
pub struct RecommendQuery {
    pub returns: Option<String>,
    pub stability: Option<String>,
    pub safety: Option<String>,
    pub network: Option<String>,
    pub count: Option<String>,
}

/// A slider, defaulting to neutral. `Number("")` is 0 in JavaScript and `Number("x")` is NaN;
/// both become 5 here, because a typo should not silently switch a dimension off.
fn slider(value: Option<&String>) -> f64 {
    value
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(5.0)
        .clamp(0.0, 10.0)
}

/// `GET /api/delegate/recommend?returns=..&stability=..&safety=..&network=..&count=..`: which
/// indexer to delegate to.
///
/// Answers in the **frontend's** field names rather than ours. `/delegate` reads `indexer.id` and
/// `indexer.delegationCapacity.utilizationPercent` straight off this response without putting it
/// through `enriched-normalise.ts` first, so serving our own row shape would blank the page the
/// way the cutover did. `kittiwake_read::recommend::legacy_indexer` is a port of that normaliser
/// and is pinned against its output by test.
///
/// The incumbent sends `s-maxage=60, stale-while-revalidate=300`; this sends `no-store` like every
/// other data route here, because Vercel's CDN caches a rewritten origin by its upstream header
/// and a five-minute-stale recommendation is not ours to give away.
pub async fn delegate_recommend(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<RecommendQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    // A hundred, because that is `/api/indexers-enriched`'s default page size and the incumbent
    // ranks over exactly what that route hands it. Reading more here would recommend indexers the
    // page it came from never showed.
    let rows = kittiwake_db::enriched_indexers(pool, 100)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    // The incumbent's own sentences, kept verbatim: they are what a delegator sees in the console
    // when the page says it could not load a recommendation.
    if rows.is_empty() {
        return Ok(bare_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Indexer data not yet available",
        ));
    }

    let eligible: Vec<&kittiwake_db::EnrichedIndexer> = rows
        .iter()
        .filter(|r| kittiwake_read::recommend::eligible(r))
        .collect();
    if eligible.is_empty() {
        return Ok(bare_error(
            axum::http::StatusCode::NOT_FOUND,
            "No eligible indexers found",
        ));
    }

    let prefs = kittiwake_read::recommend::Prefs {
        returns: slider(q.returns.as_ref()),
        stability: slider(q.stability.as_ref()),
        safety: slider(q.safety.as_ref()),
        network: slider(q.network.as_ref()),
    };
    let weights = kittiwake_read::recommend::weights(&prefs);

    // `None` for every row: the refresh job computes a per-dimension breakdown and the `indexers`
    // table has nowhere to keep it, so the sliders cannot move anything yet and the ranking is the
    // composite score. `reasons` says so rather than implying otherwise. #87
    let mut ranked: Vec<(&kittiwake_db::EnrichedIndexer, f64)> = eligible
        .iter()
        .map(|r| (*r, kittiwake_read::recommend::rank_score(r, None, &weights)))
        .collect();
    // Stable, so indexers on the same score keep the database's order: score first, then self
    // stake. An unstable sort would reshuffle the top of the board between two identical requests.
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    let candidate = |(row, score): &(&kittiwake_db::EnrichedIndexer, f64)| {
        serde_json::json!({
            "indexer": kittiwake_read::recommend::legacy_indexer(row),
            "score": score.round() as i64,
            "reasons": kittiwake_read::recommend::reasons(row, None, &weights),
        })
    };

    // `count` absent means one, and the answer is then the single recommendation rather than a
    // list of one. The incumbent turns an unparseable count into `NaN`, which slices to an empty
    // candidate list; a typo answering with nothing is a bug, so it reads as one here.
    let count = q
        .count
        .as_deref()
        .map(|c| {
            c.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map_or(1.0, |v| v.clamp(1.0, 10.0)) as usize
        })
        .unwrap_or(1);

    let body = if count == 1 {
        candidate(&ranked[0])
    } else {
        serde_json::json!({
            "candidates": ranked.iter().take(count).map(candidate).collect::<Vec<_>>(),
        })
    };

    let mut resp = axum::Json(body).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// The incumbent's error body: a sentence under `error`, and nothing else. Kept rather than
/// translated into this service's `{error, message}` because the sentence is the whole content.
fn bare_error(status: axum::http::StatusCode, message: &str) -> Response {
    let mut resp = (status, axum::Json(serde_json::json!({ "error": message }))).into_response();
    crate::no_store(resp.headers_mut());
    resp
}

#[derive(serde::Deserialize)]
pub struct DisassemblyQuery {
    pub id: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct DisassemblyDiffQuery {
    pub a: Option<String>,
    pub b: Option<String>,
}

/// `GET /api/disassembly?id=Qm…`: what a deployed mapping can actually do.
///
/// The manifest and every module it names are fetched from IPFS and decoded; nothing is executed,
/// which is what makes the answer a pure function of a content-addressed id and what lets it be
/// kept for ever rather than for seven days. `kittiwake_read::disassembly::ANALYSIS_VERSION` is
/// what expires it, and only when the analysis itself changes.
pub async fn disassembly(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DisassemblyQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let id = deployment_cid(q.id.as_deref())?;

    let report = kittiwake_read::disassembly::report(st.pool.as_ref(), &st.ipfs, &id).await?;
    let signal = kittiwake_read::disassembly::signal(&st.read.gate, &id).await;

    // Best-effort, and deliberately not joined into the `?` above: a metadata lookup that fails
    // must not take the whole disassembly report with it. The incumbent swallows the same failure
    // to `null`, and the report is the thing the page is for.
    let source_hint = match st.pool.as_ref() {
        Some(pool) => {
            kittiwake_read::disassembly::source_hint(
                pool,
                &st.ipfs,
                &st.read.gate,
                kittiwake_nest::Lane::Read,
                &id,
            )
            .await
        }
        None => None,
    };

    let mut resp = axum::Json(serde_json::json!({
        "data": kittiwake_read::disassembly::with_signal_and_source(
            &report,
            signal.as_ref(),
            source_hint.as_ref(),
        ),
    }))
    .into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// `GET /api/disassembly/diff?a=Qm…&b=Qm…`: what changed between two versions.
///
/// Both reports go through the same cache as the single-deployment route, so comparing two
/// deployments a reader has already looked at costs nothing beyond the diff itself.
pub async fn disassembly_diff(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DisassemblyDiffQuery>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let a = deployment_cid(q.a.as_deref())?;
    let b = deployment_cid(q.b.as_deref())?;
    if a == b {
        return Err(ApiError::BadRequest(
            "Both deployment IDs are identical, so there is nothing to compare".into(),
        )
        .into());
    }

    let (base, target) = tokio::try_join!(
        kittiwake_read::disassembly::report(st.pool.as_ref(), &st.ipfs, &a),
        kittiwake_read::disassembly::report(st.pool.as_ref(), &st.ipfs, &b),
    )?;
    let (base_signal, target_signal) = tokio::join!(
        kittiwake_read::disassembly::signal(&st.read.gate, &a),
        kittiwake_read::disassembly::signal(&st.read.gate, &b),
    );

    let mut resp = axum::Json(serde_json::json!({
        "data": {
            "diff": kittiwake_disasm::diff::diff(&base, &target),
            "base": kittiwake_read::disassembly::with_signal(&base, base_signal.as_ref()),
            "target": kittiwake_read::disassembly::with_signal(&target, target_signal.as_ref()),
        }
    }))
    .into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}

/// A deployment CID, or a refusal naming the shape. Requesting the gateway with anything else asks
/// it to resolve a hash that cannot exist and turns the caller's typo into a timeout.
///
/// The CID form, not the hex `deployment_id` above returns: IPFS is what this route talks to.
fn deployment_cid(raw: Option<&str>) -> Result<String, ErrorResponse> {
    raw.and_then(cid_v0).map(str::to_string).ok_or_else(|| {
        ApiError::BadRequest("Invalid deployment ID. Expected a CIDv0 hash (Qm…)".into()).into()
    })
}

/// `POST /api/analytics/clickthrough` — one trade click or protocol visit.
///
/// Unauthenticated and fire-and-forget: the caller ignores the answer, and a reader must never see
/// anything because a click could not be counted. What that buys is a public write into a shared
/// table, so the validation here is the whole of the door: the event type is checked against the
/// two the column's CHECK allows, `venue` and `session_id` are required, and the wallet is stored
/// only when it is shaped like an address.
pub async fn analytics_clickthrough(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<kittiwake_db::Clickthrough>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    if !kittiwake_db::CLICKTHROUGH_EVENT_TYPES.contains(&body.event_type.as_str())
        || body.venue.is_empty()
        || body.session_id.is_empty()
    {
        return Err(ApiError::BadRequest("invalid payload".into()).into());
    }

    kittiwake_db::record_clickthrough(pool, &body)
        .await
        .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;

    Ok(axum::Json(serde_json::json!({ "ok": true })).into_response())
}

#[cfg(test)]
mod schema_cid_tests {
    use super::schema_cid;

    const SCHEMA: &str = "QmcGeTxBaueyQtNuV2p9DGjsnu9wNDQqMgTR2XyT3Fiz6U";
    const ABI: &str = "QmPfKNZPkVATzL3EHypkcsyKhehR4jkiYEVqSzh2hxWYj1";

    /// The shape `graph build` emits: keys in alphabetical order, so `dataSources` comes first and
    /// carries CIDs of its own, and `schema:` sits below it. Reading only the text above
    /// `dataSources` finds nothing here and the route answered 404 for the whole of mainnet.
    #[test]
    fn the_schema_is_found_below_the_data_sources() {
        let manifest = format!(
            "dataSources:\n  - kind: ethereum/contract\n    mapping:\n      abis:\n        - file:\n            /: /ipfs/{ABI}\n          name: ERC20\nschema:\n  file:\n    /: /ipfs/{SCHEMA}\nspecVersion: 0.0.4\n"
        );
        assert_eq!(schema_cid(&manifest), Some(SCHEMA));
    }

    /// The hand-written order, which is the one the port did handle.
    #[test]
    fn the_schema_is_found_above_the_data_sources() {
        let manifest = format!(
            "specVersion: 0.0.5\nschema:\n  file: /ipfs/{SCHEMA}\ndataSources:\n  - mapping:\n      abis:\n        - file: /ipfs/{ABI}\n"
        );
        assert_eq!(schema_cid(&manifest), Some(SCHEMA));
    }

    /// The key on its own line wins over the word appearing earlier. The incumbent's regex takes
    /// the first `schema` anywhere and would answer with the ABI's CID here.
    #[test]
    fn a_key_named_after_the_word_does_not_claim_the_search() {
        let manifest = format!(
            "schemaVersion: 3\ndataSources:\n  - mapping:\n      abis:\n        - file: /ipfs/{ABI}\nschema:\n  file: /ipfs/{SCHEMA}\n"
        );
        assert_eq!(schema_cid(&manifest), Some(SCHEMA));
    }

    #[test]
    fn a_manifest_with_no_schema_names_none() {
        let manifest =
            format!("specVersion: 0.0.5\ndataSources:\n  - mapping:\n      file: /ipfs/{ABI}\n");
        assert_eq!(schema_cid(&manifest), None);
    }
}

// ---------------------------------------------------------------------------------------------
// Filing an issue in nightswatchhq/graph-support
// ---------------------------------------------------------------------------------------------

/// Long enough for a stack trace and a query, short enough that nobody pastes a database in.
const MAX_FIELD_CHARS: usize = 20_000;
const MAX_TITLE_CHARS: usize = 256;
const MIN_TITLE_CHARS: usize = 8;

/// The forms, parsed, for the compose page to render.
///
/// On its own path rather than sharing one with `file_issue`, because the rate limiter buckets by
/// path and not by method: three writes a minute is right for opening issues in a public repository
/// and would lock a reader out of the form on their third page load.
pub async fn issue_forms(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;

    let rows = kittiwake_db::graph_support_templates(pool)
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;

    // A template the mirror holds but this cannot parse is skipped rather than fatal: five usable
    // forms beat a page that will not load because a sixth grew a field type we do not know.
    let templates: Vec<kittiwake_support::IssueForm> = rows
        .iter()
        .filter_map(
            |r| match kittiwake_support::parse_issue_form(&r.file, &r.yaml) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(file = %r.file, error = %e, "a graph-support template did not parse");
                    None
                }
            },
        )
        .collect();

    if templates.is_empty() {
        return Err(ApiError::Unavailable("The issue forms could not be read.".into()).into());
    }

    let mut response = axum::Json(serde_json::json!({
        "templates": templates,
        "canFile": st.support_token.is_some(),
        "chooserUrl": kittiwake_support::chooser_url(),
    }))
    .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct FileIssueBody {
    template: String,
    title: String,
    values: serde_json::Value,
    handle: Option<String>,
    /// Honeypot. A real form leaves it empty because it is never shown.
    website: Option<String>,
}

/// Coerce the posted answers into the shape the renderer takes, dropping anything else.
fn read_values(raw: &serde_json::Value) -> kittiwake_support::Values {
    let mut out = std::collections::HashMap::new();
    let Some(obj) = raw.as_object() else {
        return kittiwake_support::Values(out);
    };
    for (key, value) in obj {
        let answer = match value {
            serde_json::Value::String(s) => Some(kittiwake_support::Answer::One(clamp(s))),
            serde_json::Value::Array(items) => Some(kittiwake_support::Answer::Many(
                items.iter().filter_map(|v| v.as_str()).map(clamp).collect(),
            )),
            _ => None,
        };
        if let Some(a) = answer {
            out.insert(key.clone(), a);
        }
    }
    kittiwake_support::Values(out)
}

/// Truncated on a character boundary. Slicing bytes would panic mid-codepoint on the first report
/// that pastes twenty thousand characters of anything but ASCII.
fn clamp(s: &str) -> String {
    s.chars().take(MAX_FIELD_CHARS).collect()
}

fn filing_error(message: &str, status: axum::http::StatusCode) -> Response {
    let mut r = (
        status,
        axum::Json(serde_json::json!({
            "error": message,
            "chooserUrl": kittiwake_support::chooser_url(),
        })),
    )
        .into_response();
    crate::no_store(r.headers_mut());
    r
}

/// Open an issue on a reporter's behalf.
///
/// The credential is one account, so every thread filed here is authored by it.
/// `provenance_footer` says so in the body rather than letting the archive imply the maintainer
/// reported it themselves.
pub async fn file_issue(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Option<axum::Json<FileIssueBody>>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);

    // `Option<Json<..>>` rather than `Json<..>`: a mandatory body makes axum answer 400 before the
    // handler runs, so a client that sends none gets a bare rejection with no explanation. That
    // shipped once already on the Scuttlebutt delete (kittiwake#109).
    let Some(axum::Json(payload)) = body else {
        return Ok(filing_error(
            "The request body was not JSON.",
            axum::http::StatusCode::BAD_REQUEST,
        ));
    };

    if !payload
        .website
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Ok(filing_error(
            "Rejected.",
            axum::http::StatusCode::BAD_REQUEST,
        ));
    }

    let Some(token) = st.support_token.as_deref() else {
        // Not a failure the reporter caused, and their typing is still in the form, so the answer
        // says where else to put it rather than pretending the send worked.
        return Ok(filing_error(
            "Filing from Lodestar is not configured. The issue can still be opened on GitHub.",
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
        ));
    };

    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;
    let rows = match kittiwake_db::graph_support_templates(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "the issue forms could not be read");
            return Ok(filing_error(
                "The issue forms could not be read, so nothing was filed.",
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
            ));
        }
    };

    let Some(form) = rows
        .iter()
        .find(|r| r.file == payload.template)
        .and_then(|r| kittiwake_support::parse_issue_form(&r.file, &r.yaml).ok())
    else {
        return Ok(filing_error(
            "That is not one of the issue forms.",
            axum::http::StatusCode::BAD_REQUEST,
        ));
    };

    let title = payload.title.trim();
    if title.chars().count() < MIN_TITLE_CHARS {
        return Ok(filing_error(
            "The title needs to say what broke, in at least a few words.",
            axum::http::StatusCode::BAD_REQUEST,
        ));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Ok(filing_error(
            "The title is too long.",
            axum::http::StatusCode::BAD_REQUEST,
        ));
    }

    let values = read_values(&payload.values);
    let errors = kittiwake_support::validate(&form, &values);
    if let Some(first) = errors.first() {
        let mut r = (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": first,
                "errors": errors,
                "chooserUrl": kittiwake_support::chooser_url(),
            })),
        )
            .into_response();
        crate::no_store(r.headers_mut());
        return Ok(r);
    }

    let handle = payload
        .handle
        .as_deref()
        .map(|h| h.chars().take(120).collect::<String>());
    let body_md = format!(
        "{}\n\n{}",
        kittiwake_support::render_issue_body(&form, &values),
        kittiwake_support::provenance_footer(handle.as_deref())
    );

    let client = reqwest::Client::new();
    let res = match client
        .post(format!(
            "https://api.github.com/repos/{}/issues",
            kittiwake_support::REPO
        ))
        .header(axum::http::header::ACCEPT, "application/vnd.github.v3+json")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        // GitHub refuses a request with no user agent, and a named one is what appears in their
        // rate-limit accounting rather than an anonymous bucket shared with everything else.
        .header(axum::http::header::USER_AGENT, "kittiwake")
        .json(&serde_json::json!({
            "title": title,
            "body": body_md,
            "labels": form.labels,
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, file = %form.file, "GitHub could not be reached to open an issue");
            return Ok(filing_error(
                "GitHub could not be reached, so nothing was filed.",
                axum::http::StatusCode::BAD_GATEWAY,
            ));
        }
    };

    if !res.status().is_success() {
        tracing::warn!(
            status = res.status().as_u16(),
            file = %form.file,
            rate_limit_remaining = ?res.headers().get("x-ratelimit-remaining"),
            "GitHub refused to open a graph-support issue"
        );
        return Ok(filing_error(
            "GitHub refused the issue, so nothing was filed.",
            axum::http::StatusCode::BAD_GATEWAY,
        ));
    }

    let created: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    let Some(number) = created.get("number").and_then(|n| n.as_u64()) else {
        // The issue may well exist; what is missing is the number to send anyone to.
        return Ok(filing_error(
            "The issue was accepted but GitHub did not say which number it got.",
            axum::http::StatusCode::BAD_GATEWAY,
        ));
    };

    let mut response = axum::Json(serde_json::json!({
        "number": number,
        "url": created
            .get("html_url")
            .and_then(|u| u.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://github.com/{}/issues/{number}", kittiwake_support::REPO)),
    }))
    .into_response();
    crate::no_store(response.headers_mut());
    Ok(response)
}

#[cfg(test)]
mod deployment_id_tests {
    use super::*;

    /// **base58 is case-sensitive and `addr` lowercases.**
    ///
    /// This shipped once and 400'd `/api/indexing-status`, `/api/subgraph-curation` and
    /// `/api/subgraph-history`. `deployment_id` was written to fix it, and on 2026-09-12 a browser
    /// sweep found `/api/poi?deployment=` still using `addr`, so every POI deployment page had been
    /// failing. Four routes, one mistake, and the comment warning about it was directly above.
    ///
    /// So the property is pinned rather than the callers audited again: a CID must survive, and the
    /// naive normaliser must be visibly unable to carry one.
    #[test]
    fn a_cid_survives_deployment_id_and_does_not_survive_addr() {
        let cid = "QmStfa6zwPUzKHyGrgdNMh8rgbc91sgvmskidBSujG92FR";

        let id = deployment_id(cid);
        assert_eq!(id.len(), 66, "a CID must convert to a bytes32 hex id: {id}");
        assert!(id.starts_with("0x"), "{id}");
        assert_eq!(id, id.to_lowercase(), "the hex form is lowercase");

        // The failure mode, stated so the test explains itself: `addr` mangles the CID, and the
        // mangled value cannot be converted at all.
        assert_ne!(addr(cid), cid, "addr lowercases, which is the bug");
        assert!(
            kittiwake_read::shape::ipfs_hash_to_bytes32(&addr(cid)).is_none(),
            "a lowercased CID must be unconvertible, or this test proves nothing"
        );
    }

    /// A hex id handed in directly still works, because links carry CIDs and callers carry both.
    #[test]
    fn a_bytes32_id_passes_through_unchanged() {
        let hex = "0x43a24eec54b93a1f0e5f1c6a4b9a0a6d1e2f3c4b5a69788796a5b4c3d2e1f009";
        assert_eq!(deployment_id(hex), hex);
        assert_eq!(deployment_id(&hex.to_uppercase()), hex);
    }
}

#[cfg(test)]
mod indexer_page_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use kittiwake_ext::{IndexingStatus, ProbeResult};
    use kittiwake_read::status_probe::{Probe, StatusProbes};
    use serde_json::Value;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const INDEXER: &str = "0x00000000000000000000000000000000000000aa";
    const DEPLOYMENT: &str = "QmStfa6zwPUzKHyGrgdNMh8rgbc91sgvmskidBSujG92FR";
    const METADATA: &str = "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff";

    /// Fragments of SQL: a statement containing any of them is refused.
    type Refused = Arc<Mutex<Vec<&'static str>>>;

    /// The allocation nest, the GNS nest and IPFS behind one stand-in, recording every call. The
    /// indexer has one allocation, on a deployment whose metadata document names it.
    struct Upstream(Arc<Mutex<Vec<String>>>, Refused);

    impl Respond for Upstream {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let path = req.url.path().to_string();
            if path.ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            self.0.lock().unwrap().push(path.clone());
            if path == "/api/v0/cat" {
                return ResponseTemplate::new(200)
                    .set_body_string(r#"{"displayName":"Uniswap V3"}"#);
            }
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            if self.1.lock().unwrap().iter().any(|f| q.contains(f)) {
                return ResponseTemplate::new(500)
                    .set_body_string("Resources exhausted: failed to allocate additional memory");
            }
            let rows: Vec<Value> = if path.starts_with("/gns/") {
                q.split('\'')
                    .skip(1)
                    .step_by(2)
                    .filter(|s| s.len() == 66 && s.starts_with("0x"))
                    .map(|id| {
                        serde_json::json!({
                            "deployment_id": id, "subgraph_id": "1",
                            "subgraph_metadata": METADATA, "version_metadata": null
                        })
                    })
                    .collect()
            } else {
                vec![serde_json::json!({
                    "id": INDEXER,
                    "url": "https://indexer.example",
                    "allocated_tokens": "1",
                    "subgraph_deployment": kittiwake_read::shape::ipfs_hash_to_bytes32(DEPLOYMENT),
                })]
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": rows.len(), "rows": rows,
                "provenance": {"as_of": 1, "sealed_through": 1, "source": "mock"}
            }))
        }
    }

    async fn state(
        probe: Probe,
        wait: Duration,
    ) -> (AppState, Arc<Mutex<Vec<String>>>, MockServer) {
        stand_in(
            probe,
            wait,
            Refused::default(),
            kittiwake_cache::Store::new(64),
        )
        .await
    }

    async fn stand_in(
        probe: Probe,
        wait: Duration,
        refused: Refused,
        store: kittiwake_cache::Store,
    ) -> (AppState, Arc<Mutex<Vec<String>>>, MockServer) {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(any())
            .respond_with(Upstream(seen.clone(), refused))
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in kittiwake_types::NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        // Postgres is a listener that records each connection and hangs up. Nothing is ever read
        // from it, so a request that names anything also has to ask the GNS nest and IPFS.
        let postgres = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = postgres.local_addr().unwrap().port();
        let connections = seen.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = postgres.accept().await {
                connections.lock().unwrap().push("postgres".to_string());
                drop(socket);
            }
        });
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy(&format!("postgres://unused:unused@127.0.0.1:{port}/unused"))
            .expect("a lazy pool needs no server");
        let ipfs = kittiwake_ext::Ipfs::new(&server.uri());
        let read =
            kittiwake_read::ReadCtx::new(kittiwake_nest::NestGate::new(cfg).unwrap(), store, 1)
                .with_naming(Some(kittiwake_read::naming::Naming::new(
                    pool.clone(),
                    ipfs.clone(),
                )));
        let mut st = crate::state(
            crate::Limits::new(1000, 1000, 1000, Vec::new()),
            None,
            Some(pool),
            read,
            None,
            None,
            ipfs,
            None,
            None,
            None,
            None,
            None,
        );
        st.probes = StatusProbes::with_probe(probe, wait);
        (st, seen, server)
    }

    fn peer() -> ConnectInfo<SocketAddr> {
        ConnectInfo(([127, 0, 0, 1], 1).into())
    }

    async fn body(resp: Result<Response, ErrorResponse>) -> Value {
        let Ok(resp) = resp else {
            panic!("the route refused")
        };
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn node_answering(reachable: bool) -> Probe {
        Arc::new(move |url| {
            Box::pin(async move {
                ProbeResult {
                    url,
                    reachable,
                    elapsed_ms: 3,
                    statuses: vec![IndexingStatus {
                        deployment: DEPLOYMENT.to_string(),
                        network: Some("mainnet".to_string()),
                        health: "healthy".to_string(),
                        synced: true,
                        latest_block: Some(10),
                        chain_head_block: Some(10),
                        blocks_behind: Some(0),
                    }]
                    .into_iter()
                    .filter(|_| reachable)
                    .collect(),
                    error: (!reachable).then(|| "connection refused".to_string()),
                }
            })
        })
    }

    // What the stand-in refuses, each matched on a fragment of its statement.
    const DELEGATORS: &str = "staking_legacy__stake_delegated";
    const INDEXER_ROW: &str = "FROM lodestar_indexers";
    const RATIO: &str = "FROM lodestar_network_params";

    /// The page, where it was served from, and whether its headers say it is degraded.
    async fn profile(st: &AppState) -> (String, bool, Value) {
        let Ok(resp) = indexer(
            State(st.clone()),
            peer(),
            HeaderMap::new(),
            Path(INDEXER.to_string()),
        )
        .await
        else {
            panic!("the route refused")
        };
        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let served = header("x-kittiwake-served-from");
        let degraded = header("x-kittiwake-degraded") == "true";
        (served, degraded, body(Ok(resp)).await)
    }

    /// On 2026-09-14 the delegators ran the allocation nest out of memory, and the page failed with
    /// them for half an hour.
    #[tokio::test]
    async fn a_section_the_nest_refuses_is_left_out_and_named_and_the_page_still_answers() {
        let refused = Refused::new(Mutex::new(vec![DELEGATORS]));
        let (st, _, _server) = stand_in(
            node_answering(true),
            Duration::ZERO,
            refused,
            kittiwake_cache::Store::new(64),
        )
        .await;

        let (_, degraded, body) = profile(&st).await;
        assert!(degraded, "the header says so");
        assert_eq!(body["provenance"]["degraded"], true);
        assert_eq!(
            body["data"]["degraded"],
            serde_json::json!([{"part": "delegators", "reason": "nest_upstream"}])
        );
        let indexer = &body["data"]["indexer"];
        assert!(
            indexer.get("delegators").is_none(),
            "left out, not shown empty"
        );
        assert!(indexer["stakedTokens"].is_string());
        assert!(indexer["account"]["operators"].is_array());
        assert!(indexer["closedAllocations"].is_array());
        assert_eq!(
            indexer["allocations"][0]["subgraphDeployment"]["displayName"], "Uniswap V3",
            "the rest is still named"
        );
    }

    #[tokio::test]
    async fn a_required_part_the_nest_refuses_still_fails_the_page() {
        for required in [INDEXER_ROW, RATIO] {
            let refused = Refused::new(Mutex::new(vec![required]));
            let (st, _, _server) = stand_in(
                node_answering(true),
                Duration::ZERO,
                refused,
                kittiwake_cache::Store::new(64),
            )
            .await;
            let answer = indexer(
                State(st.clone()),
                peer(),
                HeaderMap::new(),
                Path(INDEXER.to_string()),
            )
            .await;
            match answer {
                Ok(resp) => panic!("answered {} without {required}", resp.status()),
                Err(e) => assert_eq!(e.0.status(), 502, "{required}"),
            }
        }
    }

    /// Kept for the route's own window, the page would be served without its delegators as though
    /// it had none, and #148's warmer would keep it that way for an hour.
    #[tokio::test]
    async fn a_page_missing_a_section_is_not_kept_as_though_it_were_whole() {
        let window = Duration::from_millis(300);
        let refused = Refused::new(Mutex::new(vec![DELEGATORS]));
        let (st, _, _server) = stand_in(
            node_answering(true),
            Duration::ZERO,
            refused.clone(),
            kittiwake_cache::Store::keeping_partial_for(64, window),
        )
        .await;

        let (served, degraded, _) = profile(&st).await;
        assert_eq!((served.as_str(), degraded), ("nest", true));
        let (served, degraded, body) = profile(&st).await;
        assert_eq!(served, "cache");
        assert!(degraded, "from the cache, still said to be degraded");
        assert!(body["data"]["degraded"].is_array());

        refused.lock().unwrap().clear();
        // Past the window, and past the five seconds the gate remembers a failed statement for.
        tokio::time::sleep(Duration::from_millis(5500)).await;
        let (served, degraded, body) = profile(&st).await;
        assert_eq!(served, "nest", "asked again once its window was out");
        assert!(!degraded);
        assert!(body["data"].get("degraded").is_none());
        assert!(body["data"]["indexer"]["delegators"].is_array());

        tokio::time::sleep(window + Duration::from_millis(200)).await;
        let (served, _, _) = profile(&st).await;
        assert_eq!(served, "cache", "a whole page keeps its route's window");
    }

    #[tokio::test]
    async fn a_page_whose_parts_all_answer_says_nothing_of_degradation() {
        let (st, _, _server) = state(node_answering(true), Duration::ZERO).await;
        let (_, degraded, body) = profile(&st).await;
        assert!(!degraded);
        assert!(body["provenance"].get("degraded").is_none());
        let data = body["data"].as_object().unwrap();
        assert_eq!(data.keys().collect::<Vec<_>>(), ["indexer"]);
        let indexer = &data["indexer"];
        // What Lodestar's `fetchIndexerDetail` holds the route to.
        assert!(indexer["account"].is_object());
        for section in ["allocations", "closedAllocations", "delegators"] {
            assert!(indexer[section].is_array(), "{section}");
        }
        assert!(indexer["account"]["operators"].is_array());
        for figure in ["stakedTokens", "delegatedTokens"] {
            assert!(indexer[figure].is_string(), "{figure}");
        }
    }

    async fn status(st: &AppState) -> Value {
        body(
            indexer_status(
                State(st.clone()),
                peer(),
                HeaderMap::new(),
                Path(INDEXER.to_string()),
            )
            .await,
        )
        .await
    }

    /// The largest indexer's page took 10.5 s from the cache on 2026-09-15: its allocations were
    /// named again on every request, after the cache rather than inside it.
    #[tokio::test]
    async fn a_second_request_for_an_indexer_page_does_no_naming_work() {
        let (st, seen, _server) = state(node_answering(true), Duration::ZERO).await;
        let page = || async {
            body(
                indexer(
                    State(st.clone()),
                    peer(),
                    HeaderMap::new(),
                    Path(INDEXER.to_string()),
                )
                .await,
            )
            .await
        };
        let name = |v: &Value| {
            v["data"]["indexer"]["allocations"][0]["subgraphDeployment"]["displayName"].clone()
        };
        let asked = |prefix: &str| {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with(prefix))
                .count()
        };

        let first = page().await;
        assert_eq!(name(&first), "Uniswap V3");
        assert!(
            asked("/gns/") > 0,
            "the first request asked the GNS nest nothing"
        );
        assert!(
            asked("/api/v0/cat") > 0,
            "the first request asked IPFS nothing"
        );
        assert!(
            asked("postgres") > 0,
            "the first request read no indexer name"
        );
        let before = seen.lock().unwrap().len();

        let second = page().await;
        assert_eq!(
            name(&second),
            "Uniswap V3",
            "the cached answer carries the names"
        );
        let after = seen.lock().unwrap()[before..].to_vec();
        assert!(after.is_empty(), "a cached page asked {after:?}");
    }

    /// Probed inside the request, a node that did not answer cost every load of the page the probe's
    /// eight seconds (2026-09-15).
    #[tokio::test]
    async fn a_node_that_does_not_answer_costs_the_status_route_the_wait_and_no_more() {
        let silent: Probe = Arc::new(|url| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                node_answering(false)(url).await
            })
        });
        let wait = Duration::from_millis(300);
        let (st, _, _server) = state(silent, wait).await;
        assert!(
            kittiwake_read::routes::indexer_status(&st.read, INDEXER)
                .await
                .is_ok(),
            "the nest fold, cached first so only the probe is timed"
        );

        let began = Instant::now();
        let body = status(&st).await;
        let took = began.elapsed();
        assert!(took < wait + Duration::from_millis(500), "waited {took:?}");
        let node = &body["data"]["node"];
        assert_eq!(
            node["reachable"],
            Value::Null,
            "not yet known is not unreachable"
        );
        assert_eq!(node["pending"], true);
        assert_eq!(node["checkedAt"], Value::Null);
        assert_eq!(node["lastReachedAt"], Value::Null);
    }

    #[tokio::test]
    async fn a_node_that_cannot_be_reached_is_said_to_be_and_never_to_have_been_reached() {
        let (st, _, _server) = state(node_answering(false), Duration::from_secs(2)).await;
        let body = status(&st).await;
        let data = &body["data"];
        let node = &data["node"];
        assert_eq!(node["reachable"], false);
        assert_eq!(node["error"], "connection refused");
        assert_eq!(node["pending"], false);
        assert_eq!(node["ageSeconds"], 0);
        assert!(node["checkedAt"].is_i64());
        assert_eq!(node["lastReachedAt"], Value::Null);
        assert_eq!(data["unreachableCount"], 1);
        assert_eq!(data["deployments"][0]["status"], "unreachable");
    }

    #[tokio::test]
    async fn a_node_that_answers_is_said_to_have_been_reached_when_it_was_checked() {
        let (st, _, _server) = state(node_answering(true), Duration::from_secs(2)).await;
        let body = status(&st).await;
        let data = &body["data"];
        let node = &data["node"];
        assert_eq!(node["reachable"], true);
        assert_eq!(node["stale"], false);
        assert!(node["checkedAt"].is_i64());
        assert_eq!(node["lastReachedAt"], node["checkedAt"]);
        assert_eq!(data["syncedCount"], 1);
        assert_eq!(data["deployments"][0]["status"], "synced");
    }

    async fn directory_page(
        st: &AppState,
        pairs: &[(&str, &str)],
    ) -> Result<Response, ErrorResponse> {
        let params = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        subgraph_directory(State(st.clone()), peer(), HeaderMap::new(), Query(params)).await
    }

    #[tokio::test]
    async fn the_directory_answers_a_page_with_its_total_and_what_it_could_not_label() {
        let (st, _, _server) = state(node_answering(true), Duration::ZERO).await;
        let all = body(directory_page(&st, &[("first", "10")]).await).await;
        assert_eq!(all["total"], 1);
        assert_eq!(all["data"].as_array().unwrap().len(), 1);
        // No database answered, so no manifest was read and the row cannot match a network.
        assert_eq!(all["unanalysed"], 1);
        let mainnet = body(directory_page(&st, &[("network", "mainnet")]).await).await;
        assert_eq!(mainnet["total"], 0);
    }

    #[tokio::test]
    async fn a_bound_the_directory_cannot_read_is_refused_before_the_nest_is_asked() {
        let (st, seen, _server) = state(node_answering(true), Duration::ZERO).await;
        assert!(directory_page(&st, &[("signalMin", "lots")]).await.is_err());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_directory_asks_the_nest_once_and_filters_the_kept_set_after() {
        let (st, seen, _server) = state(node_answering(true), Duration::ZERO).await;
        body(directory_page(&st, &[]).await).await;
        body(directory_page(&st, &[("sort", "ratio"), ("ratioMin", "5")]).await).await;
        let asked = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with("/alloc"))
            .count();
        assert_eq!(asked, 2, "the set and its fees, once between them");
    }

    #[test]
    fn the_fee_window_moves_by_the_hour_not_the_second() {
        let t: i64 = 1_790_000_000;
        let hour = t - t.rem_euclid(3600);
        assert_eq!(thirty_days_back(hour), thirty_days_back(hour + 3599));
        assert_eq!(thirty_days_back(hour + 3600) - thirty_days_back(hour), 3600);
    }
}
