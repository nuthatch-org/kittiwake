//! Stripe's webhook: the one way a card payment becomes `paid_until`.
//!
//! Checkout is a Stripe Payment Link carrying the wallet as `client_reference_id`, so the first
//! event names the wallet and every renewal after it is found by the subscription id stored then.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use kittiwake_types::ApiError;
use serde_json::Value;
use sha2::Sha256;

use crate::{AppState, ErrorResponse};

/// Stripe's own default: an event signed longer ago than this is refused as a replay.
const TOLERANCE_SECS: i64 = 300;
/// The first charge covers a month; this carries it until the first `invoice.paid` names the
/// period exactly, whichever order the two arrive in.
const FIRST_PERIOD_SECS: i64 = 32 * 86_400;
/// A renewal that fails and is retried by Stripe should not lock a subscriber out meanwhile.
const GRACE_SECS: i64 = 86_400;

/// A wallet paying for the first time, and not one of ours.
pub(crate) fn is_news(inserted: bool, address: &str, ignore: &[String]) -> bool {
    inserted && !ignore.iter().any(|a| a.eq_ignore_ascii_case(address))
}

pub(crate) fn new_subscriber_message(address: &str, paid_until: i64) -> String {
    let until = chrono::DateTime::from_timestamp(paid_until, 0)
        .map(|d| d.format("%-d %B %Y").to_string())
        .unwrap_or_default();
    format!(
        "New Lodestar subscriber: {address}, paid until {until}.\nhttps://arbiscan.io/address/{address}"
    )
}

/// `Stripe-Signature: t=<unix>,v1=<hex hmac>[,v1=...]` over `"{t}.{body}"`.
pub(crate) fn verify_signature(secret: &str, header: &str, body: &[u8], now: i64) -> bool {
    let mut ts = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => ts = v.parse::<i64>().ok(),
            Some(("v1", v)) => sigs.push(v),
            _ => {}
        }
    }
    let Some(ts) = ts else { return false };
    if (now - ts).abs() > TOLERANCE_SECS {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    sigs.iter().any(|s| {
        bool::from(subtle::ConstantTimeEq::ct_eq(
            s.as_bytes(),
            expected.as_bytes(),
        ))
    })
}

/// What an event asks for. `address` is absent on renewals, which are found by `external_ref`.
#[derive(Debug, PartialEq)]
pub(crate) struct Credit {
    pub address: Option<String>,
    pub external_ref: Option<String>,
    pub paid_until: i64,
    pub email: Option<String>,
}

fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read an event, or `None` for one that extends nothing.
pub(crate) fn credit_for(event: &Value) -> Option<Credit> {
    let obj = &event["data"]["object"];
    match event["type"].as_str()? {
        "checkout.session.completed" => {
            if obj["mode"] != "subscription" || obj["payment_status"] != "paid" {
                return None;
            }
            let address = obj["client_reference_id"]
                .as_str()
                .filter(|a| is_address(a))?
                .to_lowercase();
            Some(Credit {
                address: Some(address),
                external_ref: obj["subscription"].as_str().map(String::from),
                paid_until: event["created"].as_i64()? + FIRST_PERIOD_SECS,
                email: obj["customer_details"]["email"].as_str().map(String::from),
            })
        }
        "invoice.paid" => {
            // Where the subscription id lives moved in Stripe's 2025 API versions; read both.
            let sub = obj["subscription"]
                .as_str()
                .or_else(|| obj["parent"]["subscription_details"]["subscription"].as_str())?;
            let end = obj["lines"]["data"]
                .as_array()?
                .iter()
                .filter_map(|l| l["period"]["end"].as_i64())
                .max()?;
            Some(Credit {
                address: None,
                external_ref: Some(sub.to_string()),
                paid_until: end + GRACE_SECS,
                email: obj["customer_email"].as_str().map(String::from),
            })
        }
        _ => None,
    }
}

