//! Live end-to-end probe of the subscription ceiling, against the real Dune account and a real
//! Postgres. Read-only as far as Dune is concerned: it calls `POST /api/v1/usage`, which is not
//! billed, and never starts an execution.
//!
//! ```sh
//! DUNE_API_KEY=... DUNE_TEST_DATABASE_URL=postgresql://... \
//!   cargo run -p dune-client --features ledger --example ceiling
//! ```
//!
//! It creates its own schema, proves the chain, and drops the schema again.

use dune_client::DuneClient;
use dune_ledger::{Ledger, MIGRATIONS};
use sqlx::postgres::PgPoolOptions;

/// The registry's `monthly_budget_microcredits`: 3,600 credits, holding back 10% of the 4,000
/// included so the ledger never spends the account to its limit.
const CONFIGURED_CEILING: i64 = 3_600_000_000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("DUNE_API_KEY").map_err(|_| "DUNE_API_KEY is required")?;
    let url = std::env::var("DUNE_TEST_DATABASE_URL")
        .map_err(|_| "DUNE_TEST_DATABASE_URL is required; never point this at production")?;
    let account = std::env::var("DUNE_ACCOUNT").unwrap_or_else(|_| "pete44".into());
    let client = DuneClient::new(key.into())?;

    // 1. What Dune itself says, straight from the endpoint.
    let usage = client.usage().await?;
    let period = usage.current().ok_or("no billing period reported")?;
    println!(
        "live account: {} to {}, {} of {} credits used",
        period.start_date, period.end_date, period.credits_used, period.credits_included
    );

    // 2. An isolated schema, both migrations, no seeded period: a fresh deployment.
    let schema = format!(
        "dune_ceiling_live_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    // Audited: identifier is a fixed prefix plus a locally generated integer timestamp.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let sp = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _| {
            let sp = sp.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(sp)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    for migration in MIGRATIONS {
        sqlx::raw_sql(*migration).execute(&pool).await?;
    }

    // 3. Admission must refuse before anything is seeded.
    let strict =
        Ledger::new(pool.clone()).requiring_fresh_observation(std::time::Duration::from_secs(3600));
    println!("before seeding, headroom query: {:?}", {
        let l = Ledger::new(pool.clone());
        l.remaining_headroom(1).await.map(|h| h.to_string())
    });

    // 4. Seed from Dune's own dates, with the live figures.
    let period_id = strict
        .ensure_period(&account, &client, CONFIGURED_CEILING)
        .await?;
    let (starts, ends, limit, observed, included): (String, String, i64, i64, Option<i64>) =
        sqlx::query_as(
            "SELECT starts_at::date::text, ends_at::date::text, limit_microcredits, \
             observed_used_microcredits, observed_included_microcredits \
             FROM dune_budget_periods WHERE id = $1",
        )
        .bind(period_id)
        .fetch_one(&pool)
        .await?;
    println!(
        "seeded period {period_id}: {starts} to {ends}, ceiling {limit} uc, \
         observed used {observed} uc, included {included:?} uc"
    );

    // 5. The stored figures must match what the live endpoint reported.
    let expected_used = (period.credits_used * 1_000_000.0).ceil() as i64;
    assert_eq!(starts, period.start_date, "period start must be Dune's");
    assert_eq!(ends, period.end_date, "period end must be Dune's");
    assert_eq!(observed, expected_used, "observed spend must be Dune's");
    assert_eq!(
        limit, CONFIGURED_CEILING,
        "ceiling below included allowance"
    );

    // 6. Headroom, and an allocation so reservations have somewhere to land.
    let headroom = strict.remaining_headroom(period_id).await?;
    println!(
        "headroom: {headroom} uc ({:.3} credits) of {:.0} configured",
        headroom as f64 / 1_000_000.0,
        CONFIGURED_CEILING as f64 / 1_000_000.0
    );
    assert_eq!(headroom, CONFIGURED_CEILING - observed);
    sqlx::query("INSERT INTO dune_budget_allocations VALUES ($1, 'A', $2)")
        .bind(period_id)
        .bind(CONFIGURED_CEILING)
        .execute(&pool)
        .await?;

    // 7. A reservation inside the headroom is admitted.
    let small = strict
        .reserve(period_id, "A", 8699345, "kittiwake", 1_000_000)
        .await;
    println!(
        "1-credit reservation against live headroom: {:?}",
        small.is_ok()
    );
    assert!(small.is_ok(), "a 1-credit reservation should fit");

    // 8. One that would breach the ceiling is refused, using the account's real spend.
    let huge = strict
        .reserve(period_id, "A", 8699345, "kittiwake", headroom)
        .await;
    println!(
        "reservation of the entire remaining headroom: {:?}",
        huge.is_err()
    );
    assert!(
        huge.is_err(),
        "the 1 credit already held must make a full-headroom reservation impossible"
    );

    // 9. A stale observation must refuse everything, even well inside the ceiling.
    sqlx::query(
        "UPDATE dune_budget_periods SET observed_at = now() - interval '2 hours' WHERE id = $1",
    )
    .bind(period_id)
    .execute(&pool)
    .await?;
    let stale = strict
        .reserve(period_id, "A", 8699345, "kittiwake", 1_000)
        .await;
    println!(
        "reservation against a 2-hour-stale observation: refused = {}",
        stale.is_err()
    );
    assert!(stale.is_err(), "a stale observation must refuse admission");

    // 10. A refresh clears it.
    strict
        .ensure_period(&account, &client, CONFIGURED_CEILING)
        .await?;
    let after = strict
        .reserve(period_id, "A", 8699345, "kittiwake", 1_000)
        .await;
    println!("after refresh: admitted = {}", after.is_ok());
    assert!(
        after.is_ok(),
        "a refreshed observation permits admission again"
    );

    // 11. The spend audit on real figures. Until the Dune-side extra-credit limit is set, this is
    //     the whole warning system, so it is worth seeing against live numbers rather than only
    //     against fixtures.
    let audit = strict.spend_audit(period_id).await?;
    let uc = 1_000_000.0;
    println!("\nspend audit (live figures):");
    println!(
        "  dune reported      {:>12.3} credits",
        audit.dune_reported_microcredits as f64 / uc
    );
    println!(
        "  ledger settled     {:>12.3} credits",
        audit.ledger_settled_microcredits as f64 / uc
    );
    println!(
        "  unaccounted        {:>12.3} credits  <- spend that never passed through the ledger",
        audit.unaccounted_microcredits as f64 / uc
    );
    println!(
        "  period elapsed     {:>12.1}%",
        audit.elapsed_fraction * 100.0
    );
    println!("  ceiling used       {:>12.2}%", audit.pct_of_ceiling());
    println!(
        "  projected at end   {:>12.3} credits (meaningful: {})",
        audit.projected_period_end_microcredits as f64 / uc,
        audit.projection_is_meaningful()
    );
    println!("  alarm              {:>12?}", audit.alarm());

    // Audited: same generated identifier as above.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    println!("\nall ceiling assertions held against the live account; schema dropped");
    Ok(())
}
