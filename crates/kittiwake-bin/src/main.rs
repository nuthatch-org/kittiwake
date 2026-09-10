//! kittiwake: Lodestar's backend.
//!
//! One process, on purpose. The admission gate in front of the nests is only the truth if there is
//! exactly one of it, and the previous service's per-process gate was meaningless precisely
//! because the platform ran many copies. Splitting the API from the scheduler would reintroduce
//! that fault, so both live here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

mod jobs;

use anyhow::Context;
use futures::future::BoxFuture;
use kittiwake_cache::Store;
use kittiwake_config::Config;
use kittiwake_nest::{NestConfig, NestGate};
use kittiwake_read::ReadCtx;
use kittiwake_sched::{JobDef, JobOutcome, JobRun, RunRecorder, Scheduler};
use kittiwake_types::NestId;

/// Writes each run to `cron_runs`, which is where a failure that predates the current deployment
/// can still be read. Platform logs only ever show the live deployment's window.
struct PgRecorder(kittiwake_db::Pool);

impl RunRecorder for PgRecorder {
    fn record(&self, run: JobRun) -> BoxFuture<'static, ()> {
        let pool = self.0.clone();
        Box::pin(async move {
            if let Err(e) = kittiwake_db::record_cron_run(
                &pool,
                run.job,
                run.started_at,
                run.duration,
                run.success,
                run.rows_written as i64,
                run.error.as_deref(),
            )
            .await
            {
                tracing::warn!(job = run.job, error = %e, "could not record the cron run");
            }
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    kittiwake_obs::init_tracing();
    let metrics = kittiwake_obs::init_metrics().ok();

    let config_path =
        std::env::var("KITTIWAKE_CONFIG").unwrap_or_else(|_| "/etc/kittiwake/config.toml".into());
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading configuration from {config_path}"))?;

    tracing::info!(config = %config_path, nest = %cfg.nest.origin, bind = %cfg.bind, "starting");

    // The gate. Every read, warm and cron passes through it, and its permits are sized to what
    // each nest itself admits.
    let mut nest_cfg = NestConfig::new(cfg.nest.origin.clone())
        .with_auth(cfg.nest.user.clone(), cfg.nest.password.clone());
    nest_cfg.read_queue_timeout = cfg.read_queue_timeout();
    nest_cfg.cron_queue_timeout = cfg.cron_queue_timeout();
    for nest in NestId::ALL {
        if let Some(p) = cfg.nest.base_paths.get(nest.as_str()) {
            nest_cfg.base_paths.insert(nest, p.clone());
        }
        if let Some(n) = cfg.nest.permits.get(nest.as_str()) {
            nest_cfg.permits.insert(nest, *n);
        }
    }
    let gate = NestGate::new(nest_cfg).map_err(|e| anyhow::anyhow!("building nest client: {e}"))?;

    let cache_cfg = cfg.cache();
    let store = Store::new(cache_cfg.max_entries);

    let pool = match cfg.database_url.as_deref() {
        Some(url) => {
            let pool = kittiwake_db::connect(url, cfg.database_max_connections)
                .await
                .context("connecting to Postgres")?;
            kittiwake_db::ping(&pool)
                .await
                .context("first query against Postgres")?;
            tracing::info!(
                max_connections = cfg.database_max_connections,
                "postgres ready"
            );
            Some(pool)
        }
        None => {
            tracing::warn!("no database_url: the Postgres-backed routes will report unavailable");
            None
        }
    };

    let limit_cfg = cfg.limits();
    let exempt = limit_cfg
        .exempt_ips
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect::<Vec<_>>();
    let limits = kittiwake_edge::Limits::new(
        limit_cfg.sql_per_minute,
        limit_cfg.read_per_minute,
        limit_cfg.mutation_per_minute,
        exempt,
    );

    let chain = match cfg.arbitrum_rpc_url.as_deref() {
        Some(url) => {
            tracing::info!("arbitrum rpc configured");
            Some(kittiwake_chain::Chain::new(
                url,
                cfg.mainnet_rpc_url.clone(),
            ))
        }
        None => {
            tracing::warn!(
                "no arbitrum_rpc_url: the oracle and supply routes will report unavailable"
            );
            None
        }
    };

    // One read context for the whole process. Building a second one for the warmer meant it cached
    // answers computed without the chain client, and because it kept them warm the edge never took
    // a cold path that would have filled them in - a supply block that was null for ever, silently.
    let read = ReadCtx::new(gate.clone(), store.clone(), cache_cfg.schema_version)
        .with_chain(chain.clone());

    // Said once, loudly, rather than discovered when the rate limiter starts refusing everyone:
    // without this the service cannot tell one visitor behind a proxy from another.
    if cfg.edge_secret.is_none() {
        tracing::warn!(
            "no edge_secret: every request arriving through a proxy is attributed to the proxy's \
             own address, so all callers share one rate-limit bucket. Set it before any rewrite \
             sends real traffic here."
        );
    }
    let studio = cfg
        .studio
        .as_ref()
        .map(|s| {
            kittiwake_dock::Secret::new(&s.session_secret)
                .map(|secret| kittiwake_edge::Studio {
                    secret,
                    bounty_board_address: cfg.bounty_board_address.clone(),
                })
                .map_err(|e| anyhow::anyhow!("invalid studio session secret: {e}"))
        })
        .transpose()?;
    let ipfs = kittiwake_ext::Ipfs::new(
        cfg.studio
            .as_ref()
            .map(|s| s.ipfs_url.as_str())
            .unwrap_or(kittiwake_ext::THEGRAPH_IPFS),
    );
    let state = kittiwake_edge::state(
        limits,
        metrics,
        pool.clone(),
        read.clone(),
        cfg.edge_secret.clone(),
        studio,
        ipfs,
    );
    let limits_handle = state.limits.clone();

    // Keyed rate-limit state grows with distinct callers; sweep the idle ones periodically so a
    // long uptime against a wide address range cannot grow it without bound.
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            limits_handle.sweep();
        }
    });

    let mut handles = Vec::new();

    {
        let recorder: Arc<dyn RunRecorder> = match &pool {
            Some(p) => Arc::new(PgRecorder(p.clone())),
            None => Arc::new(kittiwake_sched::NoopRecorder),
        };
        let mut scheduler = Scheduler::new(gate.clone(), recorder);
        let mut jobs: Vec<JobDef> = Vec::new();

        // Always: these write nowhere the platform's own crons also write. Most of them touch
        // only this process's memory; chain health additionally keeps its history in two tables
        // of its own, created here because they are the one part of the schema that is ours.
        if let Some(p) = &pool {
            kittiwake_db::ensure_chain_health_tables(p)
                .await
                .context("creating the chain-health tables")?;
            kittiwake_db::ensure_rolling_apy_table(p)
                .await
                .context("creating the rolling-APY table")?;
            kittiwake_db::ensure_graph_support_table(p)
                .await
                .context("creating the graph-support mirror table")?;
        }
        jobs.extend(jobs::live_jobs(
            store.clone(),
            pool.clone(),
            kittiwake_ext::Ext::default(),
        ));

        if cfg.warmer_enabled {
            let warm_ctx = read.clone();
            jobs.push(JobDef::every(
                "warm",
                Duration::from_secs(60),
                Arc::new(move |_ctx| {
                    let ctx = warm_ctx.clone();
                    Box::pin(async move {
                        let report = kittiwake_warmer::warm_all(&ctx).await;
                        Ok(JobOutcome {
                            rows_written: report.warmed,
                        })
                    })
                }),
            ));
        }

        if cfg.scheduler_enabled {
            match &pool {
                Some(p) => {
                    let ingest = jobs::ingest_jobs(
                        p.clone(),
                        kittiwake_ext::Ext::default(),
                        chain.clone(),
                        cfg.bounty_board_address.clone(),
                    );
                    tracing::info!(count = ingest.len(), "ingest jobs scheduled");
                    jobs.extend(ingest);
                }
                None => tracing::warn!(
                    "scheduler_enabled with no database_url: the ingest jobs have nowhere to \
                     write, so none were scheduled"
                ),
            }
        }

        handles.extend(scheduler.spawn_all(jobs));
        std::mem::forget(scheduler);
    }

    let app = kittiwake_edge::router(state);
    let addr: SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("bind address {}", cfg.bind))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .context("serving")?;

    for h in handles {
        h.abort();
    }
    tracing::info!("stopped");
    Ok(())
}

/// SIGTERM as well as ctrl-c: systemd sends the former, and a unit that only handles the latter
/// gets killed rather than drained.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining");
}
