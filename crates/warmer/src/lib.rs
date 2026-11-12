//! Keeping the expensive folds hot, so no reader ever meets one cold.
//!
//! This is the whole answer to the 23.5-second `/api/indexers`. The nest computes a whole-history
//! fold and we are not allowed to change that, so the fold is moved off the request path: it runs
//! on the dataset's own cadence, on the background lane, and every reader gets the result of a
//! fold that already happened.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::BoxFuture;
use kittiwake_read::{Read, ReadCtx, qos, routes};
use kittiwake_types::{ApiError, ServedFrom};

#[derive(Debug, Default)]
pub struct WarmReport {
    pub warmed: u64,
    /// Answers young enough to be left alone.
    pub kept: u64,
    pub failed: u64,
}

/// Warm the whole-list folds. The per-indexer pages are [`IndexerPages`].
pub async fn warm_all(ctx: &ReadCtx) -> WarmReport {
    let ctx = &ctx.warming(LIST_AHEAD);
    let began = Instant::now();
    let mut report = WarmReport::default();

    let indexers = routes::indexers(
        ctx,
        routes::IndexersParams {
            first: 100,
            skip: 0,
            order_by: "stakedTokens".into(),
            descending: true,
        },
    )
    .await;
    tally(&mut report, "indexers", indexers.is_ok(), indexers.err());

    let network = routes::network_stats(ctx).await;
    tally(&mut report, "network-stats", network.is_ok(), network.err());

    let epochs = routes::epochs(ctx, 20).await;
    tally(&mut report, "epochs", epochs.is_ok(), epochs.err());

    metrics::histogram!("warmer_duration_seconds").record(began.elapsed().as_secs_f64());
    tracing::debug!(
        warmed = report.warmed,
        failed = report.failed,
        "warm pass complete"
    );
    report
}

fn tally(
    report: &mut WarmReport,
    route: &'static str,
    ok: bool,
    err: Option<kittiwake_types::ApiError>,
) {
    if ok {
        report.warmed += 1;
        metrics::counter!("warmer_total", "route" => route, "outcome" => "ok").increment(1);
    } else {
        report.failed += 1;
        metrics::counter!("warmer_total", "route" => route, "outcome" => "error").increment(1);
        // A failed warm is not a failed request: nobody was waiting. It matters only if it keeps
        // happening, which is what the counter is for.
        if let Some(e) = err {
            tracing::debug!(route, error = %e, "warm failed");
        }
    }
}

/// The rows `/api/indexers-enriched` returns: the directory Lodestar lists.
pub const DIRECTORY_SIZE: i64 = 100;

/// Each list fold is served stale for the five-minute cap, so this refreshes it as it goes stale.
const LIST_AHEAD: Duration = Duration::from_secs(5 * 60);

/// An entry is refreshed once it would expire within this. Longer than a walk of the directory
/// takes on a healthy nest, so the walk comes round again before any entry can expire.
pub const AHEAD: Duration = Duration::from_secs(20 * 60);

/// The work one run may do. Each run then rests as long as it worked, so the warmer is idle at
/// least half the time however slow the nest is.
pub const WORK_PER_RUN: Duration = Duration::from_secs(30);

/// How long a nest that refused a warm is left alone.
pub const BACKOFF: Duration = Duration::from_secs(10 * 60);

// What Lodestar's indexer page asks for on load. An entry warmed under any other window is one
// nobody reads.
const REVENUE_WINDOW_DAYS: u32 = 30;
const TRENDS_DAYS: u32 = 90;
const STAKE_HISTORY_DAYS: i64 = 182;

/// Where the directory comes from: Postgres in production.
pub type Directory = Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<String>, String>> + Send + Sync>;

/// Keeps warm the reads behind each directory indexer's page that measured slow cold on
/// 2026-09-15. The rest of the page answered in under 0.7 s, or is not cached at all.
#[derive(Default)]
pub struct IndexerPages {
    walk: tokio::sync::Mutex<Walk>,
}

#[derive(Default)]
struct Walk {
    directory: Vec<String>,
    next: usize,
    began: Option<Instant>,
    warmed: u64,
    alloc_resumes: Option<Instant>,
    qos_resumes: Option<Instant>,
}

impl IndexerPages {
    pub fn new() -> Self {
        Self::default()
    }

    /// One run: from where the last one stopped until [`WORK_PER_RUN`] is spent or the directory is
    /// done, then a rest as long as the work.
    pub async fn run(&self, ctx: &ReadCtx, directory: &Directory) -> Result<WarmReport, String> {
        self.run_for(ctx, directory, WORK_PER_RUN).await
    }

