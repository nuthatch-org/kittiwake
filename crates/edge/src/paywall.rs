//! The paywall: paid routes answer only a signed-in wallet whose subscription is current.
//!
//! Identity is the Dock's session cookie, so a subscriber signs in once for both. The entitlement
//! is one `paid_until` per wallet in Postgres, cached here for a minute so a page that fans out to
//! twenty routes costs one row read rather than twenty.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kittiwake_types::ApiError;

use crate::{AppState, EDGE_HEADER, ErrorResponse, client_ip};

const CACHE_TTL: Duration = Duration::from_secs(60);

/// Set only on the paywall's own refusals, so the frontend can tell "subscribe" apart from the
/// Dock's 401 for a visitor who has not signed in to it.
pub const REFUSAL_HEADER: &str = "x-kittiwake-paywall";

type Entitlements = HashMap<String, (Option<i64>, Instant)>;

#[derive(Clone, Default)]
pub struct Paywall {
    cache: Arc<Mutex<Entitlements>>,
    service_tokens: Arc<Vec<String>>,
}

impl Paywall {
    pub fn with_service_tokens(tokens: Vec<String>) -> Self {
        Self {
            service_tokens: Arc::new(tokens.into_iter().filter(|t| !t.is_empty()).collect()),
            ..Self::default()
        }
    }

    fn cached(&self, address: &str) -> Option<Option<i64>> {
        let cache = self.cache.lock().unwrap();
        cache
            .get(address)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(paid_until, _)| *paid_until)
    }

    fn remember(&self, address: &str, paid_until: Option<i64>) {
        let mut cache = self.cache.lock().unwrap();
        // Bounded by the number of distinct paying-or-trying wallets; drop the stale ones whenever
        // it grows rather than running a sweeper for a map this small.
        if cache.len() > 10_000 {
            cache.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
        }
        cache.insert(address.to_string(), (paid_until, Instant::now()));
    }

    /// Forget a wallet, so a payment that just landed is honoured on the next request.
    pub fn forget(&self, address: &str) {
        self.cache.lock().unwrap().remove(&address.to_lowercase());
    }
}

/// Who gets through, decided on facts alone so it can be tested without a router.
///
/// `trusted` covers our own server-side callers: the link-preview functions presenting the edge
/// secret, and the exempt addresses (Foghorn's hourly read). Neither carries a wallet.
pub(crate) fn admit(
    trusted: bool,
    address: Option<&str>,
    paid_until: Option<i64>,
    now: i64,
) -> Result<(), ApiError> {
    if trusted {
        return Ok(());
    }
    if address.is_none() {
        return Err(ApiError::Unauthorized);
    }
    match paid_until {
        Some(until) if until > now => Ok(()),
        _ => Err(ApiError::PaymentRequired),
    }
}

fn edge_trusted(headers: &HeaderMap, secret: Option<&str>) -> bool {
    let Some(secret) = secret else { return false };
    headers
        .get(EDGE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| {
            bool::from(subtle::ConstantTimeEq::ct_eq(
                p.as_bytes(),
                secret.as_bytes(),
            ))
        })
}

fn service_trusted(headers: &HeaderMap, tokens: &[String]) -> bool {
    let Some(presented) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    tokens.iter().any(|t| {
        bool::from(subtle::ConstantTimeEq::ct_eq(
            presented.as_bytes(),
            t.as_bytes(),
        ))
    })
}

pub(crate) async fn require_subscription(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let Some(paywall) = st.paywall.clone() else {
        return next.run(req).await;
    };
    match decide(&st, &paywall, peer, req.headers()).await {
        Ok(()) => next.run(req).await,
        Err(e) => {
            let reason = match e {
                ApiError::PaymentRequired => {
                    metrics::counter!("paywall_refused_total").increment(1);
                    Some("subscribe")
                }
                ApiError::Unauthorized => Some("sign-in"),
                _ => None,
            };
            let mut resp = ErrorResponse(e).into_response();
            if let Some(reason) = reason {
                resp.headers_mut()
                    .insert(REFUSAL_HEADER, axum::http::HeaderValue::from_static(reason));
            }
            resp
        }
    }
}

