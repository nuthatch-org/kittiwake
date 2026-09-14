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
            period_start: "2026-09-13".into(),
            period_end: "2026-10-13".into(),
        })))
    }
    fn failing() -> Self {
        Self(std::sync::Mutex::new(None))
    }
    fn set(&self, used: i64, included: i64) {
        let mut guard = self.0.lock().unwrap();
        let (start, end) = guard
            .as_ref()
            .map(|u| (u.period_start.clone(), u.period_end.clone()))
            .unwrap_or_else(|| ("2026-09-13".into(), "2026-10-13".into()));
        *guard = Some(AccountUsage {
            used_microcredits: used,
            included_microcredits: included,
            period_start: start,
            period_end: end,
        });
    }
    /// Move the reported billing period on, as Dune does at a renewal.
    fn roll_period(&self, start: &str, end: &str, used: i64, included: i64) {
        *self.0.lock().unwrap() = Some(AccountUsage {
            used_microcredits: used,
            included_microcredits: included,
            period_start: start.into(),
            period_end: end.into(),
        });
    }
}

impl AccountUsageSource for FakeUsage {
    async fn account_usage(&self) -> Option<AccountUsage> {
        self.0.lock().unwrap().clone()
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

/// A database with no period row at all is where every deployment starts. `ensure_period` is the
/// only thing that gets it out of that state, so it is tested from empty.
async fn empty_fixture(schema_tag: &str) -> sqlx::PgPool {
    let (pool, period) = fixture(schema_tag).await;
    // Drop the seeded period so the test starts from genuinely nothing.
    sqlx::query("DELETE FROM dune_budget_allocations WHERE period_id = $1")
        .bind(period)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM dune_budget_periods WHERE id = $1")
        .bind(period)
        .execute(&pool)
        .await
        .unwrap();
    pool
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn ensure_period_seeds_from_dunes_own_billing_dates() {
    let pool = empty_fixture("seed").await;
    let ledger = Ledger::new(pool.clone());
    let usage = FakeUsage::reporting(7_113_000, 4_000_000_000);

    // Nothing exists yet, so admission has nothing to bind to.
    assert!(
        ledger.sync_active_period("fixture", &usage).await.is_err(),
        "no period means nothing to sync"
    );

    let period = ledger
        .ensure_period("fixture", &usage, 3_600_000_000)
        .await
        .expect("seeds");

    let (starts, ends, limit, observed): (String, String, i64, i64) = sqlx::query_as(
        "SELECT starts_at::date::text, ends_at::date::text, limit_microcredits, \
         observed_used_microcredits FROM dune_budget_periods WHERE id = $1",
    )
    .bind(period)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Dune's dates, not calendar months.
    assert_eq!(starts, "2026-09-13");
    assert_eq!(ends, "2026-10-13");
    assert_eq!(
        limit, 3_600_000_000,
        "the configured ceiling, below included"
    );
    assert_eq!(observed, 7_113_000, "real spend carried in at seed time");

    // Idempotent: a second call returns the same period rather than a duplicate.
    let again = ledger
        .ensure_period("fixture", &usage, 3_600_000_000)
        .await
        .unwrap();
    assert_eq!(again, period);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dune_budget_periods")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// A plan downgrade must tighten the ceiling, never leave the ledger authorising spend the plan no
/// longer includes.
#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn the_ceiling_is_capped_by_dunes_included_allowance() {
    let pool = empty_fixture("downgrade").await;
    let ledger = Ledger::new(pool.clone());

    // Dune includes only 1,000 credits, well below the configured 3,600.
    let usage = FakeUsage::reporting(0, 1_000_000_000);
    let period = ledger
        .ensure_period("fixture", &usage, 3_600_000_000)
        .await
        .unwrap();
    let limit: i64 =
        sqlx::query_scalar("SELECT limit_microcredits FROM dune_budget_periods WHERE id = $1")
            .bind(period)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(limit, 1_000_000_000, "the plan caps the configured ceiling");

    // And a later, larger reading must not raise it back up.
    usage.set(0, 4_000_000_000);
    ledger
        .ensure_period("fixture", &usage, 3_600_000_000)
        .await
        .unwrap();
    let limit: i64 =
        sqlx::query_scalar("SELECT limit_microcredits FROM dune_budget_periods WHERE id = $1")
            .bind(period)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(limit, 1_000_000_000, "a ceiling only ever tightens");
}

/// When Dune renews, a new period row must appear rather than the old one absorbing new spend.
#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn a_renewal_starts_a_new_period_rather_than_extending_the_old_one() {
    let pool = empty_fixture("rollover").await;
    let ledger = Ledger::new(pool.clone());
    let usage = FakeUsage::reporting(3_500_000_000, 4_000_000_000);
    let first = ledger
        .ensure_period("fixture", &usage, 3_600_000_000)
        .await
        .unwrap();

    usage.roll_period("2026-10-13", "2026-11-13", 2_000_000, 4_000_000_000);
    let second = ledger
        .ensure_period("fixture", &usage, 3_600_000_000)
        .await
        .unwrap();

    assert_ne!(first, second, "a renewal is a new period");
    let observed: i64 = sqlx::query_scalar(
        "SELECT observed_used_microcredits FROM dune_budget_periods WHERE id = $1",
    )
    .bind(second)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        observed, 2_000_000,
        "the new period starts from the new period's spend, not the old period's"
    );
}

// ---------------------------------------------------------------------------
// Spend alarms. The Dune-side extra-credit limit is not set, so these log lines
// are currently the only warning before an automatic overage charge. An alarm
// that does not fire is worse than no alarm, so each threshold is pinned.
// ---------------------------------------------------------------------------

/// Builds a period with explicit dates so the elapsed fraction, and therefore the projection,
/// is deterministic rather than dependent on when the suite runs.
async fn dated_fixture(
    tag: &str,
    elapsed_days: i64,
    total_days: i64,
    ceiling: i64,
) -> (sqlx::PgPool, i64) {
    let pool = empty_fixture(tag).await;
    let period: i64 = sqlx::query_scalar(
        "INSERT INTO dune_budget_periods(account, starts_at, ends_at, limit_microcredits) \
         VALUES ('fixture', now() - make_interval(days => $1), \
                             now() + make_interval(days => $2), $3) \
         RETURNING id",
    )
    .bind(elapsed_days as i32)
    .bind((total_days - elapsed_days) as i32)
    .bind(ceiling)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO dune_budget_allocations VALUES ($1, 'A', $2)")
        .bind(period)
        .bind(ceiling)
        .execute(&pool)
        .await
        .unwrap();
    (pool, period)
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn spend_outside_the_ledger_shows_up_as_unaccounted() {
    // Half a 30-day period gone, 3,600-credit ceiling, 4,000 included.
    let (pool, period) = dated_fixture("unaccounted", 15, 30, 3_600_000_000).await;
    let ledger = Ledger::new(pool.clone());

    // Dune has billed 100 credits. Nothing went through the ledger, which is exactly the shape of
    // a Dune-side schedule or somebody using the web UI.
    let usage = FakeUsage::reporting(100_000_000, 4_000_000_000);
    ledger.sync_account_usage(period, &usage).await.unwrap();

    let audit = ledger.spend_audit(period).await.unwrap();
    assert_eq!(audit.dune_reported_microcredits, 100_000_000);
    assert_eq!(audit.ledger_settled_microcredits, 0);
    assert_eq!(
        audit.unaccounted_microcredits, 100_000_000,
        "spend the ledger never admitted must be visible as unaccounted"
    );
    // ~100 credits at the halfway point projects to ~200 by period end: nowhere near 3,600.
    assert_eq!(audit.alarm(), dune_ledger::SpendAlarm::Nominal);
}

#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn the_alarm_escalates_with_the_share_of_the_ceiling_used() {
    let (pool, period) = dated_fixture("escalate", 15, 30, 1_000_000_000).await;
    let ledger = Ledger::new(pool.clone());
    let usage = FakeUsage::reporting(0, 4_000_000_000);

    for (used, expected) in [
        (100_000_000_i64, dune_ledger::SpendAlarm::Nominal), // 10%
        (500_000_000, dune_ledger::SpendAlarm::Elevated),    // 50%
        (800_000_000, dune_ledger::SpendAlarm::High),        // 80%
        (1_000_000_000, dune_ledger::SpendAlarm::Breached),  // 100%
    ] {
        usage.set(used, 4_000_000_000);
        ledger.sync_account_usage(period, &usage).await.unwrap();
        let audit = ledger.spend_audit(period).await.unwrap();
        assert_eq!(
            audit.alarm(),
            expected,
            "at {} microcredits ({:.0}% of ceiling)",
            used,
            audit.pct_of_ceiling()
        );
    }
}

/// The point of the projection: catch a burn rate heading past the included allowance *before* the
/// allowance is gone, because overage is charged automatically the moment it is.
#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn a_burn_rate_heading_past_the_included_allowance_raises_a_high_alarm() {
    // One tenth of the period elapsed, ceiling 3,600 credits, 4,000 included.
    let (pool, period) = dated_fixture("burn", 3, 30, 3_600_000_000).await;
    let ledger = Ledger::new(pool.clone());

