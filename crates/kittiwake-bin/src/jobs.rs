//! The ingest jobs, and the offsets that keep them out of each other's way.
//!
//! As platform crons these fired on shared boundaries with no ordering: `refresh` and
//! `snapshot-network` both landed on every fifth minute and raced for the same nest permits, and
//! the loser read `server busy`. Here every job on a shared period gets its own offset inside that
//! period, and all of them draw from the gate's background lane, so a collision is not merely
//! unlikely but impossible to construct.

use std::sync::Arc;
use std::time::Duration;

use kittiwake_db::Pool;
use kittiwake_ext::Ext;
use kittiwake_sched::{JobDef, JobOutcome};

const MINUTE: u64 = 60;

fn mins(n: u64) -> Duration {
    Duration::from_secs(n * MINUTE)
}

/// Every nest-to-Postgres job, staggered.
///
/// The offsets started as the platform's own, kept so a comparison between the two systems would be
/// like for like. They are not any more, and the reason is worth recording: the platform schedule
/// collides with itself. Epochs on `3-59/10` and delegations on `8-59/15` both land on :23 and :53,
/// and the hourly RAV ingest on :23 joins them. Three jobs, one moment, one nest.
///
/// So the offsets here are chosen instead of inherited, and `no_two_jobs_fire_at_the_same_minute_of_the_hour`
/// holds them to it. Anyone diffing the two schedules should expect delegations at :01 rather than
/// :08, allocations at :18 rather than :13, RAV at :28 rather than :23, and disputes at :48 rather
/// than on the hour.
/// Jobs that write only into this process's own memory.
///
/// Safe to run whenever the service is up, because nothing outside this process can see what they
/// write. The distinction that matters is not "is it a cron" but "does it write to a store the
/// other system also writes to" - and these do not.
pub fn live_jobs(store: kittiwake_cache::Store, pool: Option<Pool>, ext: Ext) -> Vec<JobDef> {
    let mut jobs = Vec::new();
    let pool_for_warm = pool.clone();
    {
        // Every two minutes, as the platform schedule had it. Writes into the process's own store
        // rather than Redis, because the route that reads it is in this process.
        let store = store.clone();
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-horizon-activity",
                Duration::from_secs(120),
                Arc::new(move |ctx| {
                    let store = store.clone();
                    let pool = pool.clone();
                    Box::pin(async move {
                        kittiwake_ingest::live::horizon_activity(
                            &store,
                            pool.as_ref(),
                            &ctx.gate,
                            ctx.lane,
                        )
                        .await
                        .map(|i| JobOutcome {
                            rows_written: i.rows,
                        })
                        .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(Duration::from_secs(30)),
        );
    }

    // Chain health writes its history to two tables of its own, which nothing else reads or
    // writes, so it is not gated with the shared-Postgres writers below. With no database it
    // still runs, keeping that history in the process instead - see `live::History` for what that
    // costs and why it is preferable to not running the job at all.
    {
        let store = store.clone();
        let history = Arc::new(match pool {
            Some(p) => kittiwake_ingest::live::History::Db(p),
            None => {
                tracing::warn!(
                    "no database_url: chain-head history is kept in memory, so a restart resets \
                     the stall clock and a halted chain reads live again for up to six hours"
                );
                kittiwake_ingest::live::History::Memory(Default::default())
            }
        });
        jobs.push(
            JobDef::every(
                "refresh-chain-health",
                mins(30),
                Arc::new(move |ctx| {
                    let store = store.clone();
                    let history = history.clone();
                    let ext = ext.clone();
                    Box::pin(async move {
                        let now = chrono::Utc::now().timestamp_millis();
                        kittiwake_ingest::live::chain_health(
                            &store, &history, &ctx.gate, ctx.lane, &ext, now,
                        )
                        .await
                        .map(|i| JobOutcome {
                            rows_written: i.rows,
                        })
                        .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(6)),
        );
    }

    // The IPFS cache behind search. Ten minutes, as the platform schedule had it, offset off the
    // chain-health run so the two never contend for the same nest permits.
    if let Some(pool) = pool_for_warm {
        let ipfs = kittiwake_ext::Ipfs::default();
        jobs.push(
            JobDef::every(
                "warm-ipfs",
                mins(10),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    let ipfs = ipfs.clone();
                    Box::pin(async move {
                        kittiwake_ingest::live::warm_ipfs(&pool, &ipfs, &ctx.gate, ctx.lane)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(4)),
        );
    }

    jobs
}

/// Jobs that write to Lodestar's Postgres.
///
/// Gated behind `scheduler_enabled`, because turning them on makes this process a second writer to
/// a database the platform's own crons are still writing. That is a decision, taken once, after a
/// shadow run - not a default.
pub fn ingest_jobs(
    pool: Pool,
    ext: Ext,
    chain: Option<kittiwake_chain::Chain>,
    bounty_board: Option<String>,
) -> Vec<JobDef> {
    let mut jobs = Vec::new();

    {
        let pool = pool.clone();
        let chain = chain.clone();
        jobs.push(JobDef::every(
            "refresh",
            mins(5),
            Arc::new(move |ctx| {
                let pool = pool.clone();
                let chain = chain.clone();
                Box::pin(async move {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    kittiwake_ingest::refresh::refresh(
                        &pool,
                        &ctx.gate,
                        ctx.lane,
                        chain.as_ref(),
                        now,
                    )
                    .await
                    .map(|i| JobOutcome {
                        rows_written: i.rows,
                    })
                    .map_err(|e| e.to_string())
                })
            }),
        ));
    }

    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-epochs",
                mins(10),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    Box::pin(async move {
                        kittiwake_ingest::epochs(&pool, &ctx.gate, ctx.lane)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(3)),
        );
    }

    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-delegations",
                mins(15),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    Box::pin(async move {
                        kittiwake_ingest::delegation_events(&pool, &ctx.gate, ctx.lane)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(1)),
        );
    }

    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-allocations",
                mins(60),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    Box::pin(async move {
                        kittiwake_ingest::allocations(&pool, &ctx.gate, ctx.lane)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(18)),
        );
    }

    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-rav",
                mins(60),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    Box::pin(async move {
                        // Never a backfill on the schedule. A backfill re-reads every collection
                        // ever made, which is a different kind of run in cost and in duration, and
                        // it belongs behind a deliberate invocation rather than on an hourly timer.
                        kittiwake_ingest::rav::rav(&pool, &ctx.gate, ctx.lane, false)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(28)),
        );
    }

    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-disputes",
                mins(360),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    Box::pin(async move {
                        kittiwake_ingest::disputes(&pool, &ctx.gate, ctx.lane)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(48)),
        );
    }

    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "snapshot-network",
                mins(5),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    let ext = ext.clone();
                    Box::pin(async move {
                        // The price and TVL are decoration on a row of protocol figures. If the
                        // outside source is down the snapshot still gets written, with those two
                        // columns null, because a gap in a chart is honest and a missing row is not.
                        let price = ext.grt_price().await.ok().map(|(usd, _)| usd);
                        let tvl = ext.arbitrum_tvl().await.ok();
                        if price.is_none() {
                            tracing::debug!(
                                "price source unavailable; snapshot written without it"
                            );
                        }
                        kittiwake_ingest::network_snapshot(&pool, &ctx.gate, ctx.lane, price, tvl)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(2)),
        );
    }

    // The seventeenth cron, and the last one outside the product layer. It reads the chain and
    // writes `sync_bounties`; no signing key and nothing spent, which is the whole difference
    // between this and `tap-provision`. #30.
    //
    // Both halves are required, and the job simply does not exist without them. A board address
    // with no RPC cannot be read, and an RPC with no board has nothing to read - either way the
    // honest thing is not to schedule a job that would log a failure every ten minutes.
    if let (Some(chain), Some(board)) = (chain.clone(), bounty_board) {
        jobs.push(
            JobDef::every(
                "reconcile-bounties",
                mins(10),
                Arc::new(move |_ctx| {
                    let pool = pool.clone();
                    let chain = chain.clone();
                    let board = board.clone();
                    Box::pin(async move {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        kittiwake_ingest::bounties::reconcile(&pool, &chain, &board, now)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            // The platform ran it on the ten-minute boundary, which is `refresh`'s. Nine is the
            // only offset whose six firings are all free; see the schedule test below.
            .staggered(mins(9)),
        );
    }

    jobs
}

/// Refreshes the ledger's record of what Dune says the account has actually spent.
///
/// This is what makes the subscription ceiling real rather than notional. The ledger's own books
/// only cover work it admitted; an execution started from the Dune web UI, a Dune-side schedule,
/// or another holder of the key spends the same allowance invisibly. The usage endpoint is not
/// billed, so this job costs nothing to run often.
///
/// If it fails, the stored figure and its timestamp are left alone, so a ledger configured with
/// `requiring_fresh_observation` will start refusing work rather than spend against a number
/// nobody has confirmed.
pub fn dune_usage_sync_job(
    client: Arc<dune_client::DuneClient>,
    ledger: dune_ledger::Ledger,
    account: String,
    period: Duration,
) -> JobDef {
    JobDef::every(
        "dune-usage-sync",
        period,
        Arc::new(move |_ctx| {
            let client = client.clone();
            let ledger = ledger.clone();
            let account = account.clone();
            Box::pin(async move {
                match ledger.sync_active_period(&account, client.as_ref()).await {
                    Ok(used) => {
                        tracing::debug!(
                            account = %account,
                            used_microcredits = used,
                            "Dune account usage refreshed"
                        );
                        Ok(JobOutcome { rows_written: 0 })
                    }
                    Err(e) => Err(format!("Dune usage sync failed for {account}: {e}")),
                }
            })
        }),
    )
}

/// Dune feed poller jobs driven by the query registry.
pub fn dune_jobs(
    client: Arc<dune_client::DuneClient>,
    pool: Pool,
    ledger: dune_ledger::Ledger,
    account: String,
    registry: dune_registry::Registry,
) -> Vec<JobDef> {
    let mut jobs = Vec::new();
    for (i, query) in registry.query.into_iter().enumerate() {
        if !query.enabled {
            continue;
        }
        let client = client.clone();
        let pool = pool.clone();
        let ledger = ledger.clone();
        let account = account.clone();
        let period = Duration::from_secs(query.poll_seconds);
        // Stagger Dune queries across half-minutes so they don't fire simultaneously
        let stagger = Duration::from_secs((i as u64 * 30) % 300);
        let name: &'static str = Box::leak(format!("dune-poll-{}", query.id).into_boxed_str());

        jobs.push(
            JobDef::every(
                name,
                period,
                Arc::new(move |_ctx| {
                    let client = client.clone();
                    let pool = pool.clone();
                    let ledger = ledger.clone();
                    let account = account.clone();
                    let query = query.clone();
                    Box::pin(async move {
                        match dune_poller::poll_and_materialize(
                            &client,
                            &pool,
                            Some(&ledger),
                            &account,
                            &query,
                        )
                        .await
                        {
                            Ok(changed) => Ok(JobOutcome {
                                rows_written: if changed { 1 } else { 0 },
                            }),
                            Err(e) => Err(format!("Dune poll failed for {}: {e}", query.id)),
                        }
                    })
                }),
            )
            .staggered(stagger),
        );
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No two jobs may fire at the same moment in an hour.
    ///
    /// The earlier version of this only compared jobs that shared a period, which let a
    /// thirty-minute job and a ten-minute job both land on the fourth minute and contend for the
    /// same nest permits - exactly the collision the offsets exist to prevent. The one-minute
    /// warmer is excluded because it fires every minute by definition and would make the property
    /// unsatisfiable; it draws from the same background lane and queues rather than failing.
    #[test]
    fn no_two_jobs_fire_at_the_same_minute_of_the_hour() {
        let schedule: Vec<(&str, u64, u64)> = vec![
            ("ingest-epochs", 10, 3),
            ("ingest-delegations", 15, 1),
            ("ingest-allocations", 60, 18),
            ("ingest-rav", 60, 28),
            ("ingest-disputes", 60, 48),
            ("refresh", 5, 0),
            ("snapshot-network", 5, 2),
            ("warm-ipfs", 10, 4),
            ("refresh-chain-health", 30, 6),
            ("reconcile-bounties", 10, 9),
        ];
        let fires = |period: u64, offset: u64| -> Vec<u64> {
            (0..60)
                .filter(|m| *m >= offset && (m - offset).is_multiple_of(period))
                .collect()
        };
        for (i, a) in schedule.iter().enumerate() {
            for b in schedule.iter().skip(i + 1) {
                let (fa, fb) = (fires(a.1, a.2), fires(b.1, b.2));
                let clash: Vec<u64> = fa.iter().filter(|m| fb.contains(m)).copied().collect();
                assert!(
                    clash.is_empty(),
                    "{} and {} both fire at minute {:?}",
                    a.0,
                    b.0,
                    clash
                );
            }
        }
    }

    /// Kept as well: a shared period with a shared offset is the same fault stated more directly.
    #[test]
    fn no_two_jobs_on_the_same_period_share_an_offset() {
        // Built without a live pool: only the schedule shape is under test.
        let names_and_offsets: Vec<(&str, Duration, Duration)> = vec![
            ("ingest-epochs", mins(10), mins(3)),
            ("ingest-delegations", mins(15), mins(1)),
            ("ingest-allocations", mins(60), mins(18)),
            ("ingest-rav", mins(60), mins(28)),
            ("ingest-disputes", mins(360), mins(48)),
            ("refresh", mins(5), Duration::ZERO),
            ("snapshot-network", mins(5), mins(2)),
            ("warm", mins(1), Duration::ZERO),
            ("warm-ipfs", mins(10), mins(4)),
            ("refresh-chain-health", mins(30), mins(6)),
            ("reconcile-bounties", mins(10), mins(9)),
        ];
        for (i, a) in names_and_offsets.iter().enumerate() {
            for b in names_and_offsets.iter().skip(i + 1) {
                if a.1 == b.1 {
                    assert_ne!(a.2, b.2, "{} and {} share a period and an offset", a.0, b.0);
                }
            }
        }
    }

    /// A registry with one enabled and two disabled queries. `enabled = false` is the only thing
    /// standing between a pilot entry and a live schedule that spends real credits, so the
    /// distinction is worth a test.
    fn registry_fixture() -> dune_registry::Registry {
        dune_registry::Registry::parse(
            r#"
monthly_budget_microcredits = 4000000000
[allocations]
A = 2100000000

[[query]]
id = "grt_price_daily"
dune_query_id = 8699345
owner = "pete44"
workstream = "A"
enabled = true
schedule_seconds = 86400
poll_seconds = 3600
execution_estimate_microcredits = 1000000
export_estimate_microcredits = 1000000
result_limit = 1000
target_table = "dune_grt_price_daily"
freshness_seconds = 172800

[[query]]
id = "grt_whale_moves"
dune_query_id = 8699346
owner = "pete44"
workstream = "A"
enabled = false
schedule_seconds = 43200
poll_seconds = 3600
execution_estimate_microcredits = 1000000
export_estimate_microcredits = 1000000
result_limit = 1000
target_table = "dune_grt_whale_moves"
freshness_seconds = 172800

[[query]]
id = "grt_bridge_flow"
dune_query_id = 8699347
owner = "pete44"
workstream = "A"
enabled = false
schedule_seconds = 86400
poll_seconds = 7200
execution_estimate_microcredits = 1000000
export_estimate_microcredits = 1000000
result_limit = 1000
target_table = "dune_grt_bridge_flow"
freshness_seconds = 172800
"#,
        )
        .expect("registry fixture parses")
    }

    /// A lazy pool never opens a connection, so the job shapes can be built without a database.
    fn lazy_pool() -> Pool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgresql://unused@127.0.0.1:1/unused")
            .expect("lazy pool")
    }

    fn dune_fixture_jobs() -> Vec<JobDef> {
        let client =
            Arc::new(dune_client::DuneClient::new("fixture-key".into()).expect("client builds"));
        let pool = lazy_pool();
        dune_jobs(
            client,
            pool.clone(),
            dune_ledger::Ledger::new(pool),
            "pete44".into(),
            registry_fixture(),
        )
    }

    #[tokio::test]
    async fn only_enabled_queries_are_scheduled() {
        let jobs = dune_fixture_jobs();
        assert_eq!(
            jobs.len(),
            1,
            "a disabled registry entry must never reach the scheduler"
        );
        assert_eq!(jobs[0].name, "dune-poll-grt_price_daily");
    }

    #[tokio::test]
    async fn a_poll_job_uses_the_registry_poll_interval_not_its_schedule() {
        let jobs = dune_fixture_jobs();
        // 3600 is poll_seconds; 86400 is schedule_seconds. Polling on the schedule interval would
        // leave results stale for a day, and polling is free, so the two must not be confused.
        assert!(matches!(
            jobs[0].schedule,
            kittiwake_sched::Schedule::Every(p) if p == Duration::from_secs(3600)
        ));
    }

    #[tokio::test]
    async fn dune_poll_jobs_do_not_share_an_offset_with_each_other() {
        // Force several enabled entries to check the stagger actually separates them.
        let mut registry = registry_fixture();
        for q in registry.query.iter_mut() {
            q.enabled = true;
            q.poll_seconds = 3600;
        }
        let client =
            Arc::new(dune_client::DuneClient::new("fixture-key".into()).expect("client builds"));
        let pool = lazy_pool();
        let jobs = dune_jobs(
            client,
            pool.clone(),
            dune_ledger::Ledger::new(pool),
            "pete44".into(),
            registry,
        );
        assert_eq!(jobs.len(), 3);
        let mut offsets: Vec<Duration> = jobs.iter().map(|j| j.stagger).collect();
        offsets.sort();
        offsets.dedup();
        assert_eq!(offsets.len(), 3, "staggered jobs must not converge");
    }

    #[tokio::test]
    async fn the_usage_sync_job_is_named_and_paced_as_configured() {
        let client =
            Arc::new(dune_client::DuneClient::new("fixture-key".into()).expect("client builds"));
        let job = dune_usage_sync_job(
            client,
            dune_ledger::Ledger::new(lazy_pool()),
            "pete44".into(),
            Duration::from_secs(900),
        );
        assert_eq!(job.name, "dune-usage-sync");
        assert!(matches!(
            job.schedule,
            kittiwake_sched::Schedule::Every(p) if p == Duration::from_secs(900)
        ));
    }
}
