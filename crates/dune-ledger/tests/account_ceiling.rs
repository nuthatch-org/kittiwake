//! The subscription ceiling must hold against spend the ledger never admitted.
//!
//! The account is on a fixed monthly plan with no appetite for overage, so the binding question is
//! not "has this ledger authorised too much" but "has the account been billed too much, by anyone".
//! An execution started from the Dune web UI, a Dune-side schedule, or another holder of the same
//! key spends from the same allowance and leaves no row in `dune_reservations`.

use dune_ledger::{AccountUsage, AccountUsageSource, Ledger, MIGRATIONS};
use sqlx::postgres::PgPoolOptions;

/// A stand-in for Dune's usage endpoint, so the ceiling can be tested without spending anything.
struct FakeUsage(std::sync::Mutex<Option<AccountUsage>>);

impl FakeUsage {
    fn reporting(used: i64, included: i64) -> Self {
        Self(std::sync::Mutex::new(Some(AccountUsage {
            used_microcredits: used,
            included_microcredits: included,
        })))
    }
    fn failing() -> Self {
        Self(std::sync::Mutex::new(None))
    }
    fn set(&self, used: i64, included: i64) {
        *self.0.lock().unwrap() = Some(AccountUsage {
            used_microcredits: used,
            included_microcredits: included,
        });
    }
}

impl AccountUsageSource for FakeUsage {
    async fn account_usage(&self) -> Option<AccountUsage> {
        *self.0.lock().unwrap()
    }
}

async fn fixture(schema_tag: &str) -> (sqlx::PgPool, i64) {
    let url = std::env::var("DUNE_TEST_DATABASE_URL").expect("isolated test database required");
    let schema = format!(
        "dune_ceiling_{}_{}",
        schema_tag,
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
    // Identifier is a fixed prefix plus a locally generated integer timestamp.
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
    for migration in MIGRATIONS {
        sqlx::raw_sql(*migration).execute(&pool).await.unwrap();
    }
    // Ceiling of 1,000,000 microcredits, i.e. one credit, to keep the arithmetic readable.
    let period: i64 = sqlx::query_scalar(
        "INSERT INTO dune_budget_periods(account, starts_at, ends_at, limit_microcredits) \
         VALUES ('fixture', now() - interval '1 day', now() + interval '1 day', 1000000) \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO dune_budget_allocations VALUES ($1, 'A', 1000000)")
        .bind(period)
        .execute(&pool)
        .await
        .unwrap();
    (pool, period)
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn spend_outside_the_ledger_still_counts_against_the_ceiling() {
    let (pool, period) = fixture("outside").await;
    let ledger = Ledger::new(pool.clone());

    // Nothing has passed through this ledger, so its own sum is zero and the old admission rule
    // would have allowed the full allowance.
    assert_eq!(ledger.remaining_headroom(period).await.unwrap(), 1_000_000);

    // Dune reports the account has already spent 900,000 elsewhere.
    let usage = FakeUsage::reporting(900_000, 1_000_000);
    let stored = ledger.sync_account_usage(period, &usage).await.unwrap();
    assert_eq!(stored, 900_000);
    assert_eq!(ledger.remaining_headroom(period).await.unwrap(), 100_000);

    // A reservation that fits the ledger's own (empty) books but not the account is refused.
    let refused = ledger
        .reserve(period, "A", 8699345, "fixture", 200_000)
        .await;
    assert!(
        refused.is_err(),
        "spend invisible to the ledger must still bind the ceiling"
    );

    // One that fits the real remaining headroom is admitted.
    let permitted = ledger
        .reserve(period, "A", 8699345, "fixture", 90_000)
        .await;
    assert!(permitted.is_ok(), "headroom below the ceiling is usable");
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn in_flight_reservations_are_added_to_dune_reported_spend() {
    let (pool, period) = fixture("inflight").await;
    let ledger = Ledger::new(pool.clone());
    let usage = FakeUsage::reporting(500_000, 1_000_000);
    ledger.sync_account_usage(period, &usage).await.unwrap();

    // 500,000 already billed leaves 500,000. Take 400,000 of it, not yet billed by Dune.
    let _held = ledger
        .reserve(period, "A", 8699345, "fixture", 400_000)
        .await
        .expect("fits");
    assert_eq!(ledger.remaining_headroom(period).await.unwrap(), 100_000);

    // A second reservation must see the in-flight one on top of Dune's figure, not just Dune's.
    let refused = ledger
        .reserve(period, "A", 8699345, "fixture", 200_000)
        .await;
    assert!(
        refused.is_err(),
        "an unbilled reservation must not be double-spent against the same headroom"
    );
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn a_usage_reading_never_falls_within_a_period() {
    let (pool, period) = fixture("monotonic").await;
    let ledger = Ledger::new(pool.clone());
    let usage = FakeUsage::reporting(800_000, 1_000_000);
    assert_eq!(
        ledger.sync_account_usage(period, &usage).await.unwrap(),
        800_000
    );

    // A lower reading means a stale or partial response. Dune's counter does not refund.
    usage.set(10_000, 1_000_000);
    assert_eq!(
        ledger.sync_account_usage(period, &usage).await.unwrap(),
        800_000,
        "a lower reading must not raise headroom"
    );

    usage.set(950_000, 1_000_000);
    assert_eq!(
        ledger.sync_account_usage(period, &usage).await.unwrap(),
        950_000,
        "a higher reading is adopted"
    );
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn a_failed_usage_lookup_leaves_the_stored_figure_untouched() {
    let (pool, period) = fixture("failed").await;
    let ledger = Ledger::new(pool.clone());
    ledger
        .sync_account_usage(period, &FakeUsage::reporting(700_000, 1_000_000))
        .await
        .unwrap();
    let err = ledger
        .sync_account_usage(period, &FakeUsage::failing())
        .await;
    assert!(err.is_err(), "a failed lookup is an error, not a zero");
    assert_eq!(
        ledger.remaining_headroom(period).await.unwrap(),
        300_000,
        "a failed lookup must not appear to free up headroom"
    );
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn admission_refuses_when_the_usage_observation_is_missing_or_stale() {
    let (pool, period) = fixture("stale").await;
    let strict = Ledger::new(pool.clone()).requiring_fresh_observation(
        // Anything observed longer ago than this is not good enough to spend against.
        std::time::Duration::from_secs(300),
    );

    // Never observed at all: refused, even though the books are empty and the ceiling is miles off.
    assert!(
        strict
            .reserve(period, "A", 8699345, "fixture", 1_000)
            .await
            .is_err(),
        "an unverified account balance must not be spent against"
    );

    // Observed just now: admitted.
    strict
        .sync_account_usage(period, &FakeUsage::reporting(0, 1_000_000))
        .await
        .unwrap();
    assert!(
        strict
            .reserve(period, "A", 8699345, "fixture", 1_000)
            .await
            .is_ok(),
        "a fresh observation permits admission"
    );

    // Backdate the observation past the limit: refused again.
    sqlx::query(
        "UPDATE dune_budget_periods SET observed_at = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(period)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        strict
            .reserve(period, "A", 8699345, "fixture", 1_000)
            .await
            .is_err(),
        "a stale observation must refuse admission"
    );

    // A ledger without the requirement is unaffected, which is what keeps the check opt-in.
    let relaxed = Ledger::new(pool.clone());
    assert!(
        relaxed
            .reserve(period, "A", 8699345, "fixture", 1_000)
            .await
            .is_ok()
    );
}