    // 500 credits in the first tenth projects to ~5,000 by period end: past the 4,000 included,
    // and therefore into automatic overage, despite only 14% of the ceiling being used so far.
    let usage = FakeUsage::reporting(500_000_000, 4_000_000_000);
    ledger.sync_account_usage(period, &usage).await.unwrap();
    let audit = ledger.spend_audit(period).await.unwrap();

    assert!(
        audit.pct_of_ceiling() < 20.0,
        "still well inside the ceiling"
    );
    assert!(audit.projection_is_meaningful());
    assert!(
        audit.projected_period_end_microcredits > 4_000_000_000,
        "projected {} should exceed the included allowance",
        audit.projected_period_end_microcredits
    );
    assert!(audit.projection_exceeds_included());
    assert_eq!(
        audit.alarm(),
        dune_ledger::SpendAlarm::High,
        "a burn rate heading into overage must alarm before the ceiling is anywhere near"
    );
}

/// A projection from a sliver of elapsed time is arithmetic, not information, and must not alarm.
#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn a_projection_from_almost_no_elapsed_period_does_not_cry_wolf() {
    // Half a day into a 30-day period: 1.7% elapsed, below the 3% meaningfulness floor.
    let (pool, period) = dated_fixture("early", 0, 30, 3_600_000_000).await;
    sqlx::query(
        "UPDATE dune_budget_periods SET starts_at = now() - interval '12 hours' WHERE id = $1",
    )
    .bind(period)
    .execute(&pool)
    .await
    .unwrap();
    let ledger = Ledger::new(pool.clone());
    let usage = FakeUsage::reporting(20_000_000, 4_000_000_000);
    ledger.sync_account_usage(period, &usage).await.unwrap();

    let audit = ledger.spend_audit(period).await.unwrap();
    assert!(
        !audit.projection_is_meaningful(),
        "1.7% elapsed is too little to project from"
    );
    assert!(!audit.projection_exceeds_included());
    assert_eq!(audit.alarm(), dune_ledger::SpendAlarm::Nominal);
}

