//! lodestar-cockpit: the self-hosted half of Lodestar's Cockpit.
//!
//! It runs inside the indexer's own network, beside its agent. The browser signs in with the
//! indexer's or an operator's wallet and is then allowed a fixed handful of action-queue
//! operations, which this process forwards to the agent with credentials the browser never sees.

pub mod agent;
pub mod auth;
pub mod config;

use std::sync::Arc;

use alloy::primitives::Address;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::CorsLayer;

use crate::agent::{Agent, AgentError, QueueRequest};
use crate::auth::{Auth, AuthError, COOKIE, Session};

pub struct AppState {
    pub auth: Auth,
    pub agent: Agent,
    pub allowed_origin: String,
    pub secure_cookie: bool,
}

pub fn app(state: Arc<AppState>) -> Router {
    let origin: HeaderValue = state
        .allowed_origin
        .parse()
        .expect("allowed_origin is a valid header value");
    let cors = CorsLayer::new()
        .allow_origin(origin)
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE]);

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/auth/challenge", post(challenge))
        .route("/auth/verify", post(verify))
        .route("/auth/session", get(session))
        .route("/auth/logout", post(logout))
        .route("/actions", get(list_actions).post(queue_actions))
        .route("/actions/approve", post(approve_actions))
        .route("/actions/cancel", post(cancel_actions))
        .route("/rules", get(rules))
        .fallback(|| async { err(StatusCode::NOT_FOUND, "not a Cockpit route") })
        .layer(cors)
        .with_state(state)
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

fn auth_err(e: AuthError) -> Response {
    let status = match e {
        AuthError::NotAnOperator => StatusCode::FORBIDDEN,
        AuthError::Busy => StatusCode::TOO_MANY_REQUESTS,
        AuthError::Chain(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::UNAUTHORIZED,
    };
    err(status, e)
}

fn agent_err(e: AgentError) -> Response {
    let status = match e {
        AgentError::Invalid(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    err(status, e)
}

struct Refused(StatusCode, &'static str);

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        err(self.0, self.1)
    }
}

/// A state-changing request must come from the Lodestar origin. SameSite=Strict already keeps the
/// cookie off cross-site requests; this also covers a sibling origin on the same site.
fn same_origin(state: &AppState, headers: &HeaderMap) -> Result<(), Refused> {
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(o) if o == state.allowed_origin => Ok(()),
        _ => Err(Refused(StatusCode::FORBIDDEN, "wrong or missing Origin")),
    }
}

fn current(state: &AppState, headers: &HeaderMap) -> Result<Session, Refused> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .filter(|(k, _)| *k == COOKIE)
        .find_map(|(_, v)| state.auth.open(v, auth::now()))
        .ok_or(Refused(StatusCode::UNAUTHORIZED, "sign in first"))
}

fn cookie(state: &AppState, value: &str, max_age: i64) -> HeaderValue {
    let secure = if state.secure_cookie { "; Secure" } else { "" };
    format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")
        .parse()
        .expect("cookie is ASCII")
}

#[derive(Deserialize)]
struct ChallengeBody {
    address: Address,
}

async fn challenge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ChallengeBody>,
) -> Response {
    if let Err(r) = same_origin(&state, &headers) {
        return r.into_response();
    }
    match state.auth.challenge(body.address, auth::now()) {
        Ok(i) => Json(json!({
            "nonce": i.nonce,
            "message": i.message,
            "expiresAt": i.expires_at,
            "indexer": state.auth.indexer(),
        }))
        .into_response(),
        Err(e) => auth_err(e),
    }
}

#[derive(Deserialize)]
struct VerifyBody {
    nonce: String,
    signature: String,
}

async fn verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<VerifyBody>,
) -> Response {
    if let Err(r) = same_origin(&state, &headers) {
        return r.into_response();
    }
    match state
        .auth
        .verify(&body.nonce, &body.signature, auth::now())
        .await
    {
        Ok(s) => {
            tracing::info!(signer = %s.signer, "signed in");
            let token = state.auth.seal(&s);
            let mut resp = Json(&s).into_response();
            resp.headers_mut().insert(
                header::SET_COOKIE,
                cookie(&state, &token, state.auth.session_ttl()),
            );
            resp
        }
        Err(e) => {
            tracing::warn!(error = %e, "sign-in refused");
            auth_err(e)
        }
    }
}

async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match current(&state, &headers) {
        Ok(s) => Json(s).into_response(),
        Err(r) => r.into_response(),
    }
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = same_origin(&state, &headers) {
        return r.into_response();
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, cookie(&state, "", 0));
    resp
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
}

async fn list_actions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Err(r) = current(&state, &headers) {
        return r.into_response();
    }
    match state.agent.actions(q.status.as_deref()).await {
        Ok(v) => Json(json!({ "actions": v })).into_response(),
        Err(e) => agent_err(e),
    }
}

#[derive(Deserialize)]
struct QueueBody {
    actions: Vec<QueueRequest>,
}

async fn queue_actions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<QueueBody>,
) -> Response {
    let s = match same_origin(&state, &headers).and_then(|_| current(&state, &headers)) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match state.agent.queue(&body.actions).await {
        Ok(v) => {
            tracing::info!(signer = %s.signer, count = body.actions.len(), "queued actions");
            Json(json!({ "actions": v })).into_response()
        }
        Err(e) => agent_err(e),
    }
}

#[derive(Deserialize)]
struct IdsBody {
    ids: Vec<i64>,
}

async fn approve_actions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<IdsBody>,
) -> Response {
    let s = match same_origin(&state, &headers).and_then(|_| current(&state, &headers)) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match state.agent.approve(&body.ids).await {
        Ok(v) => {
            tracing::info!(signer = %s.signer, ids = ?body.ids, "approved actions");
            Json(json!({ "actions": v })).into_response()
        }
        Err(e) => agent_err(e),
    }
}

async fn cancel_actions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<IdsBody>,
) -> Response {
    let s = match same_origin(&state, &headers).and_then(|_| current(&state, &headers)) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    match state.agent.cancel(&body.ids).await {
        Ok(v) => {
            tracing::info!(signer = %s.signer, ids = ?body.ids, "cancelled actions");
            Json(json!({ "actions": v })).into_response()
        }
        Err(e) => agent_err(e),
    }
}

async fn rules(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = current(&state, &headers) {
        return r.into_response();
    }
    match state.agent.rules().await {
        Ok(v) => Json(json!({ "rules": v })).into_response(),
        Err(e) => agent_err(e),
    }
}
