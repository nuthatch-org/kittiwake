//! kittiwake: Lodestar's backend.
//!
//! One process, on purpose. The admission gate in front of the nests is only the truth if there is
//! exactly one of it, and the previous service's per-process gate was meaningless precisely
//! because the platform ran many copies. Splitting the API from the scheduler would reintroduce
//! that fault, so both live here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

mod backup_watch;
mod envfile;
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

/// `kittiwake --check-config`: load and validate the configuration exactly as startup does, say what
/// it would refuse or ignore, and exit without binding or connecting to anything.
fn check_config(env_file: Option<&str>) -> anyhow::Result<()> {
    if let Some(file) = env_file {
        return check_config_with_env_file(file);
    }
    let config_path =
        std::env::var("KITTIWAKE_CONFIG").unwrap_or_else(|_| "/etc/kittiwake/config.toml".into());
    let (cfg, warnings) = Config::load_with_warnings(&config_path)
        .with_context(|| format!("refused: configuration from {config_path}"))?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    println!("configuration ok: {config_path} (nest {})", cfg.nest.origin);
    Ok(())
}

/// The check as a child of this binary with the file's variables added, so the load is the one
/// startup does and this process's own environment is never changed.
fn check_config_with_env_file(file: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("could not read {file}"))?;
    let parsed = envfile::parse(&text);
    for w in &parsed.warnings {
        eprintln!("warning: {file}: {w}");
    }
    let status = std::process::Command::new(std::env::current_exe()?)
        .arg("--check-config")
        .envs(parsed.vars)
        .status()
        .context("could not run the check")?;
    std::process::exit(status.code().unwrap_or(1));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--check-config") {
        let env_file = match args.get(2).map(String::as_str) {
            Some("--env-file") => Some(
                args.get(3)
                    .map(String::as_str)
                    .context("--env-file needs a path")?,
            ),
            _ => None,
        };
        return check_config(env_file);
    }
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

            // Said once, loudly, at the moment a deploy is being watched. On 2026-09-10 a binary
            // needing `ingestion_state.last_timestamp` ran for six and a half hours against a
            // database without it, and the only symptom was five ingest jobs quietly failing every
            // run. This is knowable here, in one query, before any of them fire.
            //
            // It does not refuse to start. The read API serves the dashboard from the nests and is
            // unaffected by a gap in a table only the ingest side writes, and taking the whole
            // service down for that would turn a partial outage into a total one.
            match kittiwake_db::schema_gaps(&pool).await {
                Ok(gaps) if !gaps.is_empty() => {
                    for gap in &gaps {
                        use kittiwake_db::GapKind;
                        match (&gap.column, gap.kind) {
                            (Some(column), _) => tracing::error!(
                                table = %gap.table,
                                column = %column,
                                "the database is missing a column this build declares"
                            ),
                            (None, GapKind::Missing) => tracing::error!(
                                table = %gap.table,
                                "the database is missing a table this build declares"
                            ),
                            // Present, and invisible to this role. A GRANT is owed and no schema
                            // change is, so saying "missing" would send somebody to write one.
                            (None, GapKind::NoPrivilege) => tracing::error!(
                                table = %gap.table,
                                "this role has no privilege on a table this build declares; see deploy/grant-database.sh"
                            ),
                        }
                    }
                    tracing::error!(
                        count = gaps.len(),
                        "schema gaps: anything touching these fails every run. /health reports them"
                    );
                }
                Ok(_) => tracing::info!("schema matches this build"),
                Err(e) => tracing::warn!(error = %e, "the schema could not be checked"),
            }

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
            if cfg.mainnet_rpc_fallback_url.is_some() {
                tracing::info!("mainnet fallback rpc configured");
            }
            Some(
                kittiwake_chain::Chain::new(url, cfg.mainnet_rpc_url.clone())
                    .with_mainnet_fallback(cfg.mainnet_rpc_fallback_url.clone()),
            )
        }
        None => {
            tracing::warn!(
                "no arbitrum_rpc_url: the oracle and supply routes will report unavailable"
            );
            None
        }
    };

    let ipfs = kittiwake_ext::Ipfs::new(
        cfg.studio
            .as_ref()
            .map(|s| s.ipfs_url.as_str())
            .unwrap_or(kittiwake_ext::THEGRAPH_IPFS),
    );

    // One read context for the whole process. Building a second one for the warmer meant it cached
    // answers computed without the chain client, and because it kept them warm the edge never took
    // a cold path that would have filled them in - a supply block that was null for ever, silently.
    let read = ReadCtx::new(gate.clone(), store.clone(), cache_cfg.schema_version)
        .with_chain(chain.clone())
        .with_naming(
            pool.clone()
                .map(|p| kittiwake_read::naming::Naming::new(p, ipfs.clone())),
        );

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
                    spent_nonces: Default::default(),
                })
                .map_err(|e| anyhow::anyhow!("invalid studio session secret: {e}"))
        })
        .transpose()?;
    let state = kittiwake_edge::state(
        limits,
        pool.clone(),
        read.clone(),
        cfg.edge_secret.clone(),
        studio,
        ipfs,
        cfg.foghorn_api_url.clone(),
        cfg.indexer_agent.clone(),
        cfg.receipt.as_ref().map(|r| {
            let bytes: [u8; 32] = hex::decode(&r.issuer_key)
                .ok()
                .and_then(|b| b.try_into().ok())
                .expect("receipt.issuer_key was accepted by configuration");
            Arc::new(ed25519_dalek::SigningKey::from_bytes(&bytes))
        }),
        cfg.tap.as_ref().map(|t| {
            Arc::new(
                kittiwake_chain::tap::Payer::from_hex(&t.signer_private_key)
                    .expect("tap.signer_private_key was accepted by configuration"),
            )
        }),
    )
    .with_cors(
        cfg.cors.as_ref().map(|c| c.allowed_origins.clone()),
        cfg.cors.as_ref().map(|c| c.allowed_origin_patterns.clone()),
    )
    .with_telegram(cfg.telegram.clone())
    .with_network_subgraph(cfg.network_subgraph.clone())
    .with_substreams_mainnet(cfg.substreams_mainnet.as_ref())
    .with_github_token(cfg.github_token.clone());
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
            kittiwake_db::ensure_qos_quality_table(p)
                .await
                .context("creating the QoS quality table")?;
            kittiwake_db::ensure_score_breakdown_table(p)
                .await
                .context("creating the score breakdown table")?;
            kittiwake_db::ensure_qos_day_tables(p)
                .await
                .context("creating the QoS day tables")?;
            kittiwake_db::ensure_graph_support_table(p)
                .await
                .context("creating the graph-support mirror table")?;
            kittiwake_db::ensure_studio_sign_outs_table(p)
                .await
                .context("creating the Dock sign-out table")?;
        }
        if let Some(path) = &cfg.backup_mark_path {
            jobs.push(backup_watch::job(path.clone(), cfg.telegram.clone()));
        }
        jobs.extend(jobs::live_jobs(
            store.clone(),
            pool.clone(),
            kittiwake_ext::Ext::default().with_github_token(cfg.github_token.clone()),
            cfg.cron_runs_retention_days,
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

            // Every directory indexer's page, a few indexers a minute. The directory is in
            // Postgres, so without one only the list folds are warmed.
            if let Some(p) = &pool {
                let pool = p.clone();
                let directory: kittiwake_warmer::Directory = Arc::new(move || {
                    let pool = pool.clone();
                    Box::pin(async move {
                        kittiwake_db::enriched_indexers(&pool, kittiwake_warmer::DIRECTORY_SIZE)
                            .await
                            .map(|rows| {
                                rows.into_iter().map(|r| r.address.to_lowercase()).collect()
                            })
                            .map_err(|e| e.to_string())
                    })
                });
                let pages = Arc::new(kittiwake_warmer::IndexerPages::new());
                let warm_ctx = read.clone();
                let mut job = JobDef::every(
                    "warm-indexers",
                    Duration::from_secs(60),
                    Arc::new(move |_ctx| {
                        let ctx = warm_ctx.clone();
                        let pages = pages.clone();
                        let directory = directory.clone();
                        Box::pin(async move {
                            pages.run(&ctx, &directory).await.map(|report| JobOutcome {
                                rows_written: report.warmed,
                            })
                        })
                    }),
                )
                .staggered(Duration::from_secs(30));
                // A run works up to thirty seconds past its last indexer and rests as long again;
                // a slow indexer on a slow nest can take several minutes of that.
                job.timeout = Duration::from_secs(600);
                jobs.push(job);
            }
        }

        if cfg.scheduler_enabled {
            match &pool {
                Some(p) => {
                    let ingest = jobs::ingest_jobs(
                        p.clone(),
                        kittiwake_ext::Ext::default().with_github_token(cfg.github_token.clone()),
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

    if let Some(handle) = metrics {
        let addr: SocketAddr = cfg
            .metrics_bind
            .parse()
            .with_context(|| format!("metrics bind address {}", cfg.metrics_bind))?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        tracing::info!(%addr, "serving metrics");
        handles.push(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, kittiwake_edge::metrics_router(handle)).await {
                tracing::error!(error = %e, "metrics listener stopped");
            }
        }));
    }

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