/// Settled ledger spend is accounted for and must not be double-counted as unaccounted.
#[tokio::test]
#[ignore = "requires isolated DUNE_TEST_DATABASE_URL; creates its own schema"]
async fn ledger_admitted_spend_is_not_reported_as_unaccounted() {
    let (pool, period) = dated_fixture("accounted", 15, 30, 1_000_000_000).await;
    let ledger = Ledger::new(pool.clone());

    let permit = ledger
        .reserve(period, "A", 8699345, "fixture", 50_000_000)
        .await
        .expect("fits");
    let submitted = permit.claim(8699345).await.expect("claimed");
    submitted
        .attach_execution("01AUDIT0000000000000000001")
        .await
        .expect("attached");
    ledger
        .settle("01AUDIT0000000000000000001", 40_000_000)
        .await
        .expect("settled");

    // Dune reports exactly that 40 credits and nothing else.
    let usage = FakeUsage::reporting(40_000_000, 4_000_000_000);
    ledger.sync_account_usage(period, &usage).await.unwrap();

    let audit = ledger.spend_audit(period).await.unwrap();
    assert_eq!(audit.ledger_settled_microcredits, 40_000_000);
    assert_eq!(
        audit.unaccounted_microcredits, 0,
        "spend this ledger admitted and settled is accounted for"
    );
}
