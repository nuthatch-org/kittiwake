use dune_ledger::{ExecutionCost, ExecutionCostSource, Ledger, MIGRATION};
use sqlx::postgres::PgPoolOptions;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Reports whatever the test wired in for each execution ID, defaulting to `Unknown` so an
/// unrecognised ID behaves like a lookup that found nothing rather than panicking.
struct FakeSource(Mutex<BTreeMap<String, ExecutionCost>>);

impl ExecutionCostSource for FakeSource {
    async fn execution_cost(&self, execution_id: &str) -> ExecutionCost {
        self.0
            .lock()
            .unwrap()
            .get(execution_id)
            .copied()
            .unwrap_or(ExecutionCost::Unknown)
    }
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn reconcile_settles_known_costs_and_leaves_the_rest_for_a_later_pass() {
    let url = std::env::var("DUNE_TEST_DATABASE_URL").expect("isolated test database required");
    let schema = format!(
        "dune_reconcile_test_{}",
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
    // Audited: generated identifier contains only a fixed prefix and integer timestamp.
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
    sqlx::query("INSERT INTO dune_budget_allocations VALUES ($1, 'D', 1000000)")
        .bind(period)
        .execute(&pool)
        .await
        .unwrap();
    let ledger = Ledger::new(pool.clone());

    let attach = async |exec_id: &'static str| {
        let permit = ledger
            .reserve(period, "D", 8699345, "fixture", 100000)
            .await
            .unwrap();
        let submitted = permit.claim(8699345).await.unwrap();
        submitted.attach_execution(exec_id).await.unwrap();
    };
    attach("EXECA").await;
    attach("EXECB").await;
    attach("EXECC").await;

    let source = FakeSource(Mutex::new(BTreeMap::from([
        (
            "EXECA".to_string(),
            ExecutionCost::Settled {
                microcredits: 250000,
            },
        ),
        ("EXECB".to_string(), ExecutionCost::Pending),
        // EXECC deliberately absent: an unrecognised ID reads as Unknown, same as a lookup
        // failure would.
    ])));

    let report = ledger.reconcile(&source, 10).await.unwrap();
    assert_eq!(report.settled, 1);
    assert_eq!(report.still_pending, 1);
    assert_eq!(report.unresolved, 1);

    let state = |exec_id: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<i64>)>(
                "SELECT state, actual_microcredits FROM dune_reservations WHERE execution_id = $1",
            )
            .bind(exec_id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(state("EXECA").await, ("settled".into(), Some(250000)));
    assert_eq!(state("EXECB").await, ("submitted".into(), None));
    assert_eq!(state("EXECC").await, ("submitted".into(), None));

    // A later pass only ever touches what's still submitted; EXECA is no longer offered to the
    // source at all, and a source that now knows EXECB's cost settles it too.
    let later = FakeSource(Mutex::new(BTreeMap::from([(
        "EXECB".to_string(),
        ExecutionCost::Settled {
            microcredits: 400000,
        },
    )])));
    let report = ledger.reconcile(&later, 10).await.unwrap();
    assert_eq!(report.settled, 1);
    assert_eq!(report.still_pending, 0);
    assert_eq!(report.unresolved, 1);
    assert_eq!(state("EXECB").await, ("settled".into(), Some(400000)));
    assert_eq!(state("EXECC").await, ("submitted".into(), None));

    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
