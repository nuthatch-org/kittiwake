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
    let pool_for_support = pool.clone();
    let ext_for_support = ext.clone();
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

    // The graph-support mirror. Twelve hours, because the archive is a store of write-ups rather
    // than a feed and nothing in it moves faster than that.
    //
    // Lives here rather than in `ingest_jobs` for the same reason chain health does: it writes one
    // table that is ours alone, which the platform neither reads nor writes, so running it costs
    // nothing and turning it off gains nothing. It also touches no nest, so its offset is about
    // tidiness rather than permits.
    if let Some(pool) = pool_for_support {
        let ext = ext_for_support;
        jobs.push(
            JobDef::every(
                "ingest-graph-support",
                mins(720),
                Arc::new(move |_ctx| {
                    let pool = pool.clone();
                    let ext = ext.clone();
                    Box::pin(async move {
                        kittiwake_ingest::graph_support::graph_support(&pool, &ext)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(8)),
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

    // The QoS days the routes and the score read. Every half hour, because today is recomputed on
    // every run and the oracle posts every five minutes; closed days are computed only when missing
    // or changed.
    {
        let pool = pool.clone();
        let mut job = JobDef::every(
            "ingest-qos-days",
            mins(30),
            Arc::new(move |ctx| {
                let pool = pool.clone();
                Box::pin(async move {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    kittiwake_ingest::qos_days::fill_qos_days(&pool, &ctx.gate, ctx.lane, now)
                        .await
                        .map(|i| JobOutcome {
                            rows_written: i.rows,
                        })
                        .map_err(|e| e.to_string())
                })
            }),
        )
        .staggered(mins(21));
        // A day is a handful of nest statements and takes 10 to 20 s on a busy nest, so the 90-day
        // window cannot finish inside the default five minutes. Under the 30-minute period.
        job.timeout = mins(25);
        jobs.push(job);
    }

    // Hourly, for the rewards-history chart, which keeps each day's last. The first run after a gap
    // fills it a nest statement per day, so it gets the longer timeout. #236
    {
        let pool = pool.clone();
        let mut job = JobDef::every(
            "snapshot-indexers",
            mins(60),
            Arc::new(move |ctx| {
                let pool = pool.clone();
                Box::pin(async move {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    kittiwake_ingest::snapshots::snapshot_indexers(&pool, &ctx.gate, ctx.lane, now)
                        .await
                        .map(|i| JobOutcome {
                            rows_written: i.rows,
                        })
                        .map_err(|e| e.to_string())
                })
            }),
        )
        .staggered(mins(56));
        job.timeout = mins(25);
        jobs.push(job);
    }

    // Hourly: the oracle's documents resolve half an hour behind the chain, and a thirty-day score
    // moves by a day's weight at most.
    {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "score-qos",
                mins(60),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    Box::pin(async move {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        kittiwake_ingest::qos_score::score_qos(&pool, &ctx.gate, ctx.lane, now)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            .staggered(mins(38)),
        );
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

    // Names need mainnet for ENS, so without a chain there is nothing honest to write.
    if let Some(chain) = chain.clone() {
        let pool = pool.clone();
        jobs.push(
            JobDef::every(
                "ingest-names",
                mins(60),
                Arc::new(move |ctx| {
                    let pool = pool.clone();
                    let chain = chain.clone();
                    Box::pin(async move {
                        kittiwake_ingest::names::names(&pool, &ctx.gate, ctx.lane, &chain)
                            .await
                            .map(|i| JobOutcome {
                                rows_written: i.rows,
                            })
                            .map_err(|e| e.to_string())
                    })
                }),
            )
            // Hourly at minute 11, the earliest free one. Until its first run every reader treats the
            // name columns as empty; see `kittiwake_db::NAMES_CURSOR_KEY`.
            .staggered(mins(11)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// No two jobs may fire at the same moment in an hour.
    ///
    /// The earlier version of this only compared jobs that shared a period, which let a
    /// thirty-minute job and a ten-minute job both land on the fourth minute and contend for the
    /// same nest permits - exactly the collision the offsets exist to prevent. The one-minute
    /// warmers are excluded because they fire every minute by definition and would make the
    /// property unsatisfiable; they draw from the same background lane and queue rather than failing.
    #[test]
    fn no_two_jobs_fire_at_the_same_minute_of_the_hour() {
        let schedule: Vec<(&str, u64, u64)> = vec![
            ("ingest-epochs", 10, 3),
            ("ingest-delegations", 15, 1),
            ("ingest-allocations", 60, 18),
            ("ingest-names", 60, 11),
            ("ingest-rav", 60, 28),
            ("ingest-disputes", 60, 48),
            ("score-qos", 60, 38),
            ("snapshot-indexers", 60, 56),
            ("ingest-qos-days", 30, 21),
            ("refresh", 5, 0),
            ("snapshot-network", 5, 2),
            ("warm-ipfs", 10, 4),
            ("refresh-chain-health", 30, 6),
            ("reconcile-bounties", 10, 9),
            ("ingest-graph-support", 720, 8),
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
            ("ingest-names", mins(60), mins(11)),
            ("ingest-rav", mins(60), mins(28)),
            ("ingest-disputes", mins(360), mins(48)),
            ("score-qos", mins(60), mins(38)),
            ("snapshot-indexers", mins(60), mins(56)),
            ("ingest-qos-days", mins(30), mins(21)),
            ("refresh", mins(5), Duration::ZERO),
            ("snapshot-network", mins(5), mins(2)),
            ("warm", mins(1), Duration::ZERO),
            ("warm-indexers", mins(1), Duration::from_secs(30)),
            ("warm-ipfs", mins(10), mins(4)),
            ("refresh-chain-health", mins(30), mins(6)),
            ("reconcile-bounties", mins(10), mins(9)),
            ("ingest-graph-support", mins(720), mins(8)),
        ];
        for (i, a) in names_and_offsets.iter().enumerate() {
            for b in names_and_offsets.iter().skip(i + 1) {
                if a.1 == b.1 {
                    assert_ne!(a.2, b.2, "{} and {} share a period and an offset", a.0, b.0);
                }
            }
        }
    }
}
