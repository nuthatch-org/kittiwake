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
pub mod openapi;

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
    /// Present only when the Dock has its independently managed session secret.
    pub studio: Option<Studio>,
    pub scuttle: Option<Scuttle>,
    /// The key SQL receipts are signed with. Absent means this deployment issues none.
    pub receipt_key: Option<Arc<ed25519_dalek::SigningKey>>,
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

#[derive(Clone)]
pub struct Studio {
    pub secret: kittiwake_dock::Secret,
    pub bounty_board_address: Option<String>,
}

#[derive(Clone)]
pub struct Scuttle {
    pub salt: kittiwake_scuttle::Salt,
    pub ip_pepper: Arc<str>,
    pub admin_secret: Arc<str>,
    pub admin_session_secret: kittiwake_dock::Secret,
    pub flood: Arc<kittiwake_scuttle::FloodGuard>,
    pub room: kittiwake_scuttle::Room,
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

/// The playground's catalogue: what may be asked, and what is actually there.
///
/// The labels, descriptions and samples are static and come straight from the source. Only the
/// schema is fetched, because only the schema can be out of date: baking the static half into a
/// cache means a deploy that fixes a wrong sample does not take effect for five minutes, and
/// nothing on the page says why.
///
/// A dataset whose nest cannot be reached stays in the list with `available: false` and no tables,
/// rather than vanishing. A dataset that disappears reads as one that never existed, and the
/// visitor goes looking for what they did wrong.
async fn get_sql_catalog(State(st): State<AppState>) -> Response {
    let fetched = futures::future::join_all(kittiwake_sqlplay::DATASETS.iter().map(|d| {
        let gate = st.read.gate.clone();
        async move { (d.id, gate.tables(d.nest).await) }
    }))
    .await;

    let datasets: Vec<serde_json::Value> = kittiwake_sqlplay::DATASETS
        .iter()
        .map(|d| {
            let tables = fetched
                .iter()
                .find(|(id, _)| *id == d.id)
                .and_then(|(_, r)| r.as_ref().ok());
            let tables: Vec<serde_json::Value> = tables
                .map(|ts| ts.iter().map(sql_table).collect())
                .unwrap_or_default();
            serde_json::json!({
                "id": d.id, "label": d.label, "chain": d.chain,
                "description": d.description, "archival": d.archival,
                "sample": d.sample,
                "available": !tables.is_empty(),
                "tableCount": tables.len(),
                "tables": tables,
            })
        })
        .collect();

    // The surface as a whole is available if anything on it is. All five dark means the origin is
    // gone rather than one nest being down, and the page should say so once instead of five times.
    let available = datasets
        .iter()
        .any(|d| d["available"].as_bool().unwrap_or(false));

    let mut resp = axum::Json(serde_json::json!({ "available": available, "datasets": datasets }))
        .into_response();
    no_store(resp.headers_mut());
    resp
}

/// One table as the catalogue lists it.
///
/// `sol_type` is the Solidity type where the nest decoded one and the literal string `implicit`
/// where it did not - a block number, a log index, the things the nest adds rather than decodes. In
/// that case the storage type is the honest answer, and printing "implicit" would tell a reader
/// nothing they can write a query against.
fn sql_table(t: &serde_json::Value) -> serde_json::Value {
    let columns: Vec<serde_json::Value> = t
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    let sol = c.get("sol_type").and_then(|v| v.as_str()).unwrap_or("");
                    let ty = if sol == "implicit" {
                        c.get("storage").and_then(|v| v.as_str()).unwrap_or("")
                    } else {
                        sol
                    };
                    serde_json::json!({
                        "name": c.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "type": ty,
                        "indexed": c.get("indexed").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "name": t.get("table").and_then(|v| v.as_str()).unwrap_or(""),
        "alias": t.get("alias").and_then(|v| v.as_str()).unwrap_or(""),
        "event": t.get("event").and_then(|v| v.as_str()).unwrap_or(""),
        "columns": columns,
    })
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

/// `GET /api/sql/named`: what may be asked.
///
/// The statement is returned beside the name, deliberately. A name is stable and a statement is
/// what the nest actually answered, and a receipt over an answer wants the second.
async fn get_sql_named() -> Response {
    axum::Json(serde_json::json!({
        "queries": kittiwake_sqlplay::named::NAMED_QUERIES,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct NamedCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: serde_json::Map<String, serde_json::Value>,
}

/// `POST /api/sql/named`: ask one of them.
///
/// The caller sends a name and typed arguments, never SQL. That is the whole difference between
/// this and `/api/sql/query`: the free-form tier bounds the cost of a question, and this one bounds
/// which questions exist. #19
async fn post_sql_named(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NamedCall>,
) -> Result<Response, ErrorResponse> {
    st.limits.check(
        &st.limits.sql,
        client_ip(peer, &headers, st.edge_secret.as_deref()),
    )?;

    // Errors as prompts. A refusal that only says "no" leaves a caller guessing, and guessing at an
    // endpoint is how you get a thousand probing requests.
    let Some(query) = kittiwake_sqlplay::named::find_named_query(&body.name) else {
        let allowed: Vec<&str> = kittiwake_sqlplay::named::NAMED_QUERIES
            .iter()
            .map(|q| q.name)
            .collect();
        return Err(ApiError::BadRequest(format!(
            "no query named {:?}; allowed: {}",
            body.name,
            allowed.join(", ")
        ))
        .into());
    };

    let sql = kittiwake_sqlplay::named::render(query, &body.args)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // A declared query naming a dataset nobody exposes is our bug rather than the caller's, and a
    // test asserts it cannot happen - so this arm is a 500 and says so plainly.
    let dataset = kittiwake_sqlplay::find_dataset(query.dataset).ok_or_else(|| {
        tracing::error!(
            query = query.name,
            dataset = query.dataset,
            "named query targets an unknown dataset"
        );
        ApiError::Internal("this query is misconfigured".into())
    })?;

    let spec = kittiwake_nest::QuerySpec::read(dataset.nest, sql.clone())
        .with_timeout(Duration::from_secs(6));
    let answer = st.read.gate.query(&spec).await.map_err(ApiError::from)?;

    let meta = Meta {
        provenance: answer.provenance.clone(),
        served_from: kittiwake_types::ServedFrom::Nest,
        stale: false,
        age_seconds: 0,
    };
    let mut resp = axum::Json(serde_json::json!({
        "query": query.name,
        "dataset": dataset.id,
        // Returned so the caller can see exactly what was run, and so a receipt over this answer
        // can record the statement rather than only the name.
        "sql": sql,
        "count": answer.count,
        "rows": answer.rows,
        "provenance": &meta,
    }))
    .into_response();
    provenance_headers(resp.headers_mut(), &meta);
    no_store(resp.headers_mut());
    Ok(resp)
}

/// `POST /api/sql/receipt`: a declared query, answered and signed.
///
/// Named queries only. A receipt over arbitrary SQL attests to an answer nobody can agree the
/// question for; a declared one pinned to a block means the same thing to both parties next year,
/// which is the reason the named tier exists. #19
async fn post_sql_receipt(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NamedCall>,
) -> Result<Response, ErrorResponse> {
    st.limits.check(
        &st.limits.sql,
        client_ip(peer, &headers, st.edge_secret.as_deref()),
    )?;

    let Some(key) = st.receipt_key.clone() else {
        return Err(ApiError::Unavailable(
            "this deployment does not issue receipts; sign the answer yourself with `tattler \
             attest-named` and your own key"
                .into(),
        )
        .into());
    };

    let Some(query) = kittiwake_sqlplay::named::find_named_query(&body.name) else {
        let allowed: Vec<&str> = kittiwake_sqlplay::named::NAMED_QUERIES
            .iter()
            .map(|q| q.name)
            .collect();
        return Err(ApiError::BadRequest(format!(
            "receipts are issued for declared queries only; allowed: {}",
            allowed.join(", ")
        ))
        .into());
    };

    let sql = kittiwake_sqlplay::named::render(query, &body.args)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let dataset = kittiwake_sqlplay::find_dataset(query.dataset).ok_or_else(|| {
        tracing::error!(
            query = query.name,
            dataset = query.dataset,
            "named query targets an unknown dataset"
        );
        ApiError::Internal("this query is misconfigured".into())
    })?;

    // The block the answer is pinned to has to come from the caller's own argument. Reading it off
    // the nest's tip instead would pin the receipt to whatever the tip happened to be, which is the
    // one thing a replaying verifier cannot reproduce.
    let as_of_block = receipt_pin(&body.args).ok_or_else(|| {
        ApiError::BadRequest(
            "this query has no `before_block` argument to pin the answer to".into(),
        )
    })?;

    let spec = kittiwake_nest::QuerySpec::read(dataset.nest, sql.clone())
        .with_timeout(Duration::from_secs(6));
    let answer = st.read.gate.query(&spec).await.map_err(ApiError::from)?;

    // A truncated or degraded answer is not the answer. Signing one puts our name on a partial
    // result presented as complete, which is the single thing a receipt must never do.
    if answer.provenance.truncated || answer.provenance.degraded {
        return Err(ApiError::BadRequest(
            "the answer was truncated or served from a degraded set of tables, so it will not be \
             signed; narrow the query"
                .into(),
        )
        .into());
    }
    let Some(sealed_through) = answer.provenance.sealed_through.filter(|s| *s >= 0) else {
        return Err(ApiError::Unavailable(
            "the nest reported no sealed_through, so reproducibility cannot be established".into(),
        )
        .into());
    };

    let rows = answer.rows.clone();
    let receipt_body = receipt_body(
        &rows,
        &answer.provenance,
        dataset.id,
        sql,
        as_of_block,
        sealed_through as u64,
        query.name,
        &body.args,
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    // `issue` refuses rather than signs when the body and the rows disagree, or when the answer
    // leaned on unsealed tip data. Either is our fault and neither is the caller's.
    let receipt =
        tattler::issue(receipt_body, rows, &key).map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut resp = axum::Json(receipt).into_response();
    no_store(resp.headers_mut());
    Ok(resp)
}

/// The receipt body, assembled from one answer.
///
/// Separated from the handler so a test can build the same body and hand it to tattler's own
/// verifier. A receipt this service issues that tattler will not verify is worse than no receipt,
/// and nothing about the handler's shape would reveal it.
#[allow(clippy::too_many_arguments)]
fn receipt_body(
    rows: &[serde_json::Value],
    provenance: &kittiwake_types::Provenance,
    dataset: &str,
    sql: String,
    as_of_block: u64,
    sealed_through: u64,
    query_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<tattler::Body> {
    Ok(tattler::Body {
        nid: provenance.nid.clone(),
        dataset: dataset.to_string(),
        query: sql,
        as_of_block,
        sealed_through,
        registry_hash: provenance.registry_hash.clone(),
        result_hash: tattler::result_hash(rows)?,
        row_count: rows.len(),
        issued_at: chrono::Utc::now().to_rfc3339(),
        query_name: Some(query_name.to_string()),
        query_args: Some(receipt_args(args)),
        merkle_root: None,
    })
}

/// The block a receipt is pinned to, from the argument that named it.
fn receipt_pin(args: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    match args.get("before_block")? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The arguments as the receipt records them: every value as text, sorted by key, so two identical
/// questions serialise identically and therefore sign identically.
fn receipt_args(
    args: &serde_json::Map<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, String> {
    args.iter()
        .map(|(k, v)| {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), text)
        })
        .collect()
}

/// `GET /openapi.json`: the API described on its own terms.
///
/// Deliberately outside `/api/`, so it is not itself an entry in the document it serves and does
/// not need a summary describing the thing you are already reading.
async fn get_openapi() -> Response {
    let mut resp = axum::Json(openapi::document(env!("CARGO_PKG_VERSION"))).into_response();
    // Generated from the router at compile time, so it is exactly as fresh as the binary and there
    // is nothing for a cache to get wrong.
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=300"),
    );
    resp
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
        .route("/openapi.json", get(get_openapi))
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
        .route("/api/support", get(handlers::support))
        .route("/api/support/{number}", get(handlers::support_issue))
        .route("/api/epochs", get(handlers::epochs))
        .route("/api/network-stats", get(handlers::network_stats))
        // Same fold, different panel: one cache entry serves both.
        .route("/api/grt-flow", get(handlers::grt_flow))
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
        // The Dock. These are deliberately individual registrations: the parity extractor sees
        // the real public surface, and a new route cannot wander into a rewrite unchecked.
        .route(
            "/api/studio/auth",
            get(handlers::studio_auth_get)
                .post(handlers::studio_auth_post)
                .delete(handlers::studio_auth_delete),
        )
        .route(
            "/api/studio/subgraphs",
            get(handlers::studio_subgraphs_get).post(handlers::studio_subgraphs_post),
        )
        .route(
            "/api/studio/subgraphs/{id}",
            axum::routing::patch(handlers::studio_subgraph_patch)
                .delete(handlers::studio_subgraph_delete),
        )
        .route(
            "/api/studio/deploy-key",
            get(handlers::studio_deploy_key_get).post(handlers::studio_deploy_key_post),
        )
        .route("/api/studio/metadata", post(handlers::studio_metadata_post))
        .route("/api/studio/node", post(handlers::studio_node_post))
        .route(
            "/api/studio/ipfs/{*path}",
            get(handlers::studio_ipfs_proxy).post(handlers::studio_ipfs_proxy),
        )
        .route(
            "/api/studio/bounties",
            get(handlers::studio_bounties_get).post(handlers::studio_bounties_post),
        )
        .route(
            "/api/studio/bounties/{id}",
            axum::routing::patch(handlers::studio_bounty_patch),
        )
        // The public SQL surface
        .route("/api/sql/catalog", get(get_sql_catalog))
        .route("/api/sql/query", post(post_sql_query))
        .route("/api/sql/named", get(get_sql_named).post(post_sql_named))
        .route("/api/sql/receipt", post(post_sql_receipt))
        // The long tail's Postgres reads. #67
        .route(
            "/api/indexer-disputes/{address}",
            get(handlers::indexer_disputes),
        )
        .route(
            "/api/parameter-history/{address}",
            get(handlers::parameter_history),
        )
        .route(
            "/api/indexer/{address}/revenue",
            get(handlers::indexer_revenue),
        )
        .route("/api/indexer/{address}/pnl", get(handlers::indexer_pnl))
        .route(
            "/api/subgraph-schema/{hash}",
            get(handlers::subgraph_schema),
        )
        .route("/api/dips/agreements", get(handlers::dips_agreements))
        .route("/api/ens", get(handlers::ens))
        .route(
            "/api/subgraph-versions/{hash}",
            get(handlers::subgraph_versions),
        )
        // Scuttlebutt is deliberately separate from the provenance envelope routes above: messages
        // are primary user data in Postgres, not a view of a sealed protocol nest.
        .route(
            "/api/scuttlebutt/messages",
            get(handlers::scuttle_messages).post(handlers::scuttle_post),
        )
        .route(
            "/api/scuttlebutt/messages/{id}",
            axum::routing::delete(handlers::scuttle_delete),
        )
        .route("/api/scuttlebutt/stream", get(handlers::scuttle_stream))
        .route(
            "/api/scuttlebutt/bans",
            get(handlers::scuttle_bans).post(handlers::scuttle_ban),
        )
        .route(
            "/api/scuttlebutt/admin/messages",
            get(handlers::scuttle_admin_messages),
        )
        .route(
            "/api/scuttlebutt/admin/login",
            post(handlers::scuttle_admin_login),
        )
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
    studio: Option<Studio>,
    ipfs: kittiwake_ext::Ipfs,
    scuttle: Option<Scuttle>,
    receipt_key: Option<Arc<ed25519_dalek::SigningKey>>,
) -> AppState {
    AppState {
        chain: read.chain.clone(),
        read,
        ext: kittiwake_ext::Ext::default(),
        ipfs,
        studio,
        scuttle,
        receipt_key,
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

    // ── The long tail ────────────────────────────────────────────────────────

    const SCHEMA_CID: &str = "QmUxJ9QYbNbDTNkQPHzq4ivqzGyMbwZBvXhrQfPTLE1Pjq";
    const DEPLOYMENT_CID: &str = "Qmbsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB";

    /// Base58 has no `0`, `O`, `I` or `l`, and it is case-sensitive. Normalising a near-miss into
    /// something plausible is how a wrong document gets served confidently.
    #[test]
    fn only_a_real_cidv0_is_accepted() {
        assert_eq!(
            crate::handlers::cid_v0(DEPLOYMENT_CID),
            Some(DEPLOYMENT_CID)
        );
        assert_eq!(
            crate::handlers::cid_v0(&format!("  {DEPLOYMENT_CID}  ")),
            Some(DEPLOYMENT_CID),
            "surrounding space is not part of the hash"
        );
        for bad in [
            "",
            "Qm",
            &DEPLOYMENT_CID[..45],
            &format!("{DEPLOYMENT_CID}x"),
            // A bytes32 deployment id is the other spelling and is not a CID.
            "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff",
            // `0`, `O`, `I` and `l` are not in the alphabet, so these are 46 characters of
            // something else.
            "Qm0sc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB",
            "QmOsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB",
            "QmIsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB",
            "Qmlsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRsMB",
            "Qm bsc6XQWbiv4DfLVfaNciScqYLyDWUYjWzrFBbzzmRs",
        ] {
            assert_eq!(crate::handlers::cid_v0(bad), None, "{bad:?} is not a CIDv0");
        }
    }

    fn manifest_with(schema: &str, source: &str) -> String {
        format!(
            "specVersion: 0.0.5\nschema:\n  file:\n    /: /ipfs/{schema}\ndataSources:\n  \
             - kind: ethereum\n    mapping:\n      file:\n        /: /ipfs/{source}\n"
        )
    }

    /// The schema reference sits above `dataSources`, and every mapping below it is also a CID.
    /// Searching the whole document would return whichever came first in the file.
    #[test]
    fn the_schema_hash_is_taken_from_above_the_data_sources() {
        let manifest = manifest_with(SCHEMA_CID, DEPLOYMENT_CID);
        assert_eq!(crate::handlers::schema_cid(&manifest), Some(SCHEMA_CID));
    }

    #[test]
    fn a_manifest_with_no_schema_reference_yields_nothing() {
        assert_eq!(
            crate::handlers::schema_cid("specVersion: 0.0.5\ndataSources: []\n"),
            None
        );
        assert_eq!(crate::handlers::schema_cid(""), None);
        // A truncated hash is not a hash, and half of one must not be returned as if it were.
        assert_eq!(
            crate::handlers::schema_cid("schema:\n  file:\n    /: /ipfs/Qmtooshort\n"),
            None
        );
    }
    // ── SQL receipts ─────────────────────────────────────────────────────────

    fn receipt_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    fn answer_rows() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"indexer": "0xaa", "tokens": "1000000000000000000"}),
            serde_json::json!({"indexer": "0xbb", "tokens": "2000000000000000000"}),
        ]
    }

    fn sealed_provenance() -> kittiwake_types::Provenance {
        kittiwake_types::Provenance {
            as_of: Some(497_000_000),
            sealed_through: Some(497_000_000),
            registry_hash: Some("0xreg".into()),
            nid: Some("protocol".into()),
            source: None,
            truncated: false,
            degraded: false,
        }
    }

    fn issued() -> tattler::Receipt {
        let rows = answer_rows();
        let body = receipt_body(
            &rows,
            &sealed_provenance(),
            "protocol",
            "SELECT 1".into(),
            497_000_000,
            497_000_000,
            "delegations_to_indexer",
            &serde_json::Map::from_iter([
                ("before_block".to_string(), serde_json::json!(497_000_000)),
                ("indexer".to_string(), serde_json::json!("0xAA")),
            ]),
        )
        .unwrap();
        tattler::issue(body, rows, &receipt_key()).unwrap()
    }

    /// The only claim that matters. A receipt this service issues has to satisfy tattler's own
    /// verifier, because a reader checking one will use tattler and not us.
    #[test]
    fn a_receipt_we_issue_verifies_with_tattlers_own_verifier() {
        assert_eq!(tattler::verify(&issued()).unwrap(), tattler::Verdict::Ok);
    }

    #[test]
    fn a_row_edited_after_signing_is_caught() {
        let mut receipt = issued();
        receipt.rows[0]["tokens"] = serde_json::json!("9000000000000000000");
        assert!(matches!(
            tattler::verify(&receipt).unwrap(),
            tattler::Verdict::RowsAltered { .. }
        ));
    }

    /// The signature covers the body, so editing the question it claims to answer must not survive
    /// either. Rows alone would still hash correctly.
    #[test]
    fn a_body_edited_after_signing_is_caught() {
        let mut receipt = issued();
        receipt.body.query = "SELECT 2".into();
        assert_eq!(
            tattler::verify(&receipt).unwrap(),
            tattler::Verdict::BadSignature
        );
    }

    /// Row order is not part of the answer, and a nest is free to return the same set either way.
    /// A receipt that disagreed with itself over ordering would report a forgery that never was.
    #[test]
    fn the_same_rows_in_another_order_are_the_same_answer() {
        let mut reversed = answer_rows();
        reversed.reverse();
        assert_eq!(
            tattler::result_hash(&answer_rows()).unwrap(),
            tattler::result_hash(&reversed).unwrap()
        );
    }

    #[test]
    fn the_pin_comes_from_the_argument_and_nothing_else() {
        let pin = |v: serde_json::Value| {
            receipt_pin(&serde_json::Map::from_iter([(
                "before_block".to_string(),
                v,
            )]))
        };
        assert_eq!(pin(serde_json::json!(497_000_000)), Some(497_000_000));
        assert_eq!(pin(serde_json::json!(" 497000000 ")), Some(497_000_000));
        for refused in [
            serde_json::json!("latest"),
            serde_json::json!(-1),
            serde_json::json!(4.5),
            serde_json::Value::Null,
        ] {
            assert_eq!(pin(refused.clone()), None, "{refused} is not a block");
        }
        assert_eq!(receipt_pin(&serde_json::Map::new()), None);
    }

    /// Two identical questions have to serialise identically or they sign differently, so the
    /// arguments are sorted and every value is text whatever it arrived as.
    #[test]
    fn the_recorded_arguments_are_sorted_and_textual() {
        let args = receipt_args(&serde_json::Map::from_iter([
            ("indexer".to_string(), serde_json::json!("0xAA")),
            ("before_block".to_string(), serde_json::json!(497_000_000)),
        ]));
        assert_eq!(
            args.into_iter().collect::<Vec<_>>(),
            vec![
                ("before_block".to_string(), "497000000".to_string()),
                ("indexer".to_string(), "0xAA".to_string()),
            ]
        );
    }

    // ── Scuttlebutt moderation ───────────────────────────────────────────────
    //
    // The parity harness excuses every administrator route, so these are the only thing standing
    // between the moderation surface and nobody having checked it.

    fn admin_secret() -> kittiwake_dock::Secret {
        kittiwake_dock::Secret::new("a-scuttlebutt-session-secret-of-adequate-length").unwrap()
    }

    fn cookie(value: &str) -> HeaderMap {
        headers(&[("cookie", value)])
    }

    const NOW: i64 = 1_760_000_000;

    #[test]
    fn a_moderator_with_a_valid_cookie_may_read() {
        let token = kittiwake_dock::create_session(&admin_secret(), "admin", NOW);
        let h = cookie(&format!("sb_admin={token}"));
        assert!(crate::handlers::scuttle_admin_ok(
            &admin_secret(),
            &h,
            false,
            NOW
        ));
    }

    /// A cookie rides along on any same-site request, so reading is not enough to authorise a
    /// deletion. Without the header the same valid session must buy nothing.
    #[test]
    fn a_mutation_needs_the_header_as_well_as_the_cookie() {
        let token = kittiwake_dock::create_session(&admin_secret(), "admin", NOW);
        let signed_in = cookie(&format!("sb_admin={token}"));
        assert!(!crate::handlers::scuttle_admin_ok(
            &admin_secret(),
            &signed_in,
            true,
            NOW
        ));

        let mut with_header = signed_in.clone();
        with_header.insert("x-sb-admin", HeaderValue::from_static("1"));
        assert!(crate::handlers::scuttle_admin_ok(
            &admin_secret(),
            &with_header,
            true,
            NOW
        ));
    }

    #[test]
    fn nothing_short_of_a_valid_session_gets_in() {
        let token = kittiwake_dock::create_session(&admin_secret(), "admin", NOW);
        let other = kittiwake_dock::Secret::new("a-different-secret-of-adequate-length!!").unwrap();
        let cases: Vec<(&str, HeaderMap)> = vec![
            ("no cookie at all", HeaderMap::new()),
            (
                "a cookie for some other name",
                cookie(&format!("sb_session={token}")),
            ),
            // `sb_adminish` starts with `sb_admin`, and a prefix match would accept it.
            (
                "a cookie whose name merely starts the same",
                cookie(&format!("sb_adminish={token}")),
            ),
            (
                "a truncated token",
                cookie(&format!("sb_admin={}", &token[..token.len() - 1])),
            ),
            (
                "a token signed for somebody else",
                cookie(&format!(
                    "sb_admin={}",
                    kittiwake_dock::create_session(&admin_secret(), "not-admin", NOW)
                )),
            ),
            (
                "a token signed with another secret",
                cookie(&format!(
                    "sb_admin={}",
                    kittiwake_dock::create_session(&other, "admin", NOW)
                )),
            ),
            ("no token, just the name", cookie("sb_admin=")),
        ];
        for (what, h) in cases {
            assert!(
                !crate::handlers::scuttle_admin_ok(&admin_secret(), &h, false, NOW),
                "{what} must not authorise a moderator"
            );
        }
    }

    // ── The Dock ─────────────────────────────────────────────────────────────
    //
    // The nine Studio routes landed in #64 without coverage for the three things standing between
    // them and a bad request: the slug format, the bounty amount, and the session cookie.

    const DOCK_NOW: i64 = 1_760_000_000;

    fn dock_secret() -> kittiwake_dock::Secret {
        kittiwake_dock::Secret::new("a-studio-session-secret-of-adequate-length").unwrap()
    }

    fn dock_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    /// A slug is the address the frontend routes on, so anything that is not exactly `org/name` in
    /// lowercase is refused rather than normalised into something plausible.
    #[test]
    fn a_slug_is_two_lowercase_parts_and_nothing_else() {
        for good in ["acme/uniswap-v3", "a/b", "org-1/name-2", "0x/9"] {
            assert!(crate::handlers::valid_slug(good), "{good} is a valid slug");
        }
        for bad in [
            "",
            "acme",
            "acme/",
            "/uniswap",
            "Acme/uniswap",
            "acme/Uniswap",
            "acme/uni swap",
            "acme/uni_swap",
            "acme/a/b",
            "../etc/passwd",
            "acme/uniswap!",
        ] {
            assert!(
                !crate::handlers::valid_slug(bad),
                "{bad:?} is not a valid slug"
            );
        }
    }

    /// A session that has run out is refused rather than renewed by being presented.
    #[test]
    fn a_session_older_than_its_lifetime_is_refused() {
        let token = kittiwake_dock::create_session(&admin_secret(), "admin", NOW);
        let h = cookie(&format!("sb_admin={token}"));
        let expired = NOW + kittiwake_dock::SESSION_TTL_SECS + 1;
        assert!(crate::handlers::scuttle_admin_ok(
            &admin_secret(),
            &h,
            false,
            expired - 2
        ));
        assert!(!crate::handlers::scuttle_admin_ok(
            &admin_secret(),
            &h,
            false,
            expired
        ));
    }

    /// Cookies arrive as one header, and the moderation cookie is rarely the only one in it.
    #[test]
    fn the_cookie_is_found_among_its_neighbours() {
        let h = cookie("theme=dark; sb_admin=wanted; other=x");
        assert_eq!(
            crate::handlers::scuttle_cookie(&h, "sb_admin"),
            Some("wanted")
        );
        assert_eq!(crate::handlers::scuttle_cookie(&h, "absent"), None);
    }

    /// The regression this test exists for. `amount_grt` is text in the table and the public board
    /// orders by `amount_grt::numeric`; Postgres 16 parses Infinity, so `"inf"` from one signed-in
    /// caller would sit at the top of everyone's board permanently.
    #[test]
    fn an_infinite_bounty_is_not_a_positive_number() {
        for refused in ["inf", "-inf", "infinity", "Infinity", "1e400", "NaN", "nan"] {
            assert!(
                !crate::handlers::positive_amount(refused),
                "{refused:?} must not be accepted as an amount"
            );
        }
        for accepted in ["1", "0.5", "1000000", "1e6"] {
            assert!(
                crate::handlers::positive_amount(accepted),
                "{accepted:?} is a real amount"
            );
        }
        // And the boundary, which was already right and stays that way.
        for refused in ["0", "-1", "", "one", "1 GRT"] {
            assert!(!crate::handlers::positive_amount(refused), "{refused:?}");
        }
    }

    /// The session cookie is the whole of the Dock's authorisation, so every owner-scoped route is
    /// exactly as strong as this is.
    #[test]
    fn a_studio_session_names_an_address_or_nobody() {
        let token = kittiwake_dock::create_session(&dock_secret(), "0xAbCd", DOCK_NOW);
        let other = kittiwake_dock::Secret::new("a-different-secret-of-adequate-length!!").unwrap();
        let named = |h: &HeaderMap| crate::handlers::session_address(&dock_secret(), h, DOCK_NOW);

        // Lowercased, because that is what every owner column stores.
        assert_eq!(
            named(&dock_cookie(&format!("studio_session={token}"))).as_deref(),
            Some("0xabcd")
        );
        // And found among its neighbours, which is the normal case in a browser.
        assert_eq!(
            named(&dock_cookie(&format!(
                "theme=dark; studio_session={token}; x=1"
            )))
            .as_deref(),
            Some("0xabcd")
        );

        assert_eq!(named(&HeaderMap::new()), None);
        assert_eq!(named(&dock_cookie("studio_session=")), None);
        assert_eq!(
            named(&dock_cookie(&format!(
                "studio_session={}",
                kittiwake_dock::create_session(&other, "0xAbCd", DOCK_NOW)
            ))),
            None,
            "a token signed with another secret is not a session"
        );
        assert_eq!(
            crate::handlers::session_address(
                &dock_secret(),
                &dock_cookie(&format!("studio_session={token}")),
                DOCK_NOW + kittiwake_dock::SESSION_TTL_SECS + 1
            ),
            None,
            "an expired session stays expired"
        );
    }

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
