//! Stripe's webhook: the one way a card payment becomes `paid_until`.
//!
//! Checkout is a Stripe Payment Link carrying the wallet as `client_reference_id`, so the first
//! event names the wallet and every renewal after it is found by the subscription id stored then.
//! The endpoint must be sent `checkout.session.completed`, `invoice.paid`,
//! `invoice.payment_failed`, `customer.subscription.deleted`, `charge.refunded` and
//! `charge.dispute.created`.

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
/// Margin past a period's end, or past Stripe's next retry of a failed renewal, before access stops.
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

/// What an event asks for. `address` is absent on renewals, which are found by subscription.
#[derive(Debug, PartialEq)]
pub(crate) struct Credit {
    pub address: Option<String>,
    pub external_ref: Option<String>,
    pub customer: Option<String>,
    pub paid_until: i64,
    pub email: Option<String>,
    /// When money landed; absent for a failed renewal that is only being carried to its retry.
    pub paid_at: Option<i64>,
    /// The subscription's first invoice, which routinely arrives before the checkout naming it.
    pub first_invoice: bool,
}

/// Money that went back, or a subscription that ended: access paid by it stops at `at`.
#[derive(Debug, PartialEq)]
pub(crate) struct Revoke {
    pub subscription: Option<String>,
    pub customer: Option<String>,
    /// For a refund, when the refunded charge was made, so only the period it paid for ends.
    pub paid_near: Option<i64>,
    pub at: i64,
    pub what: String,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    Credit(Credit),
    Revoke(Revoke),
    /// Something a person has to look at, because nothing here can act on it.
    Alert(String),
}

fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn usd(cents: &Value) -> String {
    format!("{:.2}", cents.as_f64().unwrap_or(0.0) / 100.0)
}

/// Read an event, or `None` for one that changes nothing.
pub(crate) fn action_for(event: &Value) -> Option<Action> {
    let obj = &event["data"]["object"];
    let s = |v: &Value| v.as_str().map(String::from);
    match event["type"].as_str()? {
        "checkout.session.completed" => {
            if obj["mode"] != "subscription" || obj["payment_status"] != "paid" {
                return None;
            }
            let Some(address) = obj["client_reference_id"]
                .as_str()
                .filter(|a| is_address(a))
            else {
                return Some(Action::Alert(format!(
                    "A paid Stripe checkout named no wallet, so nobody was credited.\n\
                     Session {}, subscription {}, customer {}, email {}.",
                    obj["id"].as_str().unwrap_or("?"),
                    obj["subscription"].as_str().unwrap_or("?"),
                    obj["customer"].as_str().unwrap_or("?"),
                    obj["customer_details"]["email"].as_str().unwrap_or("?"),
                )));
            };
            let created = event["created"].as_i64()?;
            Some(Action::Credit(Credit {
                address: Some(address.to_lowercase()),
                external_ref: s(&obj["subscription"]),
                customer: s(&obj["customer"]),
                paid_until: created + FIRST_PERIOD_SECS,
                email: s(&obj["customer_details"]["email"]),
                paid_at: Some(created),
                first_invoice: false,
            }))
        }
        kind @ ("invoice.paid" | "invoice.payment_failed") => {
            // Where the subscription id lives moved in Stripe's 2025 API versions; read both.
            let sub = obj["subscription"]
                .as_str()
                .or_else(|| obj["parent"]["subscription_details"]["subscription"].as_str())?;
            let (paid_until, paid_at) = if kind == "invoice.paid" {
                let end = obj["lines"]["data"]
                    .as_array()?
                    .iter()
                    .filter_map(|l| l["period"]["end"].as_i64())
                    .max()?;
                (end, event["created"].as_i64())
            } else {
                // Stripe is still retrying; keep the subscriber in until the next attempt.
                (obj["next_payment_attempt"].as_i64()?, None)
            };
            Some(Action::Credit(Credit {
                address: None,
                external_ref: Some(sub.to_string()),
                customer: s(&obj["customer"]),
                paid_until: paid_until + GRACE_SECS,
                email: s(&obj["customer_email"]),
                paid_at,
                first_invoice: obj["billing_reason"] == "subscription_create",
            }))
        }
        "customer.subscription.deleted" => Some(Action::Revoke(Revoke {
            subscription: Some(s(&obj["id"])?),
            customer: None,
            paid_near: None,
            at: event["created"].as_i64()?,
            what: format!("subscription {} ended", obj["id"].as_str()?),
        })),
        "charge.refunded" => {
            let what = format!(
                "refund of {} USD on charge {}",
                usd(&obj["amount_refunded"]),
                obj["id"].as_str().unwrap_or("?"),
            );
            // A partial refund is a goodwill credit, not money for the period going back.
            if obj["refunded"] != true {
                return Some(Action::Alert(format!(
                    "Stripe {what}, partial; access unchanged."
                )));
            }
            let Some(customer) = s(&obj["customer"]) else {
                return Some(Action::Alert(format!(
                    "Stripe {what}, for no customer; access unchanged."
                )));
            };
            Some(Action::Revoke(Revoke {
                subscription: None,
                customer: Some(customer),
                paid_near: Some(obj["created"].as_i64()?),
                at: event["created"].as_i64()?,
                what,
            }))
        }
        // A dispute names only its charge, which nothing here maps to a wallet.
        "charge.dispute.created" => Some(Action::Alert(format!(
            "Stripe dispute {} on charge {}: {} USD, reason {}. Access is unchanged until the \
             subscription is cancelled in Stripe.",
            obj["id"].as_str().unwrap_or("?"),
            obj["charge"].as_str().unwrap_or("?"),
            usd(&obj["amount"]),
            obj["reason"].as_str().unwrap_or("?"),
        ))),
        _ => None,
    }
}

