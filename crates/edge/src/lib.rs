//! The HTTP edge: what a caller sees, and what they are never allowed to see.
//!
//! Two invariants live here. Every data response carries its provenance in headers as well as the
//! body, so a consumer that only reads headers can still date the answer. And every failure is
//! told with a stable machine code and, where the nest gave one, the nest's own reason — never a
//! blank 500 and never another source's data wearing this route's name.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

pub mod handlers;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use kittiwake_read::ReadCtx;
use kittiwake_types::{ApiError, Meta, NestId};

pub(crate) type Limiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// One budget class. Separate limiters rather than one shared: a crawler exhausting the public SQL
/// budget must not also lock the dashboard's own reads out.
pub struct Limits {
    pub sql: Limiter,
    pub read: Limiter,
    pub mutation: Limiter,
    pub exempt: Vec<IpAddr>,
}

impl Limits {
    pub fn new(
        sql_per_min: u32,
        read_per_min: u32,
        mutation_per_min: u32,
        exempt: Vec<IpAddr>,
    ) -> Self {
        // Burst is clamped to the per-minute budget. Left unclamped, a generous burst silently
        // overrides a tight quota - a 3/min limit with a burst of 40 admits 40 callers in one
        // second, which is not a limit at all, and reads as working right up until it matters.
        let q = |n: u32, burst: u32| {
            let n = n.max(1);
            Quota::per_minute(NonZeroU32::new(n).unwrap())
                .allow_burst(NonZeroU32::new(burst.clamp(1, n)).unwrap())
        };
        Self {
            sql: RateLimiter::keyed(q(sql_per_min, 5)),
            read: RateLimiter::keyed(q(read_per_min, 40)),
            mutation: RateLimiter::keyed(q(mutation_per_min, 10)),
            exempt,
        }
    }

    pub(crate) fn check(&self, limiter: &Limiter, ip: IpAddr) -> Result<(), ApiError> {
        if self.exempt.contains(&ip) {
            return Ok(());
        }
        limiter.check_key(&ip).map_err(|not_until| {
            metrics::counter!("rate_limited_total").increment(1);
            ApiError::RateLimited {
                retry_after: not_until
                    .wait_time_from(DefaultClock::default().now())
                    .as_secs()
                    .max(1),
            }
        })
    }

    /// Drop the state of callers who have not been seen recently, so a long uptime against a wide
    /// range of source addresses cannot grow the keyed state without bound.
    pub fn sweep(&self) {
        self.sql.retain_recent();
        self.read.retain_recent();
        self.mutation.retain_recent();
    }
}

#[derive(Clone)]
pub struct AppState {
    pub read: ReadCtx,
    pub ext: kittiwake_ext::Ext,
    pub ipfs: kittiwake_ext::Ipfs,
    /// The secret the edge proxy presents. Absent means no request is ever trusted to name a client
    /// other than the one that connected - safe, and wrong once traffic arrives through Vercel.
    pub edge_secret: Option<Arc<str>>,
    /// Absent when no Arbitrum RPC is configured; the routes that need it say so rather than
    /// silently returning nothing.
    pub chain: Option<kittiwake_chain::Chain>,
    /// Absent when no database is configured; the routes that need one say so.
    pub pool: Option<kittiwake_db::Pool>,
    pub limits: Arc<Limits>,
    pub metrics: Option<kittiwake_obs::PrometheusHandle>,
    pub started: std::time::Instant,
}

/// The client's address.
///
/// `X-Forwarded-For` is honoured **only** when the immediate peer is loopback, which is the only
/// case where the header was written by our own Caddy rather than by the caller. Trusting it
/// unconditionally would let anyone choose their own rate-limit bucket.
/// The header the edge proxy proves itself with.
pub const EDGE_HEADER: &str = "x-lodestar-edge";

/// The header the edge proxy names the real client in.
///
/// A header of our own rather than `X-Forwarded-For`, and the reason is Caddy. Caddy **replaces**
/// the forwarded chain with its own peer unless that peer is in `trusted_proxies`, which is correct
/// of it - preserving an arbitrary caller's chain is how spoofing works - but it means the browser
/// address the edge put there never survives the hop. The alternative was to maintain a list of
/// Vercel's egress ranges in the Caddyfile and keep it current, which is a list that goes stale
/// silently. A header Caddy does not manage is simply passed through untouched.
pub const CLIENT_HEADER: &str = "x-lodestar-client";

