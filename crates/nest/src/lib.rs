//! The only code in the service that opens a socket to a nest.
//!
//! Everything a nest needs protecting from lives here: a global admission gate sized to the nest's
//! own permit count, a lane that keeps crons from ever taking the last permit, request coalescing
//! so N identical cold callers cost one fold, and a short memory of failure so a broken nest is
//! not asked again a hundred times a second.
//!
//! The Node service had a one-slot gate per process and Vercel ran many processes, so the only
//! thing actually bounding concurrency was the nest refusing work. This is a single process by
//! deliberate choice (see the deployment notes), which is what makes the semaphore below the
//! truth rather than a suggestion.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use kittiwake_types::{ApiError, NestId, Provenance};
use tokio::sync::{Mutex, Semaphore};

/// Which lane a query is admitted through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// A user is waiting. May take any permit.
    Read,
    /// A cron or the warmer. Bounded below the nest's permit count so a user read can always get in.
    Cron,
}

/// Everything that can go wrong between us and a nest.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum NestError {
    /// The nest refused: it is already at its concurrent-query cap.
    #[error("nest is at its concurrency cap")]
    Busy,
    /// We refused, to avoid making that worse.
    #[error("no permit available within the queue timeout")]
    GateTimeout,
    /// The nest is running but is not following the chain.
    #[error("nest not ready: {reason}")]
    Unready {
        reason: String,
        lag_blocks: Option<i64>,
        sealed_through: Option<i64>,
    },
    #[error("nest did not answer within the timeout")]
    Timeout,
    #[error("nest returned HTTP {status}: {reason}")]
    Http { status: u16, reason: String },
    #[error("could not decode the nest's answer: {0}")]
    Decode(String),
    #[error("could not reach the nest: {0}")]
    Transport(String),
}

impl From<NestError> for ApiError {
    /// The one place a nest failure becomes something a browser sees. Note what is absent: there
    /// is no arm that reaches for another source. A nest that cannot answer produces an error the
    /// caller can render, with the nest's own reason attached where it gave one.
    fn from(e: NestError) -> Self {
        match e {
            NestError::Busy | NestError::GateTimeout => ApiError::NestBusy { retry_after: 2 },
            NestError::Unready {
                reason,
                lag_blocks,
                sealed_through,
            } => ApiError::NestUnready {
                reason,
                lag_blocks,
                sealed_through,
            },
            NestError::Timeout => ApiError::NestTimeout,
            NestError::Http { status, reason } => ApiError::NestUpstream { status, reason },
            NestError::Decode(m) => ApiError::NestDecode(m),
            NestError::Transport(m) => ApiError::Unavailable(m),
        }
    }
}

/// The nest's `{"error": …}` body reduced to the sentence a caller can act on. Anything that is not
/// that shape is passed through, trimmed: the point is that the reason reaches the alert at all.
fn nest_reason(body: &str) -> String {
    let text = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned());
    let first = text.lines().next().unwrap_or_default();
    first.chars().take(240).collect()
}