async fn decide(
    st: &AppState,
    paywall: &Paywall,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let secret = st.edge_secret.as_deref();
    let trusted = edge_trusted(headers, secret)
        || service_trusted(headers, &paywall.service_tokens)
        || st.limits.exempt.contains(&client_ip(peer, headers, secret));
    if trusted {
        return Ok(());
    }
    let studio = st
        .studio
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("sign-in is not configured".into()))?;
    let now = crate::handlers::unix_now();
    let address = crate::handlers::session_address(&studio.secret, headers, now);
    let Some(address) = address else {
        return admit(false, None, None, now);
    };
    let paid_until = match paywall.cached(&address) {
        Some(p) => p,
        None => {
            let pool = st
                .pool
                .as_ref()
                .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;
            let p = kittiwake_db::subscription_paid_until(pool, &address)
                .await
                .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;
            paywall.remember(&address, p);
            p
        }
    };
    admit(false, Some(&address), paid_until, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000;

    #[test]
    fn a_visitor_who_has_not_signed_in_is_asked_to() {
        assert!(matches!(
            admit(false, None, None, NOW),
            Err(ApiError::Unauthorized)
        ));
        // A stale entitlement for nobody in particular must not stand in for a session.
        assert!(matches!(
            admit(false, None, Some(NOW + 1_000), NOW),
            Err(ApiError::Unauthorized)
        ));
    }

    #[test]
    fn a_wallet_that_never_paid_or_has_lapsed_is_asked_to_pay() {
        assert!(matches!(
            admit(false, Some("0xa"), None, NOW),
            Err(ApiError::PaymentRequired)
        ));
        assert!(matches!(
            admit(false, Some("0xa"), Some(NOW - 1), NOW),
            Err(ApiError::PaymentRequired)
        ));
        // The last second is not included: paid_until is when access ends.
        assert!(matches!(
            admit(false, Some("0xa"), Some(NOW), NOW),
            Err(ApiError::PaymentRequired)
        ));
    }

    #[test]
    fn a_current_subscriber_and_our_own_callers_get_through() {
        assert!(admit(false, Some("0xa"), Some(NOW + 1), NOW).is_ok());
        assert!(admit(true, None, None, NOW).is_ok());
    }

    #[test]
    fn the_edge_secret_must_match_exactly() {
        let mut h = HeaderMap::new();
        assert!(!edge_trusted(&h, Some("s3cret")));
        h.insert(EDGE_HEADER, "s3cre".parse().unwrap());
        assert!(!edge_trusted(&h, Some("s3cret")));
        h.insert(EDGE_HEADER, "s3cret".parse().unwrap());
        assert!(edge_trusted(&h, Some("s3cret")));
        // No secret configured means nobody is trusted, whatever they send.
        assert!(!edge_trusted(&h, None));
    }

    #[test]
    fn only_a_configured_service_token_is_trusted() {
        let tokens = vec!["svc-token-foghorn".to_string()];
        let mut h = HeaderMap::new();
        assert!(!service_trusted(&h, &tokens));
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer svc-token-foghor".parse().unwrap(),
        );
        assert!(!service_trusted(&h, &tokens));
        h.insert(
            axum::http::header::AUTHORIZATION,
            "svc-token-foghorn".parse().unwrap(),
        );
        assert!(!service_trusted(&h, &tokens), "the scheme is required");
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer svc-token-foghorn".parse().unwrap(),
        );
        assert!(service_trusted(&h, &tokens));
        assert!(
            !service_trusted(&h, &[]),
            "no tokens configured trusts nobody"
        );
        // An empty token in the config must not admit an empty bearer.
        let p = Paywall::with_service_tokens(vec![String::new()]);
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer ".parse().unwrap(),
        );
        assert!(!service_trusted(&h, &p.service_tokens));
    }

    #[test]
    fn a_cached_answer_expires_and_can_be_forgotten() {
        let p = Paywall::default();
        assert_eq!(p.cached("0xa"), None);
        p.remember("0xa", Some(NOW));
        assert_eq!(p.cached("0xa"), Some(Some(NOW)));
        p.forget("0xA");
        assert_eq!(p.cached("0xa"), None);
    }
}
