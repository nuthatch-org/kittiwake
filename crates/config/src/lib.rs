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

/// Which browser origins may call this API directly.
///
/// Absent means no CORS headers are sent at all, which is what this service did for its whole life
/// and is correct while every browser request arrives through the Vercel edge on the dashboard's
/// own origin. It becomes necessary the moment `src/proxy.ts` is deleted and the browser talks to
/// `api.lodestar-dashboard.com` itself.
///
/// Listed exactly rather than by wildcard. `Access-Control-Allow-Origin: *` cannot carry
/// credentials, and the Dock's session is a cookie, so a wildcard would silently sign every Dock
/// user out on a cross-origin call while every unauthenticated route kept working.
#[derive(Clone, Debug, Deserialize)]
pub struct CorsSettings {
    pub allowed_origins: Vec<String>,
}

/// The product surface is deliberately opt-in.  A process that has the read-side configuration
/// but not the session key must not accidentally start accepting half-configured Dock requests.
#[derive(Clone, Deserialize)]
pub struct StudioSettings {
    pub session_secret: String,
    #[serde(default = "default_ipfs_url")]
    pub ipfs_url: String,
}

impl std::fmt::Debug for StudioSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StudioSettings")
            .field("session_secret", &"<redacted>")
            .field("ipfs_url", &self.ipfs_url)
            .finish()
    }
}

fn default_ipfs_url() -> String {
    "https://api.thegraph.com/ipfs".to_string()
}

/// The indexer-agent management API `/api/indexer/present-poi` queues actions against.
///
/// The token belongs to this URL and travels only here. A caller may nominate their own agent, and
/// when they do they bring their own credential: see the handler for why.
#[derive(Clone, Deserialize)]
pub struct IndexerAgentSettings {
    pub url: String,
    /// `user:password` for Basic auth, if the endpoint wants it.
    pub token: Option<String>,
}

impl std::fmt::Debug for IndexerAgentSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexerAgentSettings")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The key that signs SQL receipts. Absent means this deployment issues none, which is a valid
/// posture rather than a misconfiguration: a reader can still sign an answer themselves with
/// `tattler attest-named` and their own key. #19
#[derive(Clone, Deserialize)]
pub struct ReceiptSettings {
    /// A 32-byte ed25519 seed, hex. The same key across restarts, or every receipt already issued
    /// is signed by a public key nobody can look up any more.
    pub issuer_key: String,
}

impl std::fmt::Debug for ReceiptSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptSettings")
            .field("issuer_key", &"<redacted>")
            .finish()
    }
}

/// The credential that opens an issue in `nightswatchhq/graph-support`.
///
/// Absent means this deployment reads the forms and refuses to file, which is what it does today:
/// the token has never been minted, so the incumbent's filing route has answered "not configured"
/// to every request it has ever had. The compose page is told so through `canFile` and sends people
/// to GitHub's own chooser rather than accepting typing it cannot deliver.
#[derive(Clone, Deserialize)]
pub struct SupportSettings {
    /// A fine-grained token with `issues:write` on that repository and nothing else.
    pub issue_token: String,
}

impl std::fmt::Debug for SupportSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupportSettings")
            .field("issue_token", &"<redacted>")
            .finish()
    }
}

/// The key that pays for a serving probe. Absent means probes go out unpaid, which is a weaker
/// question rather than a broken one: an indexer that wants a receipt answers "pay me", and the
/// probe reports that it could not tell whether the deployment serves.
#[derive(Clone, Deserialize)]
pub struct TapSettings {
    /// A 32-byte secp256k1 private key, hex, with or without `0x`. It must be the address that
    /// holds the escrow balance: an indexer that verifies looks the payer up, and a key it has
    /// never seen has no balance no matter how well formed the receipt is.
    pub signer_private_key: String,
}