/// The `/sql` envelope, as the nest actually sends it.
#[derive(Debug, Clone, serde::Deserialize)]
struct SqlEnvelope {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    degraded: bool,
    #[serde(default)]
    provenance: Option<RawProvenance>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawProvenance {
    #[serde(default)]
    as_of: Option<i64>,
    #[serde(default)]
    sealed_through: Option<i64>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    registry_hash: Option<String>,
    #[serde(default)]
    nid: Option<String>,
}

/// Rows plus the nest's account of when they were true.
#[derive(Debug, Clone)]
pub struct SqlAnswer {
    pub rows: Vec<serde_json::Value>,
    pub count: i64,
    pub provenance: Provenance,
}

/// `/ready` as the nest answers it. The nest's own judgement, which beats inferring liveness from
/// the outside: a process that answers instantly with three-week-old data is healthy by every
/// external measure and is the failure that matters.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ReadyBody {
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub lag_blocks: Option<i64>,
    #[serde(default)]
    pub tip: Option<i64>,
    #[serde(default)]
    pub last_block: Option<i64>,
    #[serde(default)]
    pub sealed_through: Option<i64>,
    #[serde(default)]
    pub stalled: bool,
    #[serde(default)]
    pub wedged: bool,
    #[serde(default)]
    pub entities_stalled: bool,
    #[serde(default)]
    pub initial_poll_failed: bool,
    #[serde(default)]
    pub quarantined: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

impl ReadyBody {
    /// Why it is not ready, in words an operator can act on. "Not ready" is a status, not a
    /// diagnosis, and the nest already distinguishes four different problems.
    pub fn explain(&self) -> String {
        if self.quarantined {
            return format!(
                "quarantined: {}",
                self.reason
                    .clone()
                    .unwrap_or_else(|| "no reason given".into())
            );
        }
        if self.initial_poll_failed {
            return "first poll failed: it has never reached the chain".into();
        }
        if self.wedged {
            return "wedged".into();
        }
        if self.stalled {
            return "stalled: it is running but no longer following the chain".into();
        }
        if self.entities_stalled {
            return "a derived entity is stalled".into();
        }
        self.reason.clone().unwrap_or_else(|| "not ready".into())
    }
}

/// How to reach the nests, and how hard they may be pushed.
#[derive(Debug, Clone)]
pub struct NestConfig {
    /// The shared origin every nest sits behind, without a trailing slash.
    pub origin: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Path per nest. Overridable so a staged nest can be pointed at without a code change.
    pub base_paths: HashMap<NestId, String>,
    /// The nest's own concurrent-query cap. Ours must not exceed it.
    pub permits: HashMap<NestId, usize>,
    /// How long a user read will queue for a permit before giving up. Short: a caller waiting
    /// thirty seconds for a *slot* has already had a bad time.
    pub read_queue_timeout: Duration,
    /// How long a cron will queue. Long: nothing is waiting on it.
    pub cron_queue_timeout: Duration,
}

impl NestConfig {
    pub fn new(origin: impl Into<String>) -> Self {
        let mut base_paths = HashMap::new();
        let mut permits = HashMap::new();
        for n in NestId::ALL {
            base_paths.insert(n, n.default_base_path().to_string());
            // The protocol nest runs with NUTHATCH_SQL_MAX_CONCURRENCY=4 by drop-in; the rest
            // admit two. Raising these is a change to the nest, never to us.
            permits.insert(n, if n == NestId::Alloc { 4 } else { 2 });
        }
        Self {
            origin: origin.into().trim_end_matches('/').to_string(),
            user: None,
            password: None,
            base_paths,
            permits,
            // See `default_read_queue_secs` in kittiwake-config: three seconds is shorter
            // than a cold fold, which stops the cache ever filling. Keep the two in step.
            read_queue_timeout: Duration::from_secs(20),
            cron_queue_timeout: Duration::from_secs(120),
        }
    }

    pub fn with_auth(mut self, user: Option<String>, password: Option<String>) -> Self {
        self.user = user;
        self.password = password;
        self
    }

    pub fn base_path(&self, nest: NestId) -> &str {
        self.base_paths
            .get(&nest)
            .map(String::as_str)
            .unwrap_or_else(|| nest.default_base_path())
    }