pub(crate) fn client_ip(
    peer: SocketAddr,
    headers: &HeaderMap,
    edge_secret: Option<&str>,
) -> IpAddr {
    let peer_ip = peer.ip();

    // A direct connection. Nothing in front, so the peer is the caller and no header is worth
    // reading.
    if !peer_ip.is_loopback() {
        return peer_ip;
    }

    // Behind Caddy. Two questions, and conflating them is what made every visitor share one
    // rate-limit bucket: *who connected to Caddy*, which Caddy tells us and nobody else can forge,
    // and *who the browser was*, which only something upstream of Caddy knows.
    let caddys_peer = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        // Caddy appends its own peer to the forwarded chain, so the rightmost entry says the same
        // thing. A fallback for a proxy that sets one and not the other.
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse::<IpAddr>().ok())
        })
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

    // The edge names the real browser in `X-Lodestar-Client` - and so could anyone else, since it
    // is just a header. It is only worth reading when the request provably came through our own
    // edge, which is what the secret establishes. Without it, every visitor arrives wearing the
    // edge's address and 120 requests a minute is the whole world's budget.
    let Some(secret) = edge_secret else {
        return caddys_peer;
    };
    let presented = headers.get(EDGE_HEADER).and_then(|v| v.to_str().ok());
    let Some(presented) = presented else {
        return caddys_peer;
    };
    // Constant time. A byte-at-a-time comparison of a shared secret is a few thousand requests away
    // from being no secret at all.
    let ok: bool = <subtle::Choice as Into<bool>>::into(subtle::ConstantTimeEq::ct_eq(
        presented.as_bytes(),
        secret.as_bytes(),
    ));
    if presented.len() != secret.len() || !ok {
        return caddys_peer;
    }

    headers
        .get(CLIENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        // The edge proved itself but named nobody. Its own address is the honest answer.
        .unwrap_or(caddys_peer)
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        let e = self.0;
        let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut body = serde_json::json!({ "error": e.code(), "message": e.to_string() });

        // The nest's own diagnosis travels to the caller: "delegation data is three weeks behind"
        // is a useful thing for a panel to render, where a bare 503 is not.
        if let ApiError::NestUnready {
            reason,
            lag_blocks,
            sealed_through,
        } = &e
        {
            body["reason"] = serde_json::json!(reason);
            body["lag_blocks"] = serde_json::json!(lag_blocks);
            body["sealed_through"] = serde_json::json!(sealed_through);
        }

        let mut resp = (status, axum::Json(body)).into_response();
        if let Some(secs) = e.retry_after()
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        no_store(resp.headers_mut());
        resp
    }
}

/// Wrapper so `ApiError` can implement `IntoResponse` without owning an axum dependency in types.
pub struct ErrorResponse(pub ApiError);

impl From<ApiError> for ErrorResponse {
    fn from(e: ApiError) -> Self {
        ErrorResponse(e)
    }
}

/// Vercel's CDN caches rewritten external origins by upstream `Cache-Control` since April 2026.
/// Every data route must therefore say `no-store`, or the edge will hold protocol figures past
/// their freshness budget and our whole staleness model is decided by somebody else's cache.
pub(crate) fn no_store(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn provenance_headers(headers: &mut HeaderMap, meta: &Meta) {
    let set = |h: &mut HeaderMap, k: &'static str, v: String| {
        if let Ok(val) = HeaderValue::from_str(&v) {
            h.insert(k, val);
        }
    };
    set(
        headers,
        "x-kittiwake-served-from",
        meta.served_from.as_str().to_string(),
    );
    set(headers, "x-kittiwake-stale", meta.stale.to_string());
    set(headers, "x-kittiwake-age", meta.age_seconds.to_string());
    if let Some(as_of) = meta.provenance.as_of {
        set(headers, "x-kittiwake-as-of", as_of.to_string());
    }
    if let Some(sealed) = meta.provenance.sealed_through {
        set(headers, "x-kittiwake-sealed-through", sealed.to_string());
    }
}

