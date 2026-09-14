#![cfg(feature = "ledger")]
//! The usage endpoint is what the ledger's subscription ceiling is checked against, so its shape
//! and its rounding matter more than most. Recorded from the live account on 2026-09-14.

use dune_client::DuneClient;
use dune_ledger::AccountUsageSource;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn client_for(server: &MockServer) -> DuneClient {
    DuneClient::with_base("fixture-secret".into(), &format!("{}/", server.uri())).unwrap()
}

/// The real response body, as recorded on 2026-09-14.
fn live_body() -> serde_json::Value {
    serde_json::json!({
        "private_queries": 0,
        "private_dashboards": 0,
        "bytes_used": 0,
        "bytes_allowed": 1000000000,
        "billing_periods": [{
            "start_date": "2026-09-13",
            "end_date": "2026-10-13",
            "credits_used": 7.113,
            "credits_included": 4000
        }]
    })
}

/// It is a POST. A GET on this path returns 405 with `allow: POST`, which cost real time to
/// discover, so it is pinned here.
#[tokio::test]
async fn usage_is_fetched_with_post() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(live_body()))
        .expect(1)
        .mount(&server)
        .await;
    let usage = client_for(&server).usage().await.expect("usage fetch");
    let period = usage.current().expect("a current period");
    assert_eq!(period.credits_used, 7.113);
    assert_eq!(period.credits_included, 4000.0);
    assert_eq!(period.start_date, "2026-09-13");
    server.verify().await;
}

/// Used rounds up and included rounds down, so neither rounding direction can invent headroom.
#[tokio::test]
async fn rounding_never_invents_headroom() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "billing_periods": [{
                "start_date": "2026-09-13",
                "end_date": "2026-10-13",
                // Deliberately awkward: neither figure is a whole microcredit.
                "credits_used": 7.1130004,
                "credits_included": 3999.9999994
            }]
        })))
        .mount(&server)
        .await;
    let usage = AccountUsageSource::account_usage(&client_for(&server))
        .await
        .expect("usage maps to the ledger type");
    assert_eq!(usage.used_microcredits, 7_113_001, "used rounds up");
    assert_eq!(
        usage.included_microcredits, 3_999_999_999,
        "included rounds down"
    );
}

/// A failed lookup is `None`, which the ledger turns into a refusal rather than a zero.
#[tokio::test]
async fn a_failed_lookup_is_none_not_zero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert!(
        AccountUsageSource::account_usage(&client_for(&server))
            .await
            .is_none(),
        "a server error must never read as an account with no spend"
    );
}

/// If Dune ever returns more than one period, the most-spent one is the safe choice for a ceiling.
#[tokio::test]
async fn the_most_spent_period_is_chosen() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "billing_periods": [
                {"start_date": "a", "end_date": "b", "credits_used": 1.0, "credits_included": 4000},
                {"start_date": "c", "end_date": "d", "credits_used": 9.0, "credits_included": 4000}
            ]
        })))
        .mount(&server)
        .await;
    let usage = AccountUsageSource::account_usage(&client_for(&server))
        .await
        .unwrap();
    assert_eq!(usage.used_microcredits, 9_000_000);
}