    async fn run_for(
        &self,
        ctx: &ReadCtx,
        directory: &Directory,
        budget: Duration,
    ) -> Result<WarmReport, String> {
        let mut walk = self.walk.lock().await;
        if walk.next >= walk.directory.len() {
            walk.directory = directory().await?;
            walk.next = 0;
            walk.began = Some(Instant::now());
            walk.warmed = 0;
        }

        let warm = ctx.warming(AHEAD);
        let began = Instant::now();
        let mut report = WarmReport::default();
        while walk.next < walk.directory.len() {
            let addr = walk.directory[walk.next].clone();
            warm_indexer(&warm, &addr, &mut walk, &mut report).await;
            walk.next += 1;
            if began.elapsed() >= budget {
                break;
            }
        }
        walk.warmed += report.warmed;

        if walk.next >= walk.directory.len()
            && let Some(t) = walk.began.take()
        {
            metrics::histogram!("warmer_directory_seconds").record(t.elapsed().as_secs_f64());
            if walk.warmed > 0 {
                tracing::info!(
                    indexers = walk.directory.len(),
                    warmed = walk.warmed,
                    took_s = t.elapsed().as_secs(),
                    "indexer pages warmed"
                );
            }
        }

        tokio::time::sleep(began.elapsed()).await;
        Ok(report)
    }
}

async fn warm_indexer(ctx: &ReadCtx, addr: &str, walk: &mut Walk, report: &mut WarmReport) {
    let now = unix_now();
    if resumed(&mut walk.alloc_resumes) && !warm_alloc(ctx, addr, now, report).await {
        walk.alloc_resumes = Some(Instant::now() + BACKOFF);
    }
    if resumed(&mut walk.qos_resumes) {
        let warmed = qos::warm_qos_nest(ctx, addr, now.div_euclid(86_400)).await;
        if !record(report, "qos-today", warmed) {
            walk.qos_resumes = Some(Instant::now() + BACKOFF);
        }
    }
}

/// The allocation nest's reads, one after another. False at the first refusal.
async fn warm_alloc(ctx: &ReadCtx, addr: &str, now: i64, report: &mut WarmReport) -> bool {
    let revenue_since = kittiwake_rewards::pnl::window_start(now, REVENUE_WINDOW_DAYS);
    let trends_since = kittiwake_rewards::pnl::window_start(now, TRENDS_DAYS);
    let reads: Vec<(&'static str, BoxFuture<'_, Result<Read, ApiError>>)> = vec![
        ("indexer", routes::indexer(ctx, addr).boxed()),
        ("indexer-status", routes::indexer_status(ctx, addr).boxed()),
        (
            "provisions-indexer",
            routes::provisions_by_indexer(ctx, addr).boxed(),
        ),
        (
            "indexer-revenue",
            routes::indexer_revenue(ctx, addr, revenue_since, false).boxed(),
        ),
        (
            "indexer-pnl",
            routes::indexer_revenue(ctx, addr, revenue_since, true).boxed(),
        ),
        (
            "indexer-stake-history",
            routes::indexer_stake_history(ctx, addr, STAKE_HISTORY_DAYS).boxed(),
        ),
        (
            "indexer-trends",
            routes::indexer_trends(ctx, addr, trends_since).boxed(),
        ),
        (
            "qos-allocation-shares",
            qos::warm_allocation_shares(ctx, addr).boxed(),
        ),
    ];
    for (route, read) in reads {
        if !record(report, route, read.await.map(|r| vec![r])) {
            return false;
        }
    }
    true
}

/// True once a back-off has run out.
fn resumed(until: &mut Option<Instant>) -> bool {
    match until {
        Some(t) if Instant::now() < *t => false,
        _ => {
            *until = None;
            true
        }
    }
}

