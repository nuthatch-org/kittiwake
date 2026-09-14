#![cfg(feature = "ledger")]
use dune_client::DuneClient;
use dune_ledger::{ExecutionCost, ExecutionCostSource};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn client_for(server: &MockServer) -> DuneClient {
    DuneClient::with_base("fixture-secret".into(), &format!("{}/", server.uri())).unwrap()
}

#[tokio::test]
async fn a_running_execution_reads_as_pending() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/execution/EXEC1/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "execution_id": "EXEC1",
            "query_id": 8699345,
            "state": "QUERY_STATE_EXECUTING",
            "is_execution_finished": false,
        })))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    assert_eq!(client.execution_cost("EXEC1").await, ExecutionCost::Pending);
}

#[tokio::test]
async fn a_finished_execution_with_a_reported_cost_settles_rounded_up_to_microcredits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/execution/EXEC2/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "execution_id": "EXEC2",
            "query_id": 8699345,
            "state": "QUERY_STATE_COMPLETED",
            "is_execution_finished": true,
            "execution_cost_credits": 0.152970589,
        })))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    assert_eq!(
        client.execution_cost("EXEC2").await,
        ExecutionCost::Settled {
            microcredits: 152971
        }
    );
}

#[tokio::test]
async fn a_finished_execution_with_no_reported_cost_is_left_unknown_not_zeroed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/execution/EXEC3/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "execution_id": "EXEC3",
            "query_id": 8699345,
            "state": "QUERY_STATE_FAILED",
            "is_execution_finished": true,
        })))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    assert_eq!(client.execution_cost("EXEC3").await, ExecutionCost::Unknown);
}

#[tokio::test]
async fn a_lookup_that_fails_outright_is_unknown_rather_than_a_panic() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/execution/EXEC4/status"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = client_for(&server).await;
    assert_eq!(client.execution_cost("EXEC4").await, ExecutionCost::Unknown);
}