fn announce(st: &AppState, text: String) {
    let Some(t) = st.telegram.clone() else { return };
    // Off the webhook's path: Stripe should not retry a payment because Telegram was slow.
    tokio::spawn(async move {
        if let Err(e) = crate::telegram::send(&t, &text).await {
            tracing::warn!(error = %e, "could not send a billing notice to Telegram");
        }
    });
}

fn until(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%-d %B %Y %H:%M UTC").to_string())
        .unwrap_or_default()
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
    let Some(action) = action_for(&event) else {
        return Ok(ok());
    };
    let credit = match action {
        Action::Credit(c) => c,
        Action::Alert(text) => {
            tracing::error!(event = ?event["id"], "{text}");
            announce(&st, text);
            return Ok(ok());
        }
        Action::Revoke(r) => {
            revoke(&st, &event, r).await?;
            return Ok(ok());
        }
    };
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;
    let db = kittiwake_db::unavailable::<kittiwake_db::DbError>;

    let address = match &credit.address {
        Some(a) => Some(a.clone()),
        None => kittiwake_db::stripe_subscription_address(
            pool,
            credit.external_ref.as_deref(),
            credit.customer.as_deref(),
        )
        .await
        .map_err(db)?,
    };
    let Some(address) = address else {
        // Acknowledged rather than retried: a checkout that names the wallet carries its own month.
        if credit.first_invoice {
            tracing::info!(event = ?event["id"], "first invoice ahead of its checkout");
        } else {
            let text = format!(
                "A Stripe renewal for subscription {} (customer {}) matched no wallet, so nobody \
                 was credited.",
                credit.external_ref.as_deref().unwrap_or("?"),
                credit.customer.as_deref().unwrap_or("?"),
            );
            tracing::error!(event = ?event["id"], "{text}");
            announce(&st, text);
        }
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
    if let Some(sub) = &credit.external_ref {
        kittiwake_db::record_stripe_subscription(
            pool,
            sub,
            &address,
            credit.customer.as_deref(),
            credit.paid_until,
            credit.paid_at,
        )
        .await
        .map_err(db)?;
    }
    if let Some(p) = &st.paywall {
        p.forget(&address);
    }
    if let Some(t) = &st.telegram
        && is_news(inserted, &address, &t.ignore_addresses)
    {
        announce(&st, new_subscriber_message(&address, credit.paid_until));
    }
    metrics::counter!("stripe_credits_total").increment(1);
    Ok(ok())
}

async fn revoke(st: &AppState, event: &Value, r: Revoke) -> Result<(), ErrorResponse> {
    let pool = st
        .pool
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("no database is configured".into()))?;
    let ended = kittiwake_db::end_stripe_access(
        pool,
        r.subscription.as_deref(),
        r.customer.as_deref(),
        r.paid_near,
        r.at,
    )
    .await
    .map_err(|e| ApiError::Unavailable(format!("database: {e}")))?;
    for (address, _) in &ended {
        if let Some(p) = &st.paywall {
            p.forget(address);
        }
    }
    let text = if ended.is_empty() {
        format!("Stripe {}: no wallet's access changed.", r.what)
    } else {
        let lines: Vec<String> = ended
            .iter()
            .map(|(a, t)| format!("{a} now paid until {}", until(*t)))
            .collect();
        format!("Stripe {}:\n{}", r.what, lines.join("\n"))
    };
    tracing::warn!(event = ?event["id"], "{text}");
    announce(st, text);
    metrics::counter!("stripe_revocations_total").increment(1);
    Ok(())
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

    /// The event as the webhook would receive it: signed, checked, and read back.
    fn received(event: Value) -> Option<Action> {
        let body = event.to_string();
        assert!(verify_signature(
            SECRET,
            &sign(NOW, &body),
            body.as_bytes(),
            NOW
        ));
        action_for(&serde_json::from_str(&body).unwrap())
    }

    fn credit(event: Value) -> Credit {
        match received(event) {
            Some(Action::Credit(c)) => c,
            other => panic!("not a credit: {other:?}"),
        }
    }

    const WALLET: &str = "0xabcdef0123456789abcdef0123456789abcdef01";

    #[test]
    fn a_completed_checkout_credits_the_wallet_it_names() {
        let e = json!({
            "type": "checkout.session.completed", "created": NOW,
            "data": {"object": {
                "mode": "subscription", "payment_status": "paid",
                "client_reference_id": "0xAbCdEf0123456789abcdef0123456789ABCDEF01",
                "subscription": "sub_1", "customer": "cus_1", "customer_details": {"email": "a@b.c"}
            }}
        });
        assert_eq!(
            credit(e),
            Credit {
                address: Some(WALLET.into()),
                external_ref: Some("sub_1".into()),
                customer: Some("cus_1".into()),
                paid_until: NOW + FIRST_PERIOD_SECS,
                email: Some("a@b.c".into()),
                paid_at: Some(NOW),
                first_invoice: false,
            }
        );
    }

    /// kittiwake#211: this used to be acknowledged in silence.
    #[test]
    fn a_paid_checkout_without_a_wallet_is_raised_rather_than_dropped() {
        let base = |id: Value, status: &str| {
            json!({"type": "checkout.session.completed", "created": NOW, "data": {"object": {
                "id": "cs_1", "mode": "subscription", "payment_status": status,
                "client_reference_id": id, "subscription": "sub_9",
                "customer_details": {"email": "a@b.c"}
            }}})
        };
        for id in [Value::Null, json!("not-a-wallet")] {
            let Some(Action::Alert(text)) = received(base(id, "paid")) else {
                panic!("no alert")
            };
            assert!(text.contains("named no wallet"), "{text}");
            assert!(text.contains("cs_1") && text.contains("sub_9") && text.contains("a@b.c"));
        }
        assert_eq!(received(base(json!(WALLET), "unpaid")), None);
    }

    #[test]
    fn a_renewal_runs_to_the_end_of_its_period_in_either_api_shape() {
        let old = json!({"type": "invoice.paid", "created": NOW, "data": {"object": {
            "subscription": "sub_1", "customer": "cus_1", "billing_reason": "subscription_cycle",
            "lines": {"data": [{"period": {"end": NOW + 100}}, {"period": {"end": NOW + 200}}]}
        }}});
        let new = json!({"type": "invoice.paid", "created": NOW, "data": {"object": {
            "parent": {"subscription_details": {"subscription": "sub_1"}}, "customer": "cus_1",
            "billing_reason": "subscription_cycle",
            "lines": {"data": [{"period": {"end": NOW + 200}}]}
        }}});
        for e in [old, new] {
            let c = credit(e);
            assert_eq!(c.address, None);
            assert_eq!(c.external_ref.as_deref(), Some("sub_1"));
            assert_eq!(c.customer.as_deref(), Some("cus_1"));
            assert_eq!(c.paid_until, NOW + 200 + GRACE_SECS);
            assert_eq!(c.paid_at, Some(NOW));
            assert!(!c.first_invoice);
        }
    }

    /// Lets the webhook tell an ordinary sign-up's race from a renewal nobody can be credited for.
    #[test]
    fn the_first_invoice_is_told_apart_from_a_renewal() {
        let e = json!({"type": "invoice.paid", "created": NOW, "data": {"object": {
            "subscription": "sub_1", "billing_reason": "subscription_create",
            "lines": {"data": [{"period": {"end": NOW + 200}}]}
        }}});
        assert!(credit(e).first_invoice);
    }

    /// lodestar#349: a failed renewal kept access for one day while Stripe retried for weeks.
    #[test]
    fn a_failed_renewal_keeps_access_until_stripe_next_retries() {
        let failed = |next: Value| {
            json!({"type": "invoice.payment_failed", "created": NOW, "data": {"object": {
                "subscription": "sub_1", "customer": "cus_1", "billing_reason": "subscription_cycle",
                "next_payment_attempt": next,
                "lines": {"data": [{"period": {"end": NOW + 30 * 86_400}}]}
            }}})
        };
        let c = credit(failed(json!(NOW + 5 * 86_400)));
        assert_eq!(c.paid_until, NOW + 5 * 86_400 + GRACE_SECS);
        assert_eq!(c.paid_at, None, "a retry is not a payment");
        assert_eq!(
            received(failed(Value::Null)),
            None,
            "no retry left extends nothing"
        );
    }

    /// kittiwake#214.
    #[test]
    fn an_ended_subscription_ends_its_access() {
        let e = json!({"type": "customer.subscription.deleted", "created": NOW, "data": {"object": {
            "id": "sub_1", "customer": "cus_1"
        }}});
        assert_eq!(
            received(e),
            Some(Action::Revoke(Revoke {
                subscription: Some("sub_1".into()),
                customer: None,
                paid_near: None,
                at: NOW,
                what: "subscription sub_1 ended".into(),
            }))
        );
    }

    #[test]
    fn a_full_refund_ends_the_period_its_charge_paid_for() {
        let refund = |refunded: bool| {
            json!({"type": "charge.refunded", "created": NOW, "data": {"object": {
                "id": "ch_1", "customer": "cus_1", "created": NOW - 3_600,
                "amount": 999, "amount_refunded": if refunded { 999 } else { 300 },
                "refunded": refunded
            }}})
        };
        assert_eq!(
            received(refund(true)),
            Some(Action::Revoke(Revoke {
                subscription: None,
                customer: Some("cus_1".into()),
                paid_near: Some(NOW - 3_600),
                at: NOW,
                what: "refund of 9.99 USD on charge ch_1".into(),
            }))
        );
        let Some(Action::Alert(text)) = received(refund(false)) else {
            panic!("a partial refund is still worth knowing about")
        };
        assert!(
            text.contains("3.00 USD") && text.contains("partial"),
            "{text}"
        );
    }

    #[test]
    fn a_dispute_is_raised() {
        let e = json!({"type": "charge.dispute.created", "created": NOW, "data": {"object": {
            "id": "dp_1", "charge": "ch_1", "amount": 999, "reason": "fraudulent"
        }}});
        let Some(Action::Alert(text)) = received(e) else {
            panic!("no alert")
        };
        assert!(text.contains("dp_1") && text.contains("ch_1") && text.contains("9.99 USD"));
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
        assert_eq!(received(json!({"type": "customer.created"})), None);
        assert_eq!(action_for(&json!({})), None);
    }
}
