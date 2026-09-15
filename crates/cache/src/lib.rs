//! Freshness as a property of the dataset, not a magic number sprinkled per route.
//!
//! The Node service picked a TTL per call site by hand — mostly 300s, some 600s, some 60s — which
//! meant the same figure could be five minutes old on one page and one minute old on another, and
//! nobody could say which was right. Here a policy is derived from the cadence at which the
//! underlying nest actually moves, so "how current is this" has one answer per dataset.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kittiwake_types::{Meta, Provenance, ServedFrom};

/// How long a value of a given dataset stays usable, and on what terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Served without hesitation.
    pub fresh_for: Duration,
    /// Beyond `fresh_for`, served while a refresh runs behind it, labelled stale.
    pub swr_for: Duration,
    /// Beyond `fresh_for + swr_for`, not served at all; the caller waits for the nest.
    pub hard_ttl: Duration,
    /// How long a nest failure is remembered, so a broken nest is not asked N times a second.
    pub neg_ttl: Duration,
}

/// The shortest freshness we will claim. Below this the cache stops absorbing bursts and the nest
/// starts feeling every page view, which is the whole thing we are here to prevent.
const FRESH_FLOOR: Duration = Duration::from_secs(30);
/// Stale-while-revalidate is generous but not unbounded: past this, waiting for the truth beats
/// serving something the reader would not accept if they knew its age.
const SWR_CAP: Duration = Duration::from_secs(300);

impl CachePolicy {
    /// Derive a policy from the cadence at which the source is expected to change.
    ///
    /// A dataset refreshed by a five-minute cron gains nothing from a thirty-second TTL except
    /// five-times the nest load for identical bytes.
    pub fn from_poll_interval(poll: Duration) -> Self {
        let fresh_for = poll.max(FRESH_FLOOR);
        let swr_for = (fresh_for * 10).min(SWR_CAP);
        Self {
            fresh_for,
            swr_for,
            hard_ttl: fresh_for + swr_for,
            neg_ttl: Duration::from_secs(5),
        }
    }
}

/// What to do with a value of a given age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Serve it and stop.
    Fresh,
    /// Serve it, labelled stale, and refresh behind the response.
    Stale,
    /// Do not serve it. Fetch, coalesced with everyone else waiting.
    Expired,
}

pub fn classify(age: Duration, policy: &CachePolicy) -> Freshness {
    if age < policy.fresh_for {
        Freshness::Fresh
    } else if age < policy.hard_ttl {
        Freshness::Stale
    } else {
        Freshness::Expired
    }
}

/// A cached answer, with the nest's account of it kept alongside the bytes.
#[derive(Debug, Clone)]
pub struct Entry {
    pub value: Arc<serde_json::Value>,
    pub provenance: Provenance,
    pub fetched_at: Instant,
}

impl Entry {
    pub fn new(value: serde_json::Value, provenance: Provenance) -> Self {
        Self {
            value: Arc::new(value),
            provenance,
            fetched_at: Instant::now(),
        }
    }

    pub fn age(&self) -> Duration {
        self.fetched_at.elapsed()
    }

    /// The delivery record for this entry, served from `from`.
    pub fn meta(&self, from: ServedFrom, stale: bool) -> Meta {
        Meta {
            provenance: self.provenance.clone(),
            served_from: from,
            stale,
            age_seconds: self.age().as_secs(),
        }
    }
}

/// The cache key. `registry_hash` deliberately does *not* go in the key: it lives in the value, so
/// a registry change forces one refresh rather than orphaning every key under the old hash and
/// silently re-running every fold at once.
pub fn key(schema_ver: u32, dataset: &str, route: &str, params: &serde_json::Value) -> String {
    let canonical = canonical_json(params);
    let digest = blake3::hash(canonical.as_bytes()).to_hex();
    format!("v{schema_ver}:{dataset}:{route}:{}", &digest[..32])
}

/// Serialize with object keys sorted, so two callers who built the same parameters in a different
/// order share a cache entry instead of each paying for their own fold.
fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

/// The in-process cache. One store for every dataset; the policy travels with the call, not the
/// store, so a route cannot accidentally inherit another route's freshness.
#[derive(Clone)]
pub struct Store {
    inner: moka::future::Cache<String, Arc<Entry>>,
}

/// How long an answer that left out a part is kept, whatever its route's policy. No longer than the
/// shortest freshness, so it is never served stale, and the part is asked for again within the minute.
pub const PARTIAL_FOR: Duration = FRESH_FLOOR;

impl Store {
    pub fn new(max_capacity: u64) -> Self {
        Self::keeping_partial_for(max_capacity, PARTIAL_FOR)
    }

    /// As [`Store::new`], keeping an answer that left out a part for `window`.
    pub fn keeping_partial_for(max_capacity: u64, window: Duration) -> Self {
        Self {
            inner: moka::future::Cache::builder()
                .max_capacity(max_capacity)
                // The longest any entry may live regardless of policy, so a dataset that stops
                // being requested cannot pin memory for ever.
                .time_to_live(Duration::from_secs(3600))
                // In the store rather than in each policy, so no reader, revalidation or warmer can
                // serve or keep a partial answer past it.
                .expire_after(PartialExpiry(window))
                .build(),
        }
    }

