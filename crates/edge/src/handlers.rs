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

use crate::{AppState, ErrorResponse, client_ip, envelope};

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
        $st.limits
            .check(&$st.limits.read, client_ip($peer, &$headers))?
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

/// An address from the path, lower-cased. The SQL layer refuses anything that is not one, but
/// normalising here means a caller using checksum case is served rather than rejected.
fn addr(raw: &str) -> String {
    raw.trim().to_lowercase()
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

pub async fn indexer_status(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        routes::indexer_status(&st.read, &addr(&address)).await?,
    ))
}

pub async fn apr_provenance(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        routes::apr_provenance(&st.read, &addr(&address)).await?,
    ))
}

/// Weekly points over the last two years, which is what the chart draws.
pub async fn indexer_stake_history(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<DaysQuery>,
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
    Ok(envelope(
        routes::rewards_history(&st.read, &addr(&address)).await?,
    ))
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
    Ok(envelope(read))
}

pub async fn subgraph_fees_30d(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    let since = now() - 30 * 86_400;
    Ok(envelope(routes::subgraph_fees_30d(&st.read, since).await?))
}

pub async fn subgraph_curation(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        routes::subgraph_curation(&st.read, &addr(&hash)).await?,
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
        routes::subgraph_history(&st.read, &addr(&hash)).await?,
    ))
}

pub async fn indexing_status(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, ErrorResponse> {
    budget!(st, peer, headers);
    Ok(envelope(
        routes::deployment_status(&st.read, &addr(&hash)).await?,
    ))
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
        .cached_external("price", std::time::Duration::from_secs(60), {
            let ext = st.ext.clone();
            std::sync::Arc::new(move || {
                let ext = ext.clone();
                Box::pin(async move {
                    let (usd, change) = ext.grt_price().await?;
                    Ok(serde_json::json!({ "usd": usd, "change24h": change }))
                })
            })
        })
        .await?;
    Ok(envelope(read))
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
        .cached_external("tvl", std::time::Duration::from_secs(600), {
            let ext = st.ext.clone();
            std::sync::Arc::new(move || {
                let ext = ext.clone();
                Box::pin(async move {
                    let tvl = ext.arbitrum_tvl().await?;
                    Ok(serde_json::json!({ "chain": "Arbitrum", "tvlUsd": tvl }))
                })
            })
        })
        .await?;
    Ok(envelope(read))
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
        .cached_external_keyed("reo", &route_key, std::time::Duration::from_secs(300), {
            std::sync::Arc::new(move || {
                let chain = chain.clone();
                let address = address.clone();
                Box::pin(async move {
                    let e = chain.eligibility(&address).await?;
                    Ok(serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
                })
            })
        })
        .await?;
    Ok(envelope(read))
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
    st.limits
        .check(&st.limits.mutation, client_ip(peer, &headers))?;
    let result = st.ext.probe_indexer(&q.url).await;
    let mut resp = axum::Json(serde_json::json!({ "data": result })).into_response();
    crate::no_store(resp.headers_mut());
    Ok(resp)
}
