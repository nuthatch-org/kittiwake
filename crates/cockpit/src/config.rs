//! Configuration, from a TOML file with a `COCKPIT_` environment overlay.

use std::net::SocketAddr;

use alloy::primitives::Address;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct AgentSettings {
    /// The indexer-agent management API, e.g. `http://indexer-agent:18000/`.
    pub url: String,
    /// Sent as a bearer token when the agent sits behind an authenticating proxy.
    pub token: Option<String>,
}

impl std::fmt::Debug for AgentSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSettings")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// The one indexer this Cockpit manages. A signer must be it or one of its operators.
    pub indexer: Address,
    /// The Lodestar build's origin, e.g. `https://lodestar.example.org`. Only it may call us.
    pub allowed_origin: String,
    pub arbitrum_rpc: String,
    /// CAIP-2, as the agent stores it.
    #[serde(default = "default_network")]
    pub protocol_network: String,
    #[serde(default = "default_session_ttl")]
    pub session_ttl_secs: u64,
    #[serde(default = "default_nonce_ttl")]
    pub nonce_ttl_secs: u64,
    /// Hex, 32 bytes. Unset means a fresh key per start, which signs everyone out on restart.
    pub session_key: Option<String>,
    /// Off only for a plain-http origin other than localhost.
    #[serde(default = "default_true")]
    pub secure_cookie: bool,
    pub agent: AgentSettings,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("indexer", &self.indexer)
            .field("allowed_origin", &self.allowed_origin)
            .field("protocol_network", &self.protocol_network)
            .field("session_ttl_secs", &self.session_ttl_secs)
            .field("nonce_ttl_secs", &self.nonce_ttl_secs)
            .field(
                "session_key",
                &self.session_key.as_ref().map(|_| "<redacted>"),
            )
            .field("secure_cookie", &self.secure_cookie)
            .field("agent", &self.agent)
            .finish()
    }
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:8088".parse().expect("constant address")
}
fn default_network() -> String {
    "eip155:42161".into()
}
fn default_session_ttl() -> u64 {
    3600
}
fn default_nonce_ttl() -> u64 {
    300
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let cfg: Config = figment::Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("COCKPIT_").split("__"))
            .extract()?;
        cfg.session_key_bytes()?;
        if cfg.allowed_origin.ends_with('/') || !cfg.allowed_origin.starts_with("http") {
            anyhow::bail!(
                "allowed_origin must be a bare origin such as https://lodestar.example.org"
            );
        }
        Ok(cfg)
    }

    pub fn session_key_bytes(&self) -> anyhow::Result<[u8; 32]> {
        let mut key = [0u8; 32];
        match &self.session_key {
            Some(h) => {
                let bytes = hex::decode(h.trim_start_matches("0x"))?;
                if bytes.len() != 32 {
                    anyhow::bail!("session_key must be 32 bytes of hex");
                }
                key.copy_from_slice(&bytes);
            }
            None => getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("randomness: {e}"))?,
        }
        Ok(key)
    }
}