/// Wrap a cached read as the response every data route returns.
pub(crate) fn envelope(read: kittiwake_read::Read) -> Response {
    let body = serde_json::json!({
        "data": &*read.entry.value,
        // The incumbent carries this and some consumers read it. Cheap, and true.
        "source": "nuthatch",
        "provenance": &read.meta,
    });
    let mut resp = axum::Json(body).into_response();
    provenance_headers(resp.headers_mut(), &read.meta);
    no_store(resp.headers_mut());
    resp
}

/// For the routes whose payload sits at the root rather than under `data`.
///
/// `/api/price` answers `{price, change24h}` and nothing else. Wrapping it would be tidier and
/// would break every caller, so the shape the callers already parse wins.
pub(crate) fn envelope_root(read: kittiwake_read::Read) -> Response {
    let mut body = match &*read.entry.value {
        serde_json::Value::Object(map) => map.clone(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("data".into(), other.clone());
            m
        }
    };
    body.insert(
        "provenance".into(),
        serde_json::to_value(&read.meta).unwrap_or_default(),
    );
    let mut resp = axum::Json(serde_json::Value::Object(body)).into_response();
    provenance_headers(resp.headers_mut(), &read.meta);
    no_store(resp.headers_mut());
    resp
}

async fn get_sql_catalog(State(_st): State<AppState>) -> Response {
    let datasets: Vec<serde_json::Value> = kittiwake_sqlplay::DATASETS
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id, "label": d.label, "chain": d.chain,
                "description": d.description, "archival": d.archival,
            })
        })
        .collect();
    let mut resp = axum::Json(serde_json::json!({ "datasets": datasets })).into_response();
    no_store(resp.headers_mut());
    resp
}

#[derive(serde::Deserialize)]
pub struct SqlQuery {
    dataset: String,
    q: String,
}

async fn post_sql_query(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<SqlQuery>,
) -> Result<Response, ErrorResponse> {
    st.limits.check(
        &st.limits.sql,
        client_ip(peer, &headers, st.edge_secret.as_deref()),
    )?;

    let dataset = kittiwake_sqlplay::find_dataset(&body.dataset)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown dataset {:?}", body.dataset)))?;
    kittiwake_sqlplay::screen(&body.q).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Six seconds, below the nest's own ceiling, so a slow query fails here with a clear message
    // rather than there. A query that cannot answer in six seconds wants a WHERE clause.
    let spec = kittiwake_nest::QuerySpec::read(dataset.nest, body.q.clone())
        .with_timeout(Duration::from_secs(6));
    let answer = st.read.gate.query(&spec).await.map_err(ApiError::from)?;

    let meta = Meta {
        provenance: answer.provenance.clone(),
        served_from: kittiwake_types::ServedFrom::Nest,
        stale: false,
        age_seconds: 0,
    };
    let mut resp = axum::Json(serde_json::json!({
        "data": { "rows": answer.rows, "count": answer.count },
        "provenance": &meta,
    }))
    .into_response();
    provenance_headers(resp.headers_mut(), &meta);
    no_store(resp.headers_mut());
    Ok(resp)
}

/// Liveness: is this process answering. Deliberately says nothing about the nests, because a
/// process that is up and a service that is correct are different questions.
async fn get_health(State(st): State<AppState>) -> Response {
    axum::Json(serde_json::json!({
        "ok": true,
        "uptime_seconds": st.started.elapsed().as_secs(),
        "cached_entries": st.read.store.entry_count(),
    }))
    .into_response()
}

/// Readiness: what every nest says about itself. The archive is reported without judgement,
/// because it is stalled by design and alerting on it would cry wolf from day one.
async fn get_ready(State(st): State<AppState>) -> Response {
    let mut nests = serde_json::Map::new();
    let mut all_ready = true;

    for nest in NestId::ALL {
        let entry = match st.read.gate.ready(nest).await {
            Ok(body) => serde_json::json!({
                "ready": true,
                "archival": nest.is_archival(),
                "lag_blocks": body.lag_blocks,
                "sealed_through": body.sealed_through,
            }),
            Err(e) => {
                if !nest.is_archival() {
                    all_ready = false;
                }
                serde_json::json!({ "ready": false, "archival": nest.is_archival(), "reason": e.to_string() })
            }
        };
        nests.insert(nest.as_str().to_string(), entry);
    }

    let status = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        axum::Json(serde_json::json!({ "ready": all_ready, "nests": nests })),
    )
        .into_response()
}

