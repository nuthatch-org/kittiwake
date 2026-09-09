//! Configuration, loaded from a TOML file with a `KITTIWAKE_` environment overlay.
//!
//! Nothing here has a default that reaches the network. An unset nest origin is a startup failure,
//! not a service that silently answers every protocol route with an error for a week.

use std::collections::HashMap;
use std::time::Duration;

use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration: {0}")]
    Load(String),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Clone, Deserialize)]
pub struct NestSettings {
    /// The shared origin every nest sits behind. Required: there is no second source.
    pub origin: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Path overrides per nest, keyed by nest id (`alloc`, `gns`, ...). Only needed when a nest
    /// is staged somewhere other than its usual path.
    #[serde(default)]
    pub base_paths: HashMap<String, String>,
    /// Concurrency caps per nest. Must not exceed what the nest itself admits.
    #[serde(default)]
    pub permits: HashMap<String, usize>,
    #[serde(default = "default_read_queue_secs")]
    pub read_queue_timeout_secs: u64,
    #[serde(default = "default_cron_queue_secs")]
    pub cron_queue_timeout_secs: u64,
}

/// A user waiting three seconds for a *slot* has already had a bad time; beyond that, telling them
/// the nest is busy is kinder than making them wait for a fold that may also be slow.
fn default_read_queue_secs() -> u64 {
    3
}
/// Nothing is waiting on a cron, so it may queue as long as it likes.
fn default_cron_queue_secs() -> u64 {
    120
}

/// Redacted by hand: a config struct ends up in a startup log line sooner or later, and the nest
/// password must not travel with it.
impl std::fmt::Debug for NestSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NestSettings")
            .field("origin", &self.origin)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("base_paths", &self.base_paths)
            .field("permits", &self.permits)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheSettings {
    /// Number of entries, not bytes. The folds are rows rather than blobs.
    #[serde(default = "default_cache_capacity")]
    pub max_entries: u64,
    /// Bumped when a response shape changes, which retires every key at once.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

fn default_cache_capacity() -> u64 {
    8192
}
fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitSettings {
    #[serde(default = "default_sql_per_min")]
    pub sql_per_minute: u32,
    #[serde(default = "default_read_per_min")]
    pub read_per_minute: u32,
    #[serde(default = "default_mutation_per_min")]
    pub mutation_per_minute: u32,
    /// Callers exempt from the public budgets: Foghorn's hourly read, and our own probes. Their
    /// reads still go through the cache, so they add close to zero nest load.
    #[serde(default)]
    pub exempt_ips: Vec<String>,
}

fn default_sql_per_min() -> u32 {
    10
}
fn default_read_per_min() -> u32 {
    120
}
fn default_mutation_per_min() -> u32 {
    30
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    pub nest: NestSettings,
    /// Absent means the Postgres-backed routes report unavailable rather than pretending.
    pub database_url: Option<String>,
    /// Arbitrum One JSON-RPC. Required for the oracle and supply reads; the routes that need it
    /// report unavailable without it rather than reaching for a public endpoint that would
    /// throttle under the directory's fan-out.
    pub arbitrum_rpc_url: Option<String>,
    /// Ethereum mainnet JSON-RPC. Optional: three public endpoints are tried in order when unset,
    /// which is acceptable here because ENS is not a protocol figure.
    pub mainnet_rpc_url: Option<String>,
    /// The Dock's BountyBoard, on Arbitrum. Absent means `reconcile-bounties` does not run, which
    /// is right: with no board there is nothing to reconcile against, and guessing an address
    /// would be worse than not looking. Deployed per environment, unlike the protocol contracts,
    /// which is why it is configuration and not a constant.
    pub bounty_board_address: Option<String>,
    #[serde(default = "default_db_max_conns")]
    pub database_max_connections: u32,
    #[serde(default)]
    pub cache: Option<CacheSettings>,
    #[serde(default)]
    pub limits: Option<LimitSettings>,
    /// The secret the edge proxy presents so its `X-Forwarded-For` can be believed.
    ///
    /// Absent is safe and, once traffic arrives through a rewrite, wrong: every visitor then looks
    /// like the edge and shares one rate-limit bucket. Set it here and in the proxy that fronts
    /// this service, and nowhere else.
    pub edge_secret: Option<String>,
    /// Turns the in-process scheduler on. Off by default so a second instance, or a staging box,
    /// cannot start writing to the same Postgres behind your back.
    #[serde(default)]
    pub scheduler_enabled: bool,
    /// Turns the warmer on. Off by default for the same reason.
    #[serde(default)]
    pub warmer_enabled: bool,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_db_max_conns() -> u32 {
    16
}

impl Config {
    /// Load from `path` if it exists, then overlay `KITTIWAKE_*` environment variables.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let cfg: Config = figment::Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("KITTIWAKE_").split("__"))
            .extract()
            .map_err(|e| ConfigError::Load(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.nest.origin.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "nest.origin is required: every protocol route reads a nest and there is no fallback".into(),
            ));
        }
        if !self.nest.origin.starts_with("http") {
            return Err(ConfigError::Invalid(format!(
                "nest.origin must be a URL, got {:?}",
                self.nest.origin
            )));
        }
        Ok(())
    }

    pub fn cache(&self) -> CacheSettings {
        self.cache.clone().unwrap_or(CacheSettings {
            max_entries: default_cache_capacity(),
            schema_version: default_schema_version(),
        })
    }

    pub fn limits(&self) -> LimitSettings {
        self.limits.clone().unwrap_or(LimitSettings {
            sql_per_minute: default_sql_per_min(),
            read_per_minute: default_read_per_min(),
            mutation_per_minute: default_mutation_per_min(),
            exempt_ips: Vec::new(),
        })
    }

    pub fn read_queue_timeout(&self) -> Duration {
        Duration::from_secs(self.nest.read_queue_timeout_secs)
    }

    pub fn cron_queue_timeout(&self) -> Duration {
        Duration::from_secs(self.nest.cron_queue_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(origin: &str) -> Config {
        Config {
            bind: default_bind(),
            nest: NestSettings {
                origin: origin.into(),
                user: None,
                password: Some("hunter2".into()),
                base_paths: HashMap::new(),
                permits: HashMap::new(),
                read_queue_timeout_secs: 3,
                cron_queue_timeout_secs: 120,
            },
            database_url: None,
            edge_secret: None,
            arbitrum_rpc_url: None,
            mainnet_rpc_url: None,
            bounty_board_address: None,
            database_max_connections: 16,
            cache: None,
            limits: None,
            scheduler_enabled: false,
            warmer_enabled: false,
        }
    }

    #[test]
    fn a_missing_nest_origin_is_a_startup_failure() {
        assert!(settings("").validate().is_err());
    }

    #[test]
    fn the_nest_password_never_reaches_a_log_line() {
        let printed = format!("{:?}", settings("https://nest.example").nest);
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }
}