/// Count one warm. False when it failed, which is the caller's cue to leave that nest alone.
fn record(
    report: &mut WarmReport,
    route: &'static str,
    result: Result<Vec<Read>, ApiError>,
) -> bool {
    match result {
        Ok(reads) => {
            for read in reads {
                let outcome = if read.meta.served_from == ServedFrom::Nest {
                    report.warmed += 1;
                    "refreshed"
                } else {
                    report.kept += 1;
                    "kept"
                };
                metrics::counter!("warmer_total", "route" => route, "outcome" => outcome)
                    .increment(1);
            }
            true
        }
        Err(e) => {
            report.failed += 1;
            metrics::counter!("warmer_total", "route" => route, "outcome" => "error").increment(1);
            tracing::info!(route, error = %e, "warm refused; leaving the nest alone for a while");
            false
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use kittiwake_db::{QosAllocationDay, QosIndexerDay};
    use kittiwake_nest::{NestConfig, NestGate};
    use kittiwake_types::NestId;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const A: &str = "0x00000000000000000000000000000000000000aa";
    const B: &str = "0x00000000000000000000000000000000000000bb";

    /// Answers every statement with no rows and records it, refusing the nests in `busy`.
    struct Nest {
        seen: Arc<Mutex<Vec<String>>>,
        busy: &'static [&'static str],
    }

    impl Respond for Nest {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let path = req.url.path();
            if path.ends_with("/ready") {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ready": true, "lag_blocks": 0, "tip": 1, "last_block": 1,
                    "sealed_through": 1, "stalled": false
                }));
            }
            let q = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.to_string())
                .unwrap_or_default();
            self.seen.lock().unwrap().push(format!("{path} :: {q}"));
            if self
                .busy
                .iter()
                .any(|n| path.starts_with(&format!("/{n}/")))
            {
                return ResponseTemplate::new(503).set_body_string("server busy");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": 0, "rows": [],
                "provenance": {"as_of": 1, "sealed_through": 1, "source": "mock"}
            }))
        }
    }

    async fn ctx(busy: &'static [&'static str]) -> (ReadCtx, Arc<Mutex<Vec<String>>>, MockServer) {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Nest {
                seen: seen.clone(),
                busy,
            })
            .mount(&server)
            .await;
        let mut cfg = NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let ctx = ReadCtx::new(
            NestGate::new(cfg).unwrap(),
            kittiwake_cache::Store::new(1024),
            1,
        );
        (ctx, seen, server)
    }

    fn directory(addresses: &'static [&'static str], asked: Arc<Mutex<u32>>) -> Directory {
        Arc::new(move || {
            *asked.lock().unwrap() += 1;
            Box::pin(async move { Ok(addresses.iter().map(|a| a.to_string()).collect()) })
        })
    }

    struct NoStoredDays;

    impl qos::QosStore for NoStoredDays {
        fn indexer_days<'a>(
            &'a self,
            _: &'a str,
            _: i64,
            _: i64,
        ) -> BoxFuture<'a, Result<Vec<QosIndexerDay>, ApiError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn cohort_days<'a>(
            &'a self,
            _: &'a str,
            _: i64,
            _: i64,
            _: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn indexer_allocation_days<'a>(
            &'a self,
            _: &'a str,
            _: i64,
            _: i64,
        ) -> BoxFuture<'a, Result<Vec<QosAllocationDay>, ApiError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn stored_days(&self, _: i64, _: i64) -> BoxFuture<'_, Result<Vec<i64>, ApiError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Asked the way each handler asks, every warmed read is answered without the nest. An entry
    /// under a key no handler builds would pass every other test and help nobody.
    #[tokio::test]
    async fn after_a_run_the_page_is_answered_without_the_nest() {
        let (ctx, seen, _server) = ctx(&[]).await;
        IndexerPages::new()
            .run_for(&ctx, &directory(&[A], Default::default()), WORK_PER_RUN)
            .await
            .unwrap();
        let warmed = seen.lock().unwrap().len();
        assert!(warmed > 0, "the run asked the nests nothing");

        let now = unix_now();
        let today = now.div_euclid(86_400);
        let since = |days| kittiwake_rewards::pnl::window_start(now, days);
        let store = NoStoredDays;
        let reads = vec![
            ("indexer", routes::indexer(&ctx, A).await),
            ("status", routes::indexer_status(&ctx, A).await),
            ("provisions", routes::provisions_by_indexer(&ctx, A).await),
            (
                "revenue",
                routes::indexer_revenue(&ctx, A, since(30), false).await,
            ),
            (
                "pnl",
                routes::indexer_revenue(&ctx, A, since(30), true).await,
            ),
            (
                "stake history",
                routes::indexer_stake_history(&ctx, A, 182).await,
            ),
            ("trends", routes::indexer_trends(&ctx, A, since(90)).await),
            (
                "qos",
                qos::indexer_qos(&ctx, Some(&store), A, today - 89, today).await,
            ),
            (
                "qos score",
                qos::indexer_qos_score(&ctx, Some(&store), A, today).await,
            ),
            (
                "qos deployments",
                qos::indexer_qos_deployments(&ctx, Some(&store), A, today).await,
            ),
        ];
        for (name, read) in reads {
            assert_eq!(read.unwrap().meta.served_from, ServedFrom::Cache, "{name}");
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            warmed,
            "a reader made a nest work"
        );
    }

    #[tokio::test]
    async fn a_nest_that_refuses_is_left_alone_and_the_other_is_not() {
        let (ctx, seen, _server) = ctx(&["alloc"]).await;
        IndexerPages::new()
            .run_for(&ctx, &directory(&[A, B], Default::default()), WORK_PER_RUN)
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        let asked = |nest: &str, who: &str| {
            seen.iter()
                .any(|s| s.starts_with(&format!("/{nest}/")) && s.contains(who))
        };
        assert!(asked("alloc", A), "the first indexer is tried");
        assert!(
            !asked("alloc", B),
            "after a refusal the nest is not asked again"
        );
        assert!(asked("qos", B), "the QoS nest is still warmed");
    }

    #[tokio::test]
    async fn a_run_stops_when_its_work_is_spent_and_the_next_carries_on() {
        let (ctx, seen, _server) = ctx(&[]).await;
        let asked = Arc::new(Mutex::new(0));
        let dir = directory(&[A, B], asked.clone());
        let pages = IndexerPages::new();
        let naming = |who: &str| {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|s| s.contains(who))
                .count()
        };

        pages.run_for(&ctx, &dir, Duration::ZERO).await.unwrap();
        assert!(naming(A) > 0, "the first run warms the first indexer");
        assert_eq!(naming(B), 0, "and stops there");

        pages.run_for(&ctx, &dir, Duration::ZERO).await.unwrap();
        assert!(naming(B) > 0, "the next run carries on");
        let total = seen.lock().unwrap().len();

        pages.run_for(&ctx, &dir, Duration::ZERO).await.unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            2,
            "a finished walk reads the directory again"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            total,
            "an entry nowhere near expiry is left alone"
        );
    }

    /// The store dropped every entry after an hour whatever its policy, which would have cut the
    /// two-hour QoS window to one and left a cold entry for whoever came next.
    #[test]
    fn every_warm_window_fits_the_store_and_outlasts_the_look_ahead() {
        // The slowest cadence behind a warmed read: stake history and trends.
        let slowest = Duration::from_secs(600);
        for window in [routes::INDEXER_PAGE_STALE_FOR, qos::LIVE_STALE_FOR] {
            let policy =
                kittiwake_cache::CachePolicy::from_poll_interval(slowest).kept_warm(window);
            assert!(policy.hard_ttl <= kittiwake_cache::MAX_AGE, "{window:?}");
            assert!(
                AHEAD < window,
                "every visit would refresh an entry kept for {window:?}"
            );
        }
    }

    /// The list folds are a background job's, so a full cron lane holds them back rather than
    /// letting them take the read permits a waiting reader needs.
    #[tokio::test]
    async fn the_list_folds_wait_for_the_cron_lane() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("q", "slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Nest {
                seen: seen.clone(),
                busy: &[],
            })
            .mount(&server)
            .await;
        let mut cfg = NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, String::new());
        }
        cfg.cron_queue_timeout = Duration::from_millis(200);
        let gate = NestGate::new(cfg).unwrap();
        let ctx = ReadCtx::new(gate.clone(), kittiwake_cache::Store::new(64), 1);

        tokio::spawn(async move {
            gate.query(&kittiwake_nest::QuerySpec::cron(NestId::Alloc, "slow"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let report = warm_all(&ctx).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
        assert_eq!(report.warmed, 0);
    }

    /// A fresh list fold is left alone and a stale one is refreshed, so the warm job keeps the
    /// cadence it had before it moved to the cron lane.
    #[tokio::test]
    async fn a_list_fold_is_refreshed_as_it_goes_stale() {
        let (ctx, seen, _server) = ctx(&[]).await;
        warm_all(&ctx).await;
        let asked = || seen.lock().unwrap().len();

        // network-stats is fresh for a minute.
        let key = kittiwake_cache::key(
            1,
            NestId::Alloc.as_str(),
            "network-stats",
            &serde_json::json!({}),
        );
        let aged = |secs| kittiwake_cache::Entry {
            fetched_at: Instant::now() - Duration::from_secs(secs),
            ..kittiwake_cache::Entry::new(serde_json::json!({}), Default::default())
        };

        ctx.store.insert(key.clone(), aged(50)).await;
        let before = asked();
        warm_all(&ctx).await;
        assert_eq!(asked(), before, "a fresh fold is left alone");

        ctx.store.insert(key, aged(70)).await;
        warm_all(&ctx).await;
        assert!(asked() > before, "a stale fold is refreshed");
    }
}