async fn get_metrics(State(st): State<AppState>) -> Response {
    match st.metrics {
        Some(handle) => handle.render().into_response(),
        None => (StatusCode::NOT_FOUND, "metrics recorder not installed").into_response(),
    }
}

pub fn router(state: AppState) -> Router {
    use axum::routing::post;
    Router::new()
        .route("/whoami", get(get_whoami))
        // Also under /api, so it can be reached through the edge rewrite and the whole chain can be
        // checked from a browser rather than only from the box.
        .route("/api/whoami", get(get_whoami))
        .route("/health", get(get_health))
        .route("/ready", get(get_ready))
        .route("/metrics", get(get_metrics))
        // The directory and the network
        .route("/api/indexers", get(handlers::indexers))
        .route("/api/indexer/{address}", get(handlers::indexer))
        .route(
            "/api/indexer-status/{address}",
            get(handlers::indexer_status),
        )
        .route(
            "/api/apr-provenance/{address}",
            get(handlers::apr_provenance),
        )
        .route(
            "/api/indexer-stake-history/{address}",
            get(handlers::indexer_stake_history),
        )
        .route("/api/curators", get(handlers::curators))
        .route("/api/epochs", get(handlers::epochs))
        .route("/api/network-stats", get(handlers::network_stats))
        // Same fold, different panel: one cache entry serves both.
        .route("/api/grt-flow", get(handlers::network_stats))
        .route("/api/token-metrics", get(handlers::token_metrics))
        // Positions
        .route("/api/portfolio", get(handlers::portfolio))
        .route("/api/rewards-history", get(handlers::rewards_history))
        .route("/api/provisions", get(handlers::provisions))
        .route("/api/payments", get(handlers::payments))
        .route("/api/poi", get(handlers::poi))
        // Activity
        .route("/api/delegation-events", get(handlers::delegation_events))
        .route("/api/delegation-flows", get(handlers::delegation_flows))
        .route("/api/developer-activity", get(handlers::developer_activity))
        // Subgraphs
        .route(
            "/api/subgraph-deployments",
            get(handlers::subgraph_deployments),
        )
        .route("/api/subgraph-fees-30d", get(handlers::subgraph_fees_30d))
        .route(
            "/api/subgraph-curation/{hash}",
            get(handlers::subgraph_curation),
        )
        .route(
            "/api/subgraph-history/{hash}",
            get(handlers::subgraph_history),
        )
        .route(
            "/api/indexing-status/{hash}",
            get(handlers::indexing_status),
        )
        // Direct Indexer Payments
        .route("/api/dips", get(handlers::dips))
        // The public SQL surface
        .route("/api/indexers-enriched", get(handlers::indexers_enriched))
        .route("/api/horizon/activity", get(handlers::horizon_activity))
        .route("/api/subgraph-search", get(handlers::subgraph_search))
        .route("/api/subgraph-names", post(handlers::subgraph_names))
        .route("/api/chain-lag", get(handlers::chain_lag))
        .route("/api/dropped-chains", get(handlers::dropped_chains))
        .route("/api/reo", get(handlers::reo))
        .route(
            "/api/indexer-node-health",
            get(handlers::indexer_node_health),
        )
        .route("/api/price", get(handlers::price))
        .route("/api/tvl", get(handlers::tvl))
        // The public SQL surface
        .route("/api/sql/catalog", get(get_sql_catalog))
        .route("/api/sql/query", post(post_sql_query))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(state)
}

/// Build the state the router needs.
#[allow(clippy::too_many_arguments)]
pub fn state(
    limits: Limits,
    metrics: Option<kittiwake_obs::PrometheusHandle>,
    pool: Option<kittiwake_db::Pool>,
    read: ReadCtx,
    edge_secret: Option<String>,
) -> AppState {
    AppState {
        chain: read.chain.clone(),
        read,
        ext: kittiwake_ext::Ext::default(),
        ipfs: kittiwake_ext::Ipfs::default(),
        edge_secret: edge_secret.map(|s| Arc::from(s.as_str())),
        pool,
        limits: Arc::new(limits),
        metrics,
        started: std::time::Instant::now(),
    }
}