pub(crate) async fn stripe_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ErrorResponse> {
    let secret = st
        .stripe_webhook_secret
        .as_deref()
        .ok_or_else(|| ApiError::Unavailable("card payments are not configured".into()))?;
    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(secret, sig, &body, crate::handlers::unix_now()) {
        return Err(ApiError::BadRequest("signature".into()).into());
    }
    let event: Value =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("not json".into()))?;
    let ok = || axum::Json(serde_json::json!({ "received": true })).into_response();
    let Some(credit) = credit_for(&event) else {
        return Ok(ok());
    };
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;
    let db = kittiwake_db::unavailable::<kittiwake_db::DbError>;

    let address = match (&credit.address, &credit.external_ref) {
        (Some(a), _) => Some(a.clone()),
        (None, Some(r)) => kittiwake_db::subscription_address_for_ref(pool, r)
            .await
            .map_err(db)?,
        (None, None) => None,
    };
    // A renewal for a subscription we never saw begin. Acknowledged rather than retried: the
    // checkout event that names the wallet carries its own month.
    let Some(address) = address else {
        tracing::warn!(event = ?event["id"], "stripe payment for an unknown subscription");
        return Ok(ok());
    };
    let inserted = kittiwake_db::extend_subscription(
        pool,
        &address,
        credit.paid_until,
        "stripe",
        credit.external_ref.as_deref(),
        credit.email.as_deref(),
    )
    .await
    .map_err(db)?;
    if let Some(p) = &st.paywall {
        p.forget(&address);
    }
    if let Some(t) = st.telegram.clone()
        && is_news(inserted, &address, &t.ignore_addresses)
    {
        let text = new_subscriber_message(&address, credit.paid_until);
        // Off the webhook's path: Stripe should not retry a payment because Telegram was slow.
        tokio::spawn(async move {
            if let Err(e) = crate::telegram::send(&t, &text).await {
                tracing::warn!(error = %e, "could not announce a new subscriber on Telegram");
            }
        });
    }
    metrics::counter!("stripe_credits_total").increment(1);
    Ok(ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &str = "whsec_test";
    const NOW: i64 = 1_790_000_000;

    fn sign(t: i64, body: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(format!("{t}.{body}").as_bytes());
        format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn only_a_fresh_event_signed_with_our_secret_is_believed() {
        let body = r#"{"id":"evt_1"}"#;
        assert!(verify_signature(
            SECRET,
            &sign(NOW, body),
            body.as_bytes(),
            NOW
        ));
        assert!(!verify_signature(
            "whsec_other",
            &sign(NOW, body),
            body.as_bytes(),
            NOW
        ));
        assert!(!verify_signature(
            SECRET,
            &sign(NOW, body),
            br#"{"id":"evt_2"}"#,
            NOW
        ));
        assert!(
            !verify_signature(SECRET, &sign(NOW - 301, body), body.as_bytes(), NOW),
            "a replay from six minutes ago"
        );
        assert!(!verify_signature(
            SECRET,
            "v1=deadbeef",
            body.as_bytes(),
            NOW
        ));
        assert!(!verify_signature(SECRET, "", body.as_bytes(), NOW));
    }

    #[test]
    fn a_completed_checkout_credits_the_wallet_it_names() {
        let e = json!({
            "type": "checkout.session.completed", "created": NOW,
            "data": {"object": {
                "mode": "subscription", "payment_status": "paid",
                "client_reference_id": "0xAbCdEf0123456789abcdef0123456789ABCDEF01",
                "subscription": "sub_1", "customer_details": {"email": "a@b.c"}
            }}
        });
        assert_eq!(
            credit_for(&e),
            Some(Credit {
                address: Some("0xabcdef0123456789abcdef0123456789abcdef01".into()),
                external_ref: Some("sub_1".into()),
                paid_until: NOW + FIRST_PERIOD_SECS,
                email: Some("a@b.c".into()),
            })
        );
    }

    #[test]
    fn a_checkout_without_a_wallet_or_without_payment_credits_nobody() {
        let base = |id: Value, status: &str| {
            json!({"type": "checkout.session.completed", "created": NOW, "data": {"object": {
                "mode": "subscription", "payment_status": status, "client_reference_id": id
            }}})
        };
        assert_eq!(credit_for(&base(Value::Null, "paid")), None);
        assert_eq!(credit_for(&base(json!("not-a-wallet"), "paid")), None);
        assert_eq!(
            credit_for(&base(
                json!("0xabcdef0123456789abcdef0123456789abcdef01"),
                "unpaid"
            )),
            None
        );
    }

    #[test]
    fn a_renewal_runs_to_the_end_of_its_period_in_either_api_shape() {
        let old = json!({"type": "invoice.paid", "data": {"object": {
            "subscription": "sub_1",
            "lines": {"data": [{"period": {"end": NOW + 100}}, {"period": {"end": NOW + 200}}]}
        }}});
        let new = json!({"type": "invoice.paid", "data": {"object": {
            "parent": {"subscription_details": {"subscription": "sub_1"}},
            "lines": {"data": [{"period": {"end": NOW + 200}}]}
        }}});
        for e in [old, new] {
            let c = credit_for(&e).unwrap();
            assert_eq!(c.address, None);
            assert_eq!(c.external_ref.as_deref(), Some("sub_1"));
            assert_eq!(c.paid_until, NOW + 200 + GRACE_SECS);
        }
    }

    #[test]
    fn only_a_first_payment_from_somebody_else_is_news() {
        let me = vec!["0x7dfb2175a77e922f060bc0a59f83be6d2f4b85d4".to_string()];
        assert!(is_news(true, "0xabc", &me));
        assert!(
            !is_news(false, "0xabc", &me),
            "a renewal is not a new subscriber"
        );
        assert!(!is_news(
            true,
            "0x7DFB2175A77E922F060BC0A59F83BE6D2F4B85D4",
            &me
        ));
    }

    #[test]
    fn the_announcement_names_the_wallet_and_the_date() {
        let m = new_subscriber_message("0xabc", 1_795_111_382);
        assert!(m.starts_with("New Lodestar subscriber: 0xabc, paid until 19 November 2026."));
        assert!(m.ends_with("https://arbiscan.io/address/0xabc"));
    }

    #[test]
    fn everything_else_is_acknowledged_and_ignored() {
        assert_eq!(credit_for(&json!({"type": "customer.created"})), None);
        assert_eq!(credit_for(&json!({})), None);
    }
}
