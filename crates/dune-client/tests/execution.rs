#![cfg(feature = "executions")]
use dune_client::DuneClient;
use dune_ledger::Ledger;
use sqlx::postgres::PgPoolOptions;
use std::collections::BTreeMap;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL"]
async fn execution_requires_committed_matching_reservation_and_never_retries() {
    let url = std::env::var("DUNE_TEST_DATABASE_URL").expect("isolated test database required");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!(
        "dune_execution_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // Audited: generated identifier contains only a fixed prefix and integer timestamp.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; SET search_path TO {schema}"
    )))
    .execute(&pool)
    .await
    .unwrap();
    for migration in dune_ledger::MIGRATIONS {
        sqlx::raw_sql(*migration).execute(&pool).await.unwrap();
    }
    let period: i64 = sqlx::query_scalar("INSERT INTO dune_budget_periods(account, starts_at, ends_at, limit_microcredits) VALUES ('fixture', now() - interval '1 day', now() + interval '1 day', 1000000) RETURNING id").fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO dune_budget_allocations VALUES ($1, 'D', 1000000)")
        .bind(period)
        .execute(&pool)
        .await
        .unwrap();
    let ledger = Ledger::new(pool.clone());
    let server = MockServer::start().await;
    let client =
        DuneClient::with_base("fixture-secret".into(), &format!("{}/", server.uri())).unwrap();
    Mock::given(method("POST"))
        .and(path("/query/8699345/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"execution_id":"ACCEPTED1", "state":"QUERY_STATE_PENDING"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let permit = ledger
        .reserve(period, "D", 8699345, "fixture", 100000)
        .await
        .unwrap();
    let id = permit.id();
    let execution = client
        .execute(8699345, &BTreeMap::new(), permit)
        .await
        .unwrap();
    assert_eq!(execution.execution_id, "ACCEPTED1");
    let state: (String, String) =
        sqlx::query_as("SELECT state, execution_id FROM dune_reservations WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, ("submitted".into(), "ACCEPTED1".into()));
    let wrong = ledger
        .reserve(period, "D", 99, "fixture", 100000)
        .await
        .unwrap();
    assert!(
        client
            .execute(8699345, &BTreeMap::new(), wrong)
            .await
            .is_err()
    );
    server.verify().await;
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let permit = ledger
        .reserve(period, "D", 8699345, "fixture", 100000)
        .await
        .unwrap();
    let id = permit.id();
    assert!(
        client
            .execute(8699345, &BTreeMap::new(), permit)
            .await
            .is_err()
    );
    let state: String = sqlx::query_scalar("SELECT state FROM dune_reservations WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "submitted");
    server.verify().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET search_path TO public; DROP SCHEMA {schema} CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}
