use dune_client::{Page, Snapshot};
use dune_poller::{MIGRATION, materialize};
use sqlx::postgres::PgPoolOptions;

fn snapshot(id: &str, at: &str) -> Snapshot {
    let mut page: Page = serde_json::from_str(include_str!(
        "../../dune-client/tests/fixtures/pilot-page.json"
    ))
    .unwrap();
    page.execution_id = id.into();
    page.execution_ended_at = Some(at.into());
    page.result.metadata.total_row_count = 1;
    page.next_offset = None;
    Snapshot {
        rows: page.result.rows.clone(),
        first_page: page,
    }
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn publication_is_atomic_idempotent_and_monotonic() {
    let url = std::env::var("DUNE_TEST_DATABASE_URL").expect("isolated test database required");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!(
        "dune_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // Audited identifier: fixed prefix plus integer timestamp, with no external input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; SET search_path TO {schema}"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    let latest = snapshot("NEW", "2026-09-13T17:56:24Z");
    assert!(materialize(&pool, &latest).await.unwrap());
    assert!(!materialize(&pool, &latest).await.unwrap());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM v_grt_price_daily")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    let complete: bool = sqlx::query_scalar("SELECT complete_day FROM v_grt_price_daily")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !complete,
        "today's partial candle must be labelled incomplete"
    );
    assert!(
        materialize(&pool, &snapshot("OLD", "2026-09-13T17:55:00Z"))
            .await
            .unwrap()
    );
    let pointer: String = sqlx::query_scalar("SELECT execution_id FROM dune_publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pointer, "NEW");

    let mut bad = snapshot("BAD", "2026-09-13T18:00:00Z");
    bad.rows[0]["open"] = serde_json::json!(999.0);
    assert!(materialize(&pool, &bad).await.is_err());
    let mut empty = snapshot("EMPTY", "2026-09-13T18:00:00Z");
    empty.rows.clear();
    assert!(materialize(&pool, &empty).await.is_err());

    sqlx::raw_sql("ALTER TABLE dune_grt_price_daily ADD CONSTRAINT reject_next CHECK (row_index < 0) NOT VALID").execute(&pool).await.unwrap();
    assert!(
        materialize(&pool, &snapshot("FAIL", "2026-09-13T18:01:00Z"))
            .await
            .is_err()
    );
    let orphan: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dune_executions WHERE execution_id = 'FAIL'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        orphan, 0,
        "failed row insert must roll back the execution record"
    );
    let pointer: String = sqlx::query_scalar("SELECT execution_id FROM dune_publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pointer, "NEW");

    // Test last_publication & check_freshness
    let (pub_id, _) = dune_poller::last_publication(&pool, "grt_price_daily")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pub_id, "NEW");
    let freshness = dune_poller::check_freshness(&pool, "grt_price_daily", 86400 * 365)
        .await
        .unwrap();
    assert_eq!(freshness.last_execution_id.as_deref(), Some("NEW"));
    assert!(freshness.is_fresh);

    // Test prune_old_executions: keeping 1 should prune OLD
    let pruned = dune_poller::prune_old_executions(&pool, "grt_price_daily", 1)
        .await
        .unwrap();
    assert_eq!(pruned, 1);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM dune_executions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 1);

    // The same locally generated identifier; only this test's schema is removed.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET search_path TO public; DROP SCHEMA {schema} CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}
