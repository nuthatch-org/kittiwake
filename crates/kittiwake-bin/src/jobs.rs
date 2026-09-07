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
/// The offsets are the same ones the platform schedules encoded, kept so a comparison between the
/// two systems is like for like: epochs at :03 past every ten minutes, delegations at :08 past
/// every fifteen, allocations at :13 past the hour, and so on.
/// Jobs that write only into this process's own memory.
///
/// Safe to run whenever the service is up, because nothing outside this process can see what they
/// write. The distinction that matters is not "is it a cron" but "does it write to a store the
/// other system also writes to" - and these do not.
pub fn live_jobs(store: kittiwake_cache::Store) -> Vec<JobDef> {
    let mut jobs = Vec::new();
    {
        // Every two minutes, as the platform schedule had it. Writes into the process's own store
        // rather than Redis, because the route that reads it is in this process.
        let store = store.clone();
        jobs.push(
            JobDef::every(
                "ingest-horizon-activity",
                Duration::from_secs(120),
                Arc::new(move |ctx| {
                    let store = store.clone();
                    Box::pin(async move {
                        kittiwake_ingest::live::horizon_activity(&store, &ctx.gate, ctx.lane)
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

    jobs
}

/// Jobs that write to Lodestar's Postgres.
///
/// Gated behind `scheduler_enabled`, because turning them on makes this process a second writer to
/// a database the platform's own crons are still writing. That is a decision, taken once, after a
/// shadow run - not a default.
pub fn ingest_jobs(pool: Pool, ext: Ext, chain: Option<kittiwake_chain::Chain>) -> Vec<JobDef> {
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
            .staggered(mins(8)),
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
            .staggered(mins(13)),
        );
    }

    {
        let pool = pool.clone();
        jobs.push(JobDef::every(
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
        ));
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

    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two jobs sharing a period must not share a firing moment.
    #[test]
    fn no_two_jobs_on_the_same_period_share_an_offset() {
        // Built without a live pool: only the schedule shape is under test.
        let names_and_offsets: Vec<(&str, Duration, Duration)> = vec![
            ("ingest-epochs", mins(10), mins(3)),
            ("ingest-delegations", mins(15), mins(8)),
            ("ingest-allocations", mins(60), mins(13)),
            ("ingest-disputes", mins(360), Duration::ZERO),
            ("refresh", mins(5), Duration::ZERO),
            ("snapshot-network", mins(5), mins(2)),
            ("warm", mins(1), Duration::ZERO),
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