/// Which address the rate limiter attributed this request to.
///
/// The caller's own, so nothing is disclosed that they did not already know. Behind a proxy it is
/// the one fact that is otherwise invisible, and getting it wrong puts every visitor in the world
/// into a single rate-limit bucket while every log and dashboard says the service is healthy.
///
/// This exists because diagnosing exactly that took an afternoon of firing three hundred requests
/// at the box and inferring the answer from how many came back 429. One request answers it now.
async fn get_whoami(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let client = client_ip(peer, &headers, st.edge_secret.as_deref());
    let mut resp = axum::Json(serde_json::json!({
        // What we rate-limit by.
        "client": client.to_string(),
        // Who actually opened the connection. Loopback means something is in front of us.
        "peer": peer.ip().to_string(),
        // Whether this request proved it came through our own edge. False here with a proxy in
        // front means the forwarded chain is not being believed, which is the failure above.
        "edge_trusted": st.edge_secret.as_deref().is_some_and(|secret| {
            headers
                .get(EDGE_HEADER)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|p| p.len() == secret.len() && p == secret)
        }),
        "named_client": headers
            .get(CLIENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "forwarded_for": headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "real_ip": headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    }))
    .into_response();
    no_store(resp.headers_mut());
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(k: &str, v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
        h
    }

    const SECRET: &str = "an-edge-secret-nobody-else-has";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The regression that 400'd three routes in production for twenty minutes.
    ///
    /// Base58 is case-sensitive, so a normaliser that lowercases turns a valid CID into rubbish.
    /// Both spellings must reach the same id, because the frontend sends one and the nest stores
    /// the other.
    #[test]
    fn both_spellings_of_a_deployment_reach_the_same_id() {
        let cid = "Qmd5j2CeMs5NBUASQWjGbFmFpxGWhwKbgrTBZwV8hj65QJ";
        let hex = "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff";
        assert_eq!(crate::handlers::deployment_id(cid), hex);
        assert_eq!(crate::handlers::deployment_id(hex), hex);
        // Mixed case hex is still hex; only the CID form is case-sensitive.
        assert_eq!(crate::handlers::deployment_id(&hex.to_uppercase()), hex);
        assert_eq!(crate::handlers::deployment_id(&format!("  {cid}  ")), hex);
    }

    /// Anything that is neither is passed through for the SQL builder to refuse, rather than being
    /// mangled into something that looks plausible.
    #[test]
    fn nonsense_is_left_for_the_validator_to_refuse() {
        assert_eq!(crate::handlers::deployment_id("' OR 1=1 --"), "' or 1=1 --");
        assert_eq!(crate::handlers::deployment_id(""), "");
    }

    #[test]
    fn a_forwarded_header_from_a_stranger_is_ignored() {
        let peer: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        let h = hdr("x-forwarded-for", "1.2.3.4");
        assert_eq!(
            client_ip(peer, &h, Some(SECRET)),
            ip("203.0.113.9"),
            "a direct caller is its own address, whatever it claims"
        );
    }

    /// The one that made this whole change necessary. Behind the proxy, `X-Forwarded-For` alone is
    /// not evidence: anyone can send one. Without the edge secret the caller is Caddy's peer, which
    /// Caddy names and nobody can forge.
    #[test]
    fn a_forwarded_header_without_the_edge_secret_names_nobody() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let h = headers(&[(CLIENT_HEADER, "1.2.3.4"), ("x-real-ip", "198.51.100.7")]);
        assert_eq!(client_ip(peer, &h, Some(SECRET)), ip("198.51.100.7"));
        assert_eq!(client_ip(peer, &h, None), ip("198.51.100.7"));
    }

    /// And with it, the browser at the far end gets its own bucket - which is the entire point.
    #[test]
    fn the_edge_secret_makes_the_real_client_visible() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let h = headers(&[
            (CLIENT_HEADER, "1.2.3.4"),
            ("x-real-ip", "198.51.100.7"),
            (EDGE_HEADER, SECRET),
        ]);
        assert_eq!(client_ip(peer, &h, Some(SECRET)), ip("1.2.3.4"));
    }

    /// A wrong secret must be worth exactly as much as no secret. Anything else and guessing is
    /// cheaper than not guessing.
    #[test]
    fn a_wrong_or_truncated_secret_earns_nothing() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        for attempt in [
            "",
            "wrong",
            &SECRET[..SECRET.len() - 1],
            "an-edge-secret-nobody-else-hasX",
        ] {
            let h = headers(&[
                (CLIENT_HEADER, "1.2.3.4"),
                ("x-real-ip", "198.51.100.7"),
                (EDGE_HEADER, attempt),
            ]);
            assert_eq!(
                client_ip(peer, &h, Some(SECRET)),
                ip("198.51.100.7"),
                "{attempt:?} must not buy the right to name a client"
            );
        }
    }

    /// If the service has no secret configured, a caller presenting one must not be able to invent
    /// the comparison. Absent means absent.
    #[test]
    fn an_unconfigured_secret_cannot_be_satisfied() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let h = headers(&[
            (CLIENT_HEADER, "1.2.3.4"),
            ("x-real-ip", "198.51.100.7"),
            (EDGE_HEADER, ""),
        ]);
        assert_eq!(client_ip(peer, &h, None), ip("198.51.100.7"));
    }

    /// A proxy that sets the forwarded chain but not `X-Real-IP`: the rightmost entry is the hop
    /// nearest us, which is the same fact under a different name.
    #[test]
    fn the_rightmost_forwarded_entry_stands_in_for_a_missing_real_ip() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let h = hdr("x-forwarded-for", "1.2.3.4, 198.51.100.7");
        assert_eq!(client_ip(peer, &h, Some(SECRET)), ip("198.51.100.7"));
    }

    /// Nothing usable at all is loopback rather than a guess - one shared bucket, which is the
    /// conservative direction to fail in.
    #[test]
    fn no_usable_header_falls_back_rather_than_inventing() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert_eq!(
            client_ip(peer, &HeaderMap::new(), Some(SECRET)),
            ip("127.0.0.1")
        );
        let h = headers(&[("x-real-ip", "not an address"), (EDGE_HEADER, SECRET)]);
        assert_eq!(client_ip(peer, &h, Some(SECRET)), ip("127.0.0.1"));
    }

    #[test]
    fn the_budget_is_per_caller_and_exact() {
        let limits = Limits::new(10, 3, 5, vec![]);
        let a: IpAddr = "1.1.1.1".parse().unwrap();
        let b: IpAddr = "2.2.2.2".parse().unwrap();

        // Burst of 3 on the read budget, then refusal.
        for i in 0..3 {
            assert!(
                limits.check(&limits.read, a).is_ok(),
                "call {i} should pass"
            );
        }
        let err = limits.check(&limits.read, a).unwrap_err();
        assert_eq!(err.code(), "rate_limited");
        assert!(err.retry_after().is_some());

        // A different caller is unaffected: the limit is per key, not global.
        assert!(limits.check(&limits.read, b).is_ok());
    }

    #[test]
    fn exhausting_the_sql_budget_does_not_lock_out_the_dashboard() {
        let limits = Limits::new(1, 5, 5, vec![]);
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        while limits.check(&limits.sql, ip).is_ok() {}
        assert!(
            limits.check(&limits.read, ip).is_ok(),
            "budgets must be separate"
        );
    }

    /// The bug this guards: a burst larger than the budget makes the budget decorative.
    #[test]
    fn a_burst_can_never_exceed_the_budget_it_belongs_to() {
        let limits = Limits::new(2, 2, 2, vec![]);
        let ip: IpAddr = "9.9.9.9".parse().unwrap();
        let mut admitted = 0;
        while limits.check(&limits.read, ip).is_ok() {
            admitted += 1;
            assert!(admitted <= 2, "admitted {admitted} against a budget of 2");
        }
        assert_eq!(admitted, 2);
    }

    #[test]
    fn an_exempt_caller_is_never_refused() {
        let ip: IpAddr = "10.0.0.7".parse().unwrap();
        let limits = Limits::new(1, 1, 1, vec![ip]);
        for _ in 0..50 {
            assert!(limits.check(&limits.sql, ip).is_ok());
        }
    }

    #[test]
    fn every_error_says_no_store_so_an_edge_cannot_cache_a_failure() {
        let resp = ErrorResponse(ApiError::NestBusy { retry_after: 2 }).into_response();
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "2");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_clock_is_available_for_the_retry_hint() {
        // Guards the governor API used above; a version bump that changes it fails here loudly.
        let _ = DefaultClock::default().now();
    }
}
