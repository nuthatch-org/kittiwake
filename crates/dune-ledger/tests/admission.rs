use dune_ledger::{Ledger, MIGRATION};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn concurrent_reservations_share_account_budget_and_uncertainty_stays_charged() {
    let url = std::env::var("DUNE_TEST_DATABASE_URL").expect("isolated test database required");
    let schema = format!(
        "dune_ledger_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    // Identifier is a fixed prefix plus locally generated integer timestamp.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
    let period: i64 = sqlx::query_scalar("INSERT INTO dune_budget_periods(account, starts_at, ends_at, limit_microcredits) VALUES ('fixture', now() - interval '1 day', now() + interval '1 day', 1000000) RETURNING id").fetch_one(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO dune_budget_allocations VALUES ($1, 'A', 1000000), ($1, 'B', 1000000)",
    )
    .bind(period)
    .execute(&pool)
    .await
    .unwrap();
    let ledger = Ledger::new(pool.clone());
    let (a, b) = tokio::join!(
        ledger.reserve(period, "A", 8699345, "fixture", 700000),
        ledger.reserve(period, "B", 8699345, "fixture", 700000)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let permit = match (a, b) {
        (Ok(p), Err(_)) | (Err(_), Ok(p)) => p,
        _ => unreachable!(),
    };
    let id = permit.id();
    let submitted = permit.claim(8699345).await.unwrap();
    // Until an execution is known, the full reserved amount remains unavailable.
    assert!(
        ledger
            .reserve(period, "A", 8699345, "fixture", 300001)
            .await
            .is_err()
    );
    submitted.attach_execution("FIXTURE1").await.unwrap();
    ledger.settle("FIXTURE1", 100000).await.unwrap();
    ledger.settle("FIXTURE1", 100000).await.unwrap();
    assert!(ledger.settle("FIXTURE1", 99999).await.is_err());
    assert!(ledger.settle("UNKNOWN", 1).await.is_err());
    assert!(
        ledger
            .reserve(period, "A", 8699345, "fixture", 900000)
            .await
            .is_ok()
    );
    assert!(
        ledger
            .reserve(period, "B", 8699345, "fixture", 1)
            .await
            .is_err()
    );
    let state: String = sqlx::query_scalar("SELECT state FROM dune_reservations WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "settled");

    // Test record_observed and current_usage
    let observed_ok = ledger
        .record_observed(
            "fixture",
            "A",
            8699345,
            "fixture_poller",
            Some("OBSERVED1"),
            50000,
        )
        .await
        .unwrap();
    assert!(observed_ok);
    // Duplicate observed execution is a no-op
    let duplicate = ledger
        .record_observed(
            "fixture",
            "A",
            8699345,
            "fixture_poller",
            Some("OBSERVED1"),
            50000,
        )
        .await
        .unwrap();
    assert!(!duplicate);

    let usage = ledger.current_usage("fixture").await.unwrap().unwrap();
    assert_eq!(usage.period_id, period);
    assert_eq!(usage.total_limit_microcredits, 1000000);
    assert_eq!(usage.total_charged_microcredits, 1050000);
    assert_eq!(usage.workstream_usage.get("A"), Some(&1050000));
    assert_eq!(usage.unresolved_count, 1);
    assert_eq!(usage.percent_used(), 105.0);
    assert!(usage.is_threshold_exceeded(60.0));
    assert!(usage.is_threshold_exceeded(95.0));

    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
