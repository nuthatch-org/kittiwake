use dune_client::{DuneClient, Error, Status};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/pilot-page.json")).unwrap()
}
fn client(server: &MockServer) -> DuneClient {
    DuneClient::with_base("fixture-key".into(), &format!("{}/", server.uri())).unwrap()
}

#[test]
fn recorded_cost_is_present_on_status() {
    let status: Status = serde_json::from_str(include_str!("fixtures/pilot-status.json")).unwrap();
    assert_eq!(status.execution_cost_credits, Some(0.152970589));
    assert_eq!(status.result_metadata.unwrap().total_row_count, 31);
}

#[tokio::test]
async fn snapshot_pins_execution_and_does_not_follow_next_uri() {
    let server = MockServer::start().await;
    let mut first = fixture();
    first["result"]["metadata"]["total_row_count"] = json!(2);
    first["next_uri"] = json!("https://untrusted.invalid/steal-key");
    let mut second = first.clone();
    second["next_offset"] = Value::Null;
    second["result"]["rows"][0]["day"] = json!("2026-09-12 00:00:00.000 UTC");
    Mock::given(method("GET"))
        .and(path("/query/8699345/results"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/execution/01M2DYKBS3WHDN1ZW6W393XES4/results"))
        .and(query_param("offset", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(second))
        .expect(1)
        .mount(&server)
        .await;
    let snapshot = client(&server).snapshot(8699345, 1, 2).await.unwrap();
    assert_eq!(snapshot.rows.len(), 2);
    server.verify().await;
}

#[tokio::test]
async fn truncated_result_never_becomes_complete_snapshot() {
    let server = MockServer::start().await;
    let mut page = fixture();
    page["next_offset"] = Value::Null;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server).snapshot(8699345, 1, 31).await,
        Err(Error::Response("incomplete snapshot"))
    ));
}

#[tokio::test]
async fn repeated_offset_is_rejected_without_looping() {
    let server = MockServer::start().await;
    let mut page = fixture();
    page["next_offset"] = json!(0);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server).snapshot(8699345, 1, 31).await,
        Err(Error::Response("pagination offset"))
    ));
    server.verify().await;
}

#[tokio::test]
async fn over_budget_result_stops_after_first_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture()))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server).snapshot(8699345, 1, 10).await,
        Err(Error::Response("row budget exceeded"))
    ));
    server.verify().await;
}

#[tokio::test]
async fn credit_error_is_not_retried_or_echoed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(402).set_body_string("fixture-key"))
        .expect(1)
        .mount(&server)
        .await;
    let error = client(&server)
        .latest_results(8699345, 1)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Dune returned HTTP 402");
    server.verify().await;
}

#[tokio::test]
async fn transient_failure_has_bounded_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "0"))
        // Four attempts, not three: Dune's rate limiter sends no Retry-After and a 429 measured
        // on 2026-09-14 took up to 15s to clear, so the budget was widened. Bounded either way.
        .expect(4)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server).latest_results(8699345, 1).await,
        Err(Error::Http(503))
    ));
    server.verify().await;
}

/// A rate limit is retried like a transient failure, and still gives up rather than hammering.
/// `Retry-After: 0` keeps the test quick; the real endpoint sends no such header, which is why
/// the client carries its own seconds-scale schedule for 429 specifically.
#[tokio::test]
async fn rate_limit_is_retried_and_still_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .expect(4)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server).latest_results(8699345, 1).await,
        Err(Error::Http(429))
    ));
    server.verify().await;
}

/// A rate limit that clears partway through is not surfaced to the caller at all.
#[tokio::test]
async fn rate_limit_that_clears_is_invisible_to_the_caller() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture()))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .latest_results(8699345, 1)
        .await
        .expect("a cleared rate limit should not reach the caller");
    assert_eq!(page.query_id, 8699345);
    server.verify().await;
}

#[tokio::test]
async fn upload_csv_posts_expected_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/uploads/csv"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "table_name": "burrmill_fx_overflow",
            "bytes_written": 128
        })))
        .expect(1)
        .mount(&server)
        .await;
    let res = client(&server)
        .upload_csv(
            "burrmill_fx_overflow",
            "test description",
            "a,b\n1,2",
            false,
        )
        .await
        .unwrap();
    assert_eq!(res.table_name.as_deref(), Some("burrmill_fx_overflow"));
    assert_eq!(res.bytes_written, Some(128));
    server.verify().await;
}