    pub fn permits_for(&self, nest: NestId) -> usize {
        self.permits.get(&nest).copied().unwrap_or(2)
    }
}

/// One query, and how patient we are about it.
#[derive(Debug, Clone)]
pub struct QuerySpec {
    pub nest: NestId,
    pub sql: String,
    pub lane: Lane,
    /// Wall-clock ceiling on the nest's answer. The public SQL surface uses six seconds to match
    /// the nest's own; the protocol folds legitimately take longer and get more.
    pub timeout: Duration,
}

impl QuerySpec {
    pub fn read(nest: NestId, sql: impl Into<String>) -> Self {
        Self {
            nest,
            sql: sql.into(),
            lane: Lane::Read,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn cron(nest: NestId, sql: impl Into<String>) -> Self {
        Self {
            nest,
            sql: sql.into(),
            lane: Lane::Cron,
            timeout: Duration::from_secs(60),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

type Flight = Shared<BoxFuture<'static, Result<Arc<SqlAnswer>, NestError>>>;

struct InFlight {
    flight: Flight,
    /// Set once the flight holds its nest permit.
    admitted: Arc<AtomicBool>,
}

struct GateInner {
    cfg: NestConfig,
    http: reqwest::Client,
    /// The real budget for a nest. Every read, warm and cron passes through here.
    permits: HashMap<NestId, Arc<Semaphore>>,
    /// An inner, smaller budget that only crons and the warmer take. Sized one below the real
    /// count, so a background job can never hold every permit while a user waits.
    cron_permits: HashMap<NestId, Arc<Semaphore>>,
    inflight: Mutex<HashMap<String, InFlight>>,
    neg_cache: moka::future::Cache<String, NestError>,
    ready_cache: moka::future::Cache<NestId, Result<ReadyBody, NestError>>,
}

/// The inner lane: one permit narrower than the nest admits, so a background job can never take
/// the last slot from a user who is waiting. Never zero, because a one-permit nest still has to be
/// usable by the jobs that feed it.
fn cron_width(permits: usize) -> usize {
    permits.saturating_sub(1).max(1)
}

/// The admission gate. Cheap to clone; every clone shares the same permits.
#[derive(Clone)]
pub struct NestGate {
    inner: Arc<GateInner>,
}

impl NestGate {
    pub fn new(cfg: NestConfig) -> Result<Self, NestError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kittiwake/", env!("CARGO_PKG_VERSION")))
            // Keep sockets warm: the nest origin is one host and we talk to it constantly.
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| NestError::Transport(e.to_string()))?;

        let mut permits = HashMap::new();
        let mut cron_permits = HashMap::new();
        for n in NestId::ALL {
            let p = cfg.permits_for(n);
            // Registered here rather than on first use. A counter that springs into existence only
            // once it is non-zero makes "nothing was refused" and "never wired up" the same absent
            // line, and absence reads as health. The gauge puts the cap beside the refusals so the
            // number can be judged without reading the config on the box.
            metrics::counter!("nest_busy_total", "nest" => n.as_str()).increment(0);
            metrics::counter!("nest_query_total", "nest" => n.as_str()).increment(0);
            // Same reasoning as the two above, and it cost an afternoon to learn twice: this one
            // was not registered, so "nothing timed out" and "the counter does not exist" were the
            // same absent line while a fold was deadlocking on every call.
            metrics::counter!("nest_gate_timeout_total", "nest" => n.as_str()).increment(0);
            metrics::gauge!("nest_permits", "nest" => n.as_str()).set(p as f64);
            permits.insert(n, Arc::new(Semaphore::new(p)));
            cron_permits.insert(n, Arc::new(Semaphore::new(cron_width(p))));
        }

        Ok(Self {
            inner: Arc::new(GateInner {
                http,
                permits,
                cron_permits,
                inflight: Mutex::new(HashMap::new()),
                neg_cache: moka::future::Cache::builder()
                    .max_capacity(4096)
                    .time_to_live(Duration::from_secs(5))
                    .build(),
                ready_cache: moka::future::Cache::builder()
                    .max_capacity(16)
                    // Short enough to notice a nest falling over, long enough that a burst of
                    // page views does not become a burst of /ready probes.
                    .time_to_live(Duration::from_secs(10))
                    .build(),
                cfg,
            }),
        })
    }

    pub fn config(&self) -> &NestConfig {
        &self.inner.cfg
    }

    /// How many queries to this nest may usefully be in flight at once on this lane.
    ///
    /// This is the number a caller fanning out should use, and the reason it exists is #43: a
    /// hardcoded `buffer_unordered(4)` against a two-permit nest does not go four at a time, it
    /// queues by construction and then blames the nest. Asking for more slots than the gate holds
    /// buys nothing and costs a `nest_gate_timeout_total` when the queue outlasts
    /// `read_queue_timeout`.
    ///
    /// It is a width, not a reservation. Nothing is held by calling it, and by the time the caller
    /// acts on it another task may have taken a permit - which is fine, because the gate is still
    /// the thing that admits. This only stops us queueing on purpose.
    pub fn lane_width(&self, nest: NestId, lane: Lane) -> usize {
        let p = self.inner.cfg.permits_for(nest);
        match lane {
            Lane::Read => p.max(1),
            Lane::Cron => cron_width(p),
        }
    }

    /// Permits currently free for a nest. Exposed for metrics and tests, not for decisions.
    pub fn available_permits(&self, nest: NestId) -> usize {
        self.inner
            .permits
            .get(&nest)
            .map(|s| s.available_permits())
            .unwrap_or(0)
    }

    fn url(&self, nest: NestId, suffix: &str) -> String {
        format!(
            "{}{}{}",
            self.inner.cfg.origin,
            self.inner.cfg.base_path(nest),
            suffix
        )
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.inner.cfg.user, &self.inner.cfg.password) {
            (Some(u), p) => rb.basic_auth(u, p.clone()),
            _ => rb,
        }
    }

    /// Ask the nest whether it is following the chain. Cached briefly; archival nests are exempt
    /// because a frozen archive reports `stalled` for ever and is correct to.
    pub async fn ready(&self, nest: NestId) -> Result<ReadyBody, NestError> {
        if let Some(cached) = self.inner.ready_cache.get(&nest).await {
            return cached;
        }
        let result = self.fetch_ready(nest).await;
        self.inner.ready_cache.insert(nest, result.clone()).await;
        result
    }

    /// The nest's own description of what it holds: one entry per decoded event table.
    ///
    /// Not cached here. The SQL catalogue is the only caller, it caches the answer itself, and a
    /// second cache underneath that would mean a nest coming back after a redeploy takes two
    /// expiries to show up rather than one.
    pub async fn tables(&self, nest: NestId) -> Result<Vec<serde_json::Value>, NestError> {
        let req = self.authed(self.inner.http.get(self.url(nest, "/tables")));
        let resp = tokio::time::timeout(Duration::from_secs(15), req.send())
            .await
            .map_err(|_| NestError::Timeout)?
            .map_err(|e| NestError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let reason = nest_reason(&resp.text().await.unwrap_or_default());
            return Err(NestError::Http { status, reason });
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| NestError::Decode(e.to_string()))?;
        Ok(body
            .get("tables")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default())
    }

    async fn fetch_ready(&self, nest: NestId) -> Result<ReadyBody, NestError> {
        let req = self.authed(self.inner.http.get(self.url(nest, "/ready")));
        let resp = tokio::time::timeout(Duration::from_secs(5), req.send())
            .await
            .map_err(|_| NestError::Timeout)?
            .map_err(|e| NestError::Transport(e.to_string()))?;

        let status = resp.status();
        let body: ReadyBody = resp.json().await.unwrap_or_default();

        // 503 is how a nest says "not ready", so it is an answer rather than a failure.
        if status.is_success() && body.ready {
            return Ok(body);
        }
        if status.is_success() || status.as_u16() == 503 {
            if nest.is_archival() {
                // A frozen archive is not broken. Report it ready and let the response's own
                // provenance tell the reader where its data ends.
                return Ok(body);
            }
            return Err(NestError::Unready {
                reason: body.explain(),
                lag_blocks: body.lag_blocks,
                sealed_through: body.sealed_through,
            });
        }
        Err(NestError::Http {
            status: status.as_u16(),
            reason: body.explain(),
        })
    }

    /// Run one query, through the gate. No caching, no coalescing: this is the raw path.
    pub async fn query(&self, spec: &QuerySpec) -> Result<SqlAnswer, NestError> {
        self.query_marking(spec, None).await
    }

    /// [`Self::query`], setting `admitted` once the nest permit is held.
    async fn query_marking(
        &self,
        spec: &QuerySpec,
        admitted: Option<&AtomicBool>,
    ) -> Result<SqlAnswer, NestError> {
        let (queue_timeout, _lane_permit) = match spec.lane {
            Lane::Read => (self.inner.cfg.read_queue_timeout, None),
            Lane::Cron => {
                let sem = self
                    .inner
                    .cron_permits
                    .get(&spec.nest)
                    .cloned()
                    .ok_or(NestError::GateTimeout)?;
                let p =
                    tokio::time::timeout(self.inner.cfg.cron_queue_timeout, sem.acquire_owned())
                        .await
                        .map_err(|_| NestError::GateTimeout)?
                        .map_err(|_| NestError::GateTimeout)?;
                (self.inner.cfg.cron_queue_timeout, Some(p))
            }
        };

        let sem = self
            .inner
            .permits
            .get(&spec.nest)
            .cloned()
            .ok_or(NestError::GateTimeout)?;
        let _permit = tokio::time::timeout(queue_timeout, sem.acquire_owned())
            .await
            .map_err(|_| {
                metrics::counter!("nest_gate_timeout_total", "nest" => spec.nest.as_str())
                    .increment(1);
                NestError::GateTimeout
            })?
            .map_err(|_| NestError::GateTimeout)?;

        if let Some(a) = admitted {
            a.store(true, Ordering::Release);
        }
        metrics::counter!("nest_query_total", "nest" => spec.nest.as_str()).increment(1);
        self.execute(spec).await
    }

    async fn execute(&self, spec: &QuerySpec) -> Result<SqlAnswer, NestError> {
        let req = self
            .authed(self.inner.http.get(self.url(spec.nest, "/sql")))
            .query(&[("q", spec.sql.as_str())]);

        let resp = tokio::time::timeout(spec.timeout, req.send())
            .await
            .map_err(|_| NestError::Timeout)?
            .map_err(|e| {
                if e.is_timeout() {
                    NestError::Timeout
                } else {
                    NestError::Transport(e.to_string())
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // The nest protecting itself, which is a different thing from the nest being broken.
            if status.as_u16() == 503 && body.contains("server busy") {
                metrics::counter!("nest_busy_total", "nest" => spec.nest.as_str()).increment(1);
                return Err(NestError::Busy);
            }
            // The nest's own query budget ran out: the same fact as our timeout, told by the nest.
            if status.as_u16() == 504 {
                return Err(NestError::Timeout);
            }
            return Err(NestError::Http {
                status: status.as_u16(),
                reason: nest_reason(&body),
            });
        }

        let env: SqlEnvelope = resp
            .json()
            .await
            .map_err(|e| NestError::Decode(e.to_string()))?;
        let raw = env.provenance.unwrap_or(RawProvenance {
            as_of: None,
            sealed_through: None,
            source: None,
            registry_hash: None,
            nid: None,
        });

        Ok(SqlAnswer {
            rows: env.rows,
            count: env.count,
            provenance: Provenance {
                as_of: raw.as_of,
                sealed_through: raw.sealed_through,
                registry_hash: raw.registry_hash,
                nid: raw.nid,
                source: raw.source.or_else(|| Some(spec.nest.as_str().to_string())),
                truncated: env.truncated,
                degraded: env.degraded,
                partial: false,
            },
        })
    }

    /// Run one query, coalesced: concurrent callers with the same key share a single nest query.
    ///
    /// This is the difference between a cold cache costing one fold and costing one fold per
    /// waiting browser tab. The failure is shared too, briefly, so a broken nest is not asked
    /// again by everyone who was queued behind the first failure.
    pub async fn query_coalesced(
        &self,
        key: &str,
        spec: QuerySpec,
    ) -> Result<Arc<SqlAnswer>, NestError> {
        // The caller's key is a label; two statements must never share a flight under one.
        let base = format!(
            "{key}:{}:{}",
            spec.nest.as_str(),
            blake3::hash(spec.sql.as_bytes()).to_hex()
        );
        let slot = |lane: Lane| match lane {
            Lane::Read => format!("{base}:read"),
            Lane::Cron => format!("{base}:cron"),
        };
        let own = slot(spec.lane);
        if let Some(err) = self.inner.neg_cache.get(&own).await {
            metrics::counter!("nest_negative_cache_hit_total").increment(1);
            return Err(err);
        }

        let (slot_key, flight) = {
            let mut map = self.inner.inflight.lock().await;
            let joined = map
                .get(&own)
                .map(|f| (own.clone(), f.flight.clone()))
                .or_else(|| {
                    // A reader rides a background flight only once it holds its permit: before
                    // that, joining it means waiting in the cron lane's queue and on its timeouts.
                    let cron = (spec.lane == Lane::Read).then(|| slot(Lane::Cron))?;
                    let f = map
                        .get(&cron)
                        .filter(|f| f.admitted.load(Ordering::Acquire))?;
                    Some((cron, f.flight.clone()))
                });
            if let Some(joined) = joined {
                metrics::counter!("nest_coalesced_total").increment(1);
                joined
            } else {
                let gate = self.clone();
                let admitted = Arc::new(AtomicBool::new(false));
                let marker = admitted.clone();
                // **Spawned, not merely shared.**
                //
                // A `Shared` future only advances while somebody polls it. The leader used to be
                // polled by whichever caller held it, so when every waiter gave up - each on its
                // own outer timeout - the future was left suspended *inside* `query`, after it had
                // taken a nest permit and before it released one. The `inflight` map kept it alive,
                // so the permit was never dropped either.
                //
                // Eight of those and the pool is gone for good: on 2026-09-12 `nest_query_total`
                // did not move for 51 seconds while `nest_gate_timeout_total` climbed, with no
                // connections open to the nest and the box at 0.01 load. Nothing was slow. The
                // permits were held by futures nobody was polling.
                //
                // A spawned task runs to completion whether or not anyone is still waiting, so the
                // permit is always released - which is the property the gate depends on and the
                // one `shared()` alone cannot give it.
                let handle = tokio::spawn(async move {
                    gate.query_marking(&spec, Some(&marker)).await.map(Arc::new)
                });
                let fut: BoxFuture<'static, Result<Arc<SqlAnswer>, NestError>> = async move {
                    match handle.await {
                        Ok(r) => r,
                        // The task was cancelled or panicked. Reported as transport rather than
                        // swallowed: a caller that gets no answer must not read it as an empty one.
                        Err(e) => Err(NestError::Transport(format!("query task failed: {e}"))),
                    }
                }
                .boxed();
                let shared = fut.shared();
                map.insert(
                    own.clone(),
                    InFlight {
                        flight: shared.clone(),
                        admitted,
                    },
                );
                (own, shared)
            }
        };

        let result = flight.clone().await;

        // Whoever finishes first clears the slot; a late waiter still gets the shared result.
        {
            let mut map = self.inner.inflight.lock().await;
            if map.get(&slot_key).is_some_and(|f| f.flight.ptr_eq(&flight)) {
                map.remove(&slot_key);
            }
        }

        // Filed under the flight's own lane, so a cron's queue timeout is never a reader's answer.
        if let Err(ref e) = result {
            self.inner.neg_cache.insert(slot_key, e.clone()).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A permit must come back even when nobody is left waiting for the answer.
    ///
    /// This is the shape that killed the pool: a caller starts a query, the query takes a permit,
    /// and then the caller goes away - its own timeout fires, or the request is dropped. If the
    /// work only advances while somebody polls it, it stops mid-query holding the permit, and the
    /// in-flight map keeps it alive so nothing is ever dropped. Repeat until the pool is empty and
    /// the service is wedged with no connections open and nothing to show for it.
    ///
    /// Asserted on a plain semaphore, because that is the invariant the gate rests on and it holds
    /// regardless of how the query is spelled: work that owns a permit must be driven to
    /// completion by something that outlives the caller.
    #[tokio::test]
    async fn a_permit_survives_its_caller_going_away() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));

        // The leader: takes the permit, then yields as a real query would while waiting on the net.
        let s = sem.clone();
        let handle = tokio::spawn(async move {
            let _permit = s.acquire_owned().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        });

        // The caller gives up long before the work finishes.
        let gave_up = tokio::time::timeout(std::time::Duration::from_millis(5), async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        })
        .await;
        assert!(
            gave_up.is_err(),
            "the caller must have given up for this to mean anything"
        );

        // The spawned work finishes anyway, and the permit comes back.
        handle.await.unwrap();
        assert_eq!(
            sem.available_permits(),
            1,
            "the permit was not returned after its caller went away"
        );
    }

    /// The counterpart, and the bug: work that is only polled by its caller stops when the caller
    /// does, and its permit stops with it.
    #[tokio::test]
    async fn an_unpolled_future_never_gives_its_permit_back() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let s = sem.clone();

        let leader = async move {
            let _permit = s.acquire_owned().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        };

        // Polled once, briefly, then abandoned - but held, exactly as the in-flight map held it.
        let mut boxed = Box::pin(leader);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(20), &mut boxed).await;