    pub async fn get(&self, key: &str) -> Option<Arc<Entry>> {
        self.inner.get(key).await
    }

    pub async fn insert(&self, key: String, entry: Entry) -> Arc<Entry> {
        let entry = Arc::new(entry);
        self.inner.insert(key, entry.clone()).await;
        entry
    }

    pub async fn invalidate(&self, key: &str) {
        self.inner.invalidate(key).await;
    }

    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

struct PartialExpiry(Duration);

impl moka::Expiry<String, Arc<Entry>> for PartialExpiry {
    fn expire_after_create(&self, _: &String, entry: &Arc<Entry>, _: Instant) -> Option<Duration> {
        entry.provenance.partial.then_some(self.0)
    }

    // An overwrite decides afresh. moka's default keeps the old entry's expiry, which would keep a
    // partial answer that replaced a whole one for an hour.
    fn expire_after_update(
        &self,
        _: &String,
        entry: &Arc<Entry>,
        _: Instant,
        _: Option<Duration>,
    ) -> Option<Duration> {
        entry.provenance.partial.then_some(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_cadence_is_floored_so_the_nest_is_not_asked_per_page_view() {
        let p = CachePolicy::from_poll_interval(Duration::from_secs(2));
        assert_eq!(p.fresh_for, FRESH_FLOOR);
    }

    #[test]
    fn a_slow_cadence_keeps_its_own_freshness() {
        let p = CachePolicy::from_poll_interval(Duration::from_secs(300));
        assert_eq!(p.fresh_for, Duration::from_secs(300));
        assert_eq!(
            p.swr_for, SWR_CAP,
            "stale-while-revalidate is capped, not 10x300"
        );
        assert_eq!(p.hard_ttl, Duration::from_secs(600));
    }

    #[test]
    fn the_three_ages_map_to_the_three_decisions() {
        let p = CachePolicy::from_poll_interval(Duration::from_secs(60));
        assert_eq!(classify(Duration::from_secs(1), &p), Freshness::Fresh);
        assert_eq!(classify(Duration::from_secs(59), &p), Freshness::Fresh);
        assert_eq!(classify(Duration::from_secs(60), &p), Freshness::Stale);
        assert_eq!(
            classify(p.hard_ttl - Duration::from_secs(1), &p),
            Freshness::Stale
        );
        assert_eq!(classify(p.hard_ttl, &p), Freshness::Expired);
    }

    #[test]
    fn parameter_order_does_not_split_the_cache() {
        let a: serde_json::Value = serde_json::from_str(r#"{"first":10,"skip":0}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"skip":0,"first":10}"#).unwrap();
        assert_eq!(
            key(1, "alloc", "indexers", &a),
            key(1, "alloc", "indexers", &b)
        );
    }

    #[test]
    fn different_parameters_do_not_share_an_entry() {
        let a = serde_json::json!({"first": 10});
        let b = serde_json::json!({"first": 20});
        assert_ne!(
            key(1, "alloc", "indexers", &a),
            key(1, "alloc", "indexers", &b)
        );
    }

    #[test]
    fn bumping_the_schema_version_busts_every_key() {
        let p = serde_json::json!({"first": 10});
        assert_ne!(
            key(1, "alloc", "indexers", &p),
            key(2, "alloc", "indexers", &p)
        );
    }

    #[tokio::test]
    async fn a_stored_entry_comes_back_with_its_provenance() {
        let store = Store::new(64);
        let prov = Provenance {
            as_of: Some(499_000_000),
            ..Default::default()
        };
        store
            .insert(
                "k".into(),
                Entry::new(serde_json::json!({"n": 1}), prov.clone()),
            )
            .await;
        let got = store.get("k").await.expect("entry present");
        assert_eq!(got.provenance, prov);
        assert_eq!(
            got.meta(ServedFrom::Cache, false).served_from,
            ServedFrom::Cache
        );
    }

    /// Kept like any other answer, one missing a part would be served for its route's whole window
    /// as though it were whole, and the part not asked for again until then.
    #[tokio::test]
    async fn an_answer_missing_a_part_is_kept_for_its_window_and_no_longer() {
        let window = Duration::from_millis(200);
        let store = Store::keeping_partial_for(64, window);
        let entry = |partial| {
            Entry::new(
                serde_json::json!({}),
                Provenance {
                    partial,
                    ..Default::default()
                },
            )
        };
        store.insert("partial".into(), entry(true)).await;
        store.insert("whole".into(), entry(false)).await;
        store.insert("made whole".into(), entry(true)).await;
        store.insert("made whole".into(), entry(false)).await;
        store.insert("made partial".into(), entry(false)).await;
        store.insert("made partial".into(), entry(true)).await;
        assert!(store.get("partial").await.is_some(), "kept for its window");

        tokio::time::sleep(window + Duration::from_millis(100)).await;
        assert!(store.get("partial").await.is_none());
        assert!(store.get("made partial").await.is_none());
        assert!(store.get("whole").await.is_some());
        assert!(
            store.get("made whole").await.is_some(),
            "a whole answer replacing a partial one does not inherit its window"
        );
    }
}
