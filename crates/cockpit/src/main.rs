use std::sync::Arc;

use anyhow::Context;
use kittiwake_cockpit::agent::Agent;
use kittiwake_cockpit::auth::{Auth, ChainOperators};
use kittiwake_cockpit::config::Config;
use kittiwake_cockpit::{AppState, app};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    kittiwake_obs::init_tracing();

    let path = std::env::var("COCKPIT_CONFIG").unwrap_or_else(|_| "cockpit.toml".into());
    let cfg = Config::load(&path).with_context(|| format!("loading configuration from {path}"))?;

    let agent = Agent::connect(
        &cfg.agent.url,
        cfg.agent.token.clone(),
        cfg.protocol_network.clone(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("agent.url refused: {e}"))?;
    let auth = Auth::new(
        cfg.indexer,
        cfg.allowed_origin.clone(),
        cfg.nonce_ttl_secs,
        cfg.session_ttl_secs,
        cfg.session_key_bytes()?,
        Box::new(ChainOperators(kittiwake_chain::Chain::new(
            cfg.arbitrum_rpc.clone(),
            None,
        ))),
    );
    let state = Arc::new(AppState {
        auth,
        agent,
        allowed_origin: cfg.allowed_origin.clone(),
        secure_cookie: cfg.secure_cookie,
    });

    tracing::info!(bind = %cfg.bind, indexer = %cfg.indexer, origin = %cfg.allowed_origin, "starting");
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