        assert_eq!(
            sem.available_permits(),
            0,
            "this is the leak: the permit is still held by a future nobody is polling"
        );
        drop(boxed);
        assert_eq!(
            sem.available_permits(),
            1,
            "dropping it is the only way it came back"
        );
    }
    use std::sync::atomic::AtomicUsize;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    fn envelope(rows: usize) -> serde_json::Value {
        serde_json::json!({
            "count": rows,
            "rows": (0..rows).map(|i| serde_json::json!({"i": i})).collect::<Vec<_>>(),
            "provenance": {
                "as_of": 499_688_638i64,
                "sealed_through": 499_685_543i64,
                "source": "graph-allocations-nest",
                "registry_hash": "0xbfb02cde",
                "nid": "36d3c714"
            }
        })
    }

    async fn gate_for(server: &MockServer, permits: usize) -> NestGate {
        let mut cfg = NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, String::new());
            cfg.permits.insert(n, permits);
        }
        cfg.read_queue_timeout = Duration::from_millis(300);
        NestGate::new(cfg).unwrap()
    }

    #[tokio::test]
    async fn provenance_survives_the_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(2)))
            .mount(&server)
            .await;

        let gate = gate_for(&server, 4).await;
        let answer = gate
            .query(&QuerySpec::read(NestId::Alloc, "SELECT 1"))
            .await
            .unwrap();

        assert_eq!(answer.rows.len(), 2);
        assert_eq!(answer.provenance.as_of, Some(499_688_638));
        assert_eq!(answer.provenance.sealed_through, Some(499_685_543));
        assert_eq!(
            answer.provenance.source.as_deref(),
            Some("graph-allocations-nest")
        );
    }

    #[tokio::test]
    async fn a_busy_nest_is_told_apart_from_a_broken_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string("server busy: too many concurrent SQL queries"),
            )
            .mount(&server)
            .await;

        let gate = gate_for(&server, 4).await;
        let err = gate
            .query(&QuerySpec::read(NestId::Alloc, "SELECT 1"))
            .await
            .unwrap_err();
        assert_eq!(err, NestError::Busy);

        let api: ApiError = err.into();
        assert_eq!(api.status(), 503);
        assert_eq!(api.code(), "nest_busy");
    }

    /// The property the whole gate exists for: however many callers arrive, the nest never sees
    /// more concurrent queries than it admits.
    #[tokio::test]
    async fn the_nest_never_sees_more_concurrency_than_it_admits() {
        struct Counter {
            live: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }
        impl Respond for Counter {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(60));
                self.live.fetch_sub(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(envelope(1))
            }
        }

        let server = MockServer::start().await;
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(Counter {
                live: live.clone(),
                peak: peak.clone(),
            })
            .mount(&server)
            .await;

        let mut cfg = NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, String::new());
            cfg.permits.insert(n, 4);
        }
        cfg.read_queue_timeout = Duration::from_secs(30);
        let gate = NestGate::new(cfg).unwrap();

        // Twenty distinct queries, so nothing coalesces and every one must pass the gate.
        let mut tasks = Vec::new();
        for i in 0..20 {
            let g = gate.clone();
            tasks.push(tokio::spawn(async move {
                g.query(&QuerySpec::read(NestId::Alloc, format!("SELECT {i}")))
                    .await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) <= 4,
            "peak was {}",
            peak.load(Ordering::SeqCst)
        );
    }

    /// Crons must never hold the last permit, or a background job makes the site look down.
    #[tokio::test]
    async fn a_cron_cannot_take_the_last_permit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(1)))
            .mount(&server)
            .await;

        let mut cfg = NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, String::new());
            cfg.permits.insert(n, 4);
        }
        let gate = NestGate::new(cfg).unwrap();

        // The cron lane is one narrower than the nest's own cap.
        let cron_sem = gate.inner.cron_permits.get(&NestId::Alloc).unwrap();
        assert_eq!(cron_sem.available_permits(), 3);
        assert_eq!(gate.available_permits(NestId::Alloc), 4);
    }

    /// N identical cold callers must cost the nest exactly one fold.
    #[tokio::test]
    async fn identical_cold_callers_cost_one_query() {
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        struct Counting(Arc<AtomicUsize>);
        impl Respond for Counting {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                self.0.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(80));
                ResponseTemplate::new(200).set_body_json(envelope(3))
            }
        }
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(Counting(hits.clone()))
            .mount(&server)
            .await;

        let gate = gate_for(&server, 4).await;
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let g = gate.clone();
            tasks.push(tokio::spawn(async move {
                g.query_coalesced("indexers:v1", QuerySpec::read(NestId::Alloc, "SELECT 1"))
                    .await
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap().rows.len(), 3);
        }

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the nest should have been asked once"
        );
    }

    /// A key that forgets a parameter must not hand one statement's rows to another.
    #[tokio::test]
    async fn one_key_over_two_statements_runs_both() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        for (q, rows) in [("SELECT 1", 1), ("SELECT 2", 2)] {
            Mock::given(method("GET"))
                .and(path("/sql"))
                .and(query_param("q", q))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(envelope(rows))
                        .set_delay(Duration::from_millis(80)),
                )
                .mount(&server)
                .await;
        }

        let gate = gate_for(&server, 4).await;
        let (a, b) = tokio::join!(
            gate.query_coalesced("same", QuerySpec::read(NestId::Alloc, "SELECT 1")),
            gate.query_coalesced("same", QuerySpec::read(NestId::Alloc, "SELECT 2")),
        );
        assert_eq!(a.unwrap().rows.len(), 1);
        assert_eq!(b.unwrap().rows.len(), 2);
    }

    /// Answers `slow` after a long wait and anything else soon, counting only the latter.
    async fn slow_and_quick(server: &MockServer, hits: Arc<AtomicUsize>) {
        use wiremock::matchers::query_param;
        struct Counting(Arc<AtomicUsize>, Duration);
        impl Respond for Counting {
            fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
                self.0.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_body_json(envelope(1))
                    .set_delay(self.1)
            }
        }
        Mock::given(method("GET"))
            .and(path("/sql"))
            .and(query_param("q", "slow"))
            .respond_with(Counting(
                Arc::new(AtomicUsize::new(0)),
                Duration::from_secs(3),
            ))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(Counting(hits, Duration::from_millis(200)))
            .mount(server)
            .await;
    }

    /// A reader must not wait in a background job's queue while a read permit sits free.
    #[tokio::test]
    async fn a_reader_does_not_queue_behind_a_cron_flight_that_has_not_started() {
        let server = MockServer::start().await;
        slow_and_quick(&server, Arc::new(AtomicUsize::new(0))).await;
        let gate = gate_for(&server, 2).await;

        let g = gate.clone();
        tokio::spawn(async move { g.query(&QuerySpec::cron(NestId::Alloc, "slow")).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let g = gate.clone();
        tokio::spawn(async move {
            g.query_coalesced("k", QuerySpec::cron(NestId::Alloc, "SELECT k"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(gate.available_permits(NestId::Alloc), 1);

        let started = std::time::Instant::now();
        let read = gate
            .query_coalesced("k", QuerySpec::read(NestId::Alloc, "SELECT k"))
            .await;
        assert!(read.is_ok(), "{read:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_cron_queue_timeout_is_not_remembered_for_readers() {
        let server = MockServer::start().await;
        slow_and_quick(&server, Arc::new(AtomicUsize::new(0))).await;
        let mut cfg = NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, String::new());
            cfg.permits.insert(n, 2);
        }
        cfg.cron_queue_timeout = Duration::from_millis(100);
        let gate = NestGate::new(cfg).unwrap();

        let g = gate.clone();
        tokio::spawn(async move { g.query(&QuerySpec::cron(NestId::Alloc, "slow")).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cron = gate
            .query_coalesced("k", QuerySpec::cron(NestId::Alloc, "SELECT k"))
            .await;
        assert_eq!(cron.unwrap_err(), NestError::GateTimeout);

        let read = gate
            .query_coalesced("k", QuerySpec::read(NestId::Alloc, "SELECT k"))
            .await;
        assert!(read.is_ok(), "{read:?}");
    }

    /// Once a cron flight holds its permit, a reader asking the same thing rides it.
    #[tokio::test]
    async fn a_reader_joins_a_cron_flight_that_is_already_running() {
        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        slow_and_quick(&server, hits.clone()).await;
        let gate = gate_for(&server, 2).await;

        let g = gate.clone();
        let cron = tokio::spawn(async move {
            g.query_coalesced("k", QuerySpec::cron(NestId::Alloc, "SELECT k"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let read = gate
            .query_coalesced("k", QuerySpec::read(NestId::Alloc, "SELECT k"))
            .await;
        assert!(read.is_ok() && cron.await.unwrap().is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unready_nest_reports_its_own_reason() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ready"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "ready": false, "stalled": true, "lag_blocks": 41_233, "sealed_through": 499_000_000i64
            })))
            .mount(&server)
            .await;

        let gate = gate_for(&server, 4).await;
        let err = gate.ready(NestId::Alloc).await.unwrap_err();
        match err {
            NestError::Unready {
                reason, lag_blocks, ..
            } => {
                assert!(reason.contains("stalled"), "{reason}");
                assert_eq!(lag_blocks, Some(41_233));
            }
            other => panic!("expected Unready, got {other:?}"),
        }
    }

    /// The nest's query guard answers 504 when a sound query runs past its budget. That is the same
    /// fact as our own timeout, so it is reported as one, not as an upstream fault.
    #[tokio::test]
    async fn a_nest_query_timeout_is_a_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(ResponseTemplate::new(504).set_body_json(serde_json::json!({
                "error": "query exceeded the 30s time budget on the read-only SQL surface",
                "timeout_secs": 30
            })))
            .mount(&server)
            .await;

        let gate = gate_for(&server, 4).await;
        let err = gate
            .query(&QuerySpec::read(NestId::Alloc, "SELECT 1"))
            .await
            .unwrap_err();
        assert!(matches!(err, NestError::Timeout), "{err:?}");
    }

    /// Any other refusal keeps the nest's own sentence, so the alert built on it can say why.
    #[tokio::test]
    async fn an_upstream_error_carries_the_nests_reason() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sql"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "query failed: Out of Memory Error: failed to pin block\n\nhint: lower the query"
            })))
            .mount(&server)
            .await;

        let gate = gate_for(&server, 4).await;
        let err = gate
            .query(&QuerySpec::read(NestId::Alloc, "SELECT 1"))
            .await
            .unwrap_err();
        match err {
            NestError::Http { status, reason } => {
                assert_eq!(status, 400);
                assert_eq!(
                    reason,
                    "query failed: Out of Memory Error: failed to pin block"
                );
            }
            other => panic!("expected Http, got {other:?}"),
        }
        let api: kittiwake_types::ApiError = NestError::Http {
            status: 400,
            reason: "query failed".into(),
        }
        .into();
        assert_eq!(api.status(), 502);
        assert_eq!(api.to_string(), "nest upstream error 400: query failed");
    }

    /// A frozen archive says `stalled` for ever and is right to. Alerting on it would cry wolf
    /// from day one and the next real outage would be ignored with it.
    #[tokio::test]
    async fn a_frozen_archive_is_not_an_outage() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ready"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "ready": false, "stalled": true, "sealed_through": 497_860_300i64
            })))
            .mount(&server)
            .await;

        let gate = gate_for(&server, 2).await;
        let body = gate
            .ready(NestId::LegacyFlows)
            .await
            .expect("archive is not an outage");
        assert!(body.stalled);
    }
}

#[cfg(test)]
mod metric_tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// #6 asked whether `nest_busy_total` had gone to zero after the cutover, and the honest
    /// answer from outside was that nobody could tell: the counter did not exist until something
    /// refused, so a healthy service and an unwired one scraped identically. A local recorder
    /// rather than the global one, so this does not depend on test ordering.
    #[test]
    fn every_nest_reports_a_zero_before_anything_has_been_refused() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            NestGate::new(NestConfig::new("http://nest.invalid")).unwrap();
        });

        let rendered = handle.render();
        for n in NestId::ALL {
            for metric in ["nest_busy_total", "nest_query_total", "nest_permits"] {
                let line = format!("{metric}{{nest=\"{}\"}}", n.as_str());
                assert!(
                    rendered.contains(&line),
                    "{line} is missing, so its absence would read as health:\n{rendered}"
                );
            }
        }
        // The cap the gate was built with, visible without reading the box's config.
        assert!(
            rendered.contains("nest_permits{nest=\"alloc\"} 4"),
            "{rendered}"
        );
    }
}