impl std::fmt::Debug for TapSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapSettings")
            .field("signer_private_key", &"<redacted>")
            .finish()
    }
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
    #[serde(default)]
    pub studio: Option<StudioSettings>,
    #[serde(default)]
    pub cors: Option<CorsSettings>,
    /// Foghorn's API, which `/api/foghorn/{*path}` proxies. Absent means that route answers 503
    /// rather than guessing at a host.
    pub foghorn_api_url: Option<String>,
    #[serde(default)]
    pub indexer_agent: Option<IndexerAgentSettings>,
    #[serde(default)]
    pub receipt: Option<ReceiptSettings>,
    pub tap: Option<TapSettings>,
    pub support: Option<SupportSettings>,
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
        if let Some(receipt) = &self.receipt {
            // Checked here so a deployment that means to issue receipts finds out at boot rather
            // than the first time somebody asks for one.
            if hex::decode(&receipt.issuer_key).map(|b| b.len()) != Ok(32) {
                return Err(ConfigError::Invalid(
                    "receipt.issuer_key must be 32 bytes of hexadecimal".into(),
                ));
            }
        }
        if let Some(tap) = &self.tap {
            // Checked at boot rather than at the first probe. A malformed key would otherwise
            // surface as every indexer in a round reporting that it wants to be paid, which reads
            // as a network-wide fault rather than as our own configuration.
            let key = tap.signer_private_key.trim_start_matches("0x");
            if hex::decode(key).map(|b| b.len()) != Ok(32) {
                return Err(ConfigError::Invalid(
                    "tap.signer_private_key must be 32 bytes of hexadecimal".into(),
                ));
            }
        }
        if let Some(cors) = &self.cors {
            if cors.allowed_origins.is_empty() {
                return Err(ConfigError::Invalid(
                    "cors.allowed_origins is present but empty; remove the section instead, or a \
                     reader will think browsers are allowed when none are"
                        .into(),
                ));
            }
            for o in &cors.allowed_origins {
                // An origin is scheme://host[:port] and nothing else. A trailing slash or a path
                // makes a header no browser will ever match, and the failure is a CORS error in
                // somebody's console rather than anything this process notices.
                if o == "*" {
                    return Err(ConfigError::Invalid(
                        "cors.allowed_origins may not be \"*\": a wildcard cannot carry the Dock's \
                         session cookie, so it would sign users out rather than let them in"
                            .into(),
                    ));
                }
                if !o.starts_with("https://") && !o.starts_with("http://localhost") {
                    return Err(ConfigError::Invalid(format!(
                        "cors.allowed_origins entry {o:?} is not an https origin"
                    )));
                }
                if o.ends_with('/') || o[8..].contains('/') {
                    return Err(ConfigError::Invalid(format!(
                        "cors.allowed_origins entry {o:?} has a path or trailing slash; an origin \
                         is scheme://host[:port] and a browser will never match this"
                    )));
                }
            }
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

    fn with_cors(origins: &[&str]) -> Config {
        let mut c = settings("https://nest.example");
        c.cors = Some(CorsSettings {
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
        });
        c
    }

    #[test]
    fn the_dashboard_origins_are_accepted() {
        assert!(
            with_cors(&[
                "https://www.lodestar-dashboard.com",
                "https://lodestar-dashboard.com",
            ])
            .validate()
            .is_ok()
        );
    }

    /// A wildcard cannot carry credentials, and the Dock's session is a cookie. Allowing it would
    /// leave every unauthenticated route working and sign out anyone using the Dock, which is a
    /// failure nobody would attribute to a CORS header.
    #[test]
    fn a_wildcard_origin_is_refused_because_of_the_cookie() {
        let err = with_cors(&["*"]).validate().unwrap_err().to_string();
        assert!(err.contains("session cookie"), "{err}");
    }

    /// An origin is scheme://host[:port]. A browser will never match one with a path on it, and
    /// the failure surfaces in somebody's console rather than here.
    #[test]
    fn an_origin_with_a_path_is_refused() {
        assert!(
            with_cors(&["https://www.lodestar-dashboard.com/"])
                .validate()
                .is_err()
        );
        assert!(
            with_cors(&["https://www.lodestar-dashboard.com/api"])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn a_plaintext_origin_is_refused_unless_it_is_localhost() {
        assert!(with_cors(&["http://evil.example"]).validate().is_err());
        assert!(with_cors(&["http://localhost:3000"]).validate().is_ok());
    }

    /// Present and empty is worse than absent: it reads as "browsers are configured" while
    /// allowing none of them.
    #[test]
    fn an_empty_list_is_refused_rather_than_treated_as_absent() {
        assert!(with_cors(&[]).validate().is_err());
    }

    #[test]
    fn no_cors_section_is_fine_and_is_what_ships_today() {
        assert!(settings("https://nest.example").validate().is_ok());
    }

    fn settings(origin: &str) -> Config {
        Config {
            bind: default_bind(),
            cors: None,
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
            studio: None,
            foghorn_api_url: None,
            indexer_agent: None,
            receipt: None,
            tap: None,
            support: None,
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

#[cfg(test)]
mod deploy_safety {
    /// A config that predates every optional section still parses.
    ///
    /// The box's `/etc/kittiwake/config.toml` is written once and never overwritten, so a section
    /// added here that is not genuinely optional stops the service starting on the next deploy,
    /// after the binary has already been swapped.
    #[test]
    fn a_config_without_any_of_the_newer_sections_parses() {
        let minimal = r#"
bind = "127.0.0.1:8080"
scheduler_enabled = true
warmer_enabled = true

[nest]
origin = "https://nest.example"
username = "u"
password = "p"
"#;
        let dir = std::env::temp_dir().join(format!("kittiwake-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, minimal).unwrap();
        let parsed = super::Config::load(path.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();
        assert!(parsed.is_ok(), "{:?}", parsed.err());
    }
}
