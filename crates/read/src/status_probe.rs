//! What an indexer's own `/status` endpoint said, asked behind the request rather than inside it.
//!
//! `/api/indexer-status` used to probe the node on every call and wait out the probe's eight
//! seconds, so a node that was slow or down cost every page load eight seconds (2026-09-15). A
//! reader now waits at most [`WAIT`], and is told what the last probe found and how old it is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use kittiwake_ext::ProbeResult;
use tokio::time::Instant;

/// Asks one node what it is serving.
pub type Probe = Arc<dyn Fn(String) -> BoxFuture<'static, ProbeResult> + Send + Sync>;

/// A probe this young is served without asking the node again. Lodestar asks every thirty seconds.
pub const FRESH_FOR: Duration = Duration::from_secs(60);

/// A probe this old no longer describes the node, and is not served as though it did.
pub const SERVE_FOR: Duration = Duration::from_secs(10 * 60);

/// The longest a reader waits for the node. The probe carries on to its own timeout behind the
/// answer, and the next reader is served what it found.
pub const WAIT: Duration = Duration::from_secs(2);

/// One finished probe.
pub struct Checked {
    pub result: ProbeResult,
    /// Unix seconds.
    pub checked_at: i64,
    at: Instant,
}

impl Checked {
    pub fn age(&self) -> Duration {
        self.at.elapsed()
    }
}

/// What a reader may be told about a node.
pub struct NodeView {
    /// The probe to report, or `None` when none young enough has finished.
    pub checked: Option<Arc<Checked>>,
    /// Unix seconds of the last probe that reached the node, however long ago.
    pub last_reached_at: Option<i64>,
    /// A probe is running behind this answer.
    pub pending: bool,
}

impl NodeView {
    pub fn json(&self) -> serde_json::Value {
        let c = self.checked.as_deref();
        serde_json::json!({
            // Null until a probe has finished: not having asked yet says nothing about the node.
            "reachable": c.map(|c| c.result.reachable),
            "checkedAt": c.map(|c| c.checked_at),
            "ageSeconds": c.map(|c| c.age().as_secs()),
            "stale": c.is_some_and(|c| c.age() >= FRESH_FOR),
            "lastReachedAt": self.last_reached_at,
            "error": c.and_then(|c| c.result.error.clone()),
            "pending": self.pending,
        })
    }
}

#[derive(Default)]
struct Slot {
    last: Option<Arc<Checked>>,
    last_reached_at: Option<i64>,
    running: Option<Shared<BoxFuture<'static, ()>>>,
}

impl Slot {
    fn view(&self) -> NodeView {
        NodeView {
            checked: self.last.clone().filter(|c| c.age() < SERVE_FOR),
            last_reached_at: self.last_reached_at,
            pending: self.running.is_some(),
        }
    }
}

/// The last probe of each node, keyed by the URL the registry gives it. Cheap to clone.
#[derive(Clone)]
pub struct StatusProbes {
    probe: Probe,
    wait: Duration,
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

impl StatusProbes {
    pub fn new(ext: kittiwake_ext::Ext) -> Self {
        Self::with_probe(
            Arc::new(move |url| {
                let ext = ext.clone();
                Box::pin(async move { ext.probe_indexer(&url).await })
            }),
            WAIT,
        )
    }

    pub fn with_probe(probe: Probe, wait: Duration) -> Self {
        Self {
            probe,
            wait,
            slots: Arc::default(),
        }
    }

    /// What is known about the node at `url`. A probe that is no longer fresh is repeated behind
    /// the answer; the reader waits for it only when there is nothing to serve, and then briefly.
    pub async fn observe(&self, url: &str) -> NodeView {
        let running = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry(url.to_string()).or_default();
            let age = slot.last.as_ref().map(|c| c.age());
            if age.is_some_and(|a| a < FRESH_FOR) {
                return slot.view();
            }
            let running = self.start(slot, url);
            if age.is_some_and(|a| a < SERVE_FOR) {
                return slot.view();
            }
            running
        };
        let _ = tokio::time::timeout(self.wait, running).await;
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(url.to_string())
            .or_default()
            .view()
    }

    /// The probe running for this node, started if there is none.
    fn start(&self, slot: &mut Slot, url: &str) -> Shared<BoxFuture<'static, ()>> {
        if let Some(running) = &slot.running {
            return running.clone();
        }
        let probe = (self.probe)(url.to_string());
        let slots = self.slots.clone();
        let url = url.to_string();
        let running = async move {
            let result = probe.await;
            let checked_at = unix_now();
            let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry(url).or_default();
            if result.reachable {
                slot.last_reached_at = Some(checked_at);
            }
            slot.last = Some(Arc::new(Checked {
                result,
                checked_at,
                at: Instant::now(),
            }));
            slot.running = None;
        }
        .boxed()
        .shared();
        // Spawned, so the probe is recorded even after every reader has stopped waiting for it.
        tokio::spawn(running.clone());
        slot.running = Some(running.clone());
        running
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    const URL: &str = "https://indexer.example";

    /// A node that takes the given time over each answer in turn, repeating the last, and counts
    /// how often it was asked.
    fn node(answers: Vec<(Duration, bool)>) -> (StatusProbes, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let probe: Probe = Arc::new(move |url| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (took, reachable) = answers[n.min(answers.len() - 1)];
            Box::pin(async move {
                tokio::time::sleep(took).await;
                ProbeResult {
                    url,
                    reachable,
                    elapsed_ms: took.as_millis() as u64,
                    statuses: Vec::new(),
                    error: (!reachable).then(|| "timed out".to_string()),
                }
            })
        });
        (StatusProbes::with_probe(probe, WAIT), asked)
    }

    const QUICK: Duration = Duration::ZERO;
    const SLOW: Duration = Duration::from_secs(8);

    #[tokio::test(start_paused = true)]
    async fn one_probe_runs_however_many_readers_ask_at_once() {
        let (probes, asked) = node(vec![(Duration::from_secs(1), true)]);
        let views = futures::future::join_all((0..5).map(|_| probes.observe(URL))).await;
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert!(
            views
                .iter()
                .all(|v| v.checked.as_ref().is_some_and(|c| c.result.reachable))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_slower_than_the_wait_costs_a_reader_the_wait_and_the_next_reader_nothing() {
        let (probes, asked) = node(vec![(SLOW, false)]);
        let began = Instant::now();
        let first = probes.observe(URL).await;
        assert!(began.elapsed() < WAIT + Duration::from_millis(10));
        assert!(first.checked.is_none());
        assert!(first.pending);
        assert_eq!(first.json()["reachable"], serde_json::Value::Null);

        tokio::time::sleep(SLOW).await;
        let began = Instant::now();
        let next = probes.observe(URL).await;
        assert_eq!(began.elapsed(), Duration::ZERO);
        let checked = next
            .checked
            .as_ref()
            .expect("the probe finished behind the first answer");
        assert!(!checked.result.reachable);
        assert!(!next.pending);
        assert_eq!(next.last_reached_at, None, "a node never reached");
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_older_probe_is_served_at_once_with_its_age_while_the_node_is_asked_again() {
        let (probes, asked) = node(vec![(QUICK, true), (SLOW, true)]);
        probes.observe(URL).await;
        let old = FRESH_FOR + Duration::from_secs(30);
        tokio::time::sleep(old).await;

        let began = Instant::now();
        let view = probes.observe(URL).await;
        assert_eq!(
            began.elapsed(),
            Duration::ZERO,
            "the reader waited for the node"
        );
        assert_eq!(view.checked.as_ref().unwrap().age(), old);
        assert!(view.pending);
        let json = view.json();
        assert_eq!(json["ageSeconds"], old.as_secs());
        assert_eq!(json["stale"], true);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn when_the_node_was_last_reached_survives_a_probe_that_could_not_reach_it() {
        let (probes, _) = node(vec![(QUICK, true), (QUICK, false)]);
        probes.observe(URL).await;
        tokio::time::sleep(FRESH_FOR).await;
        probes.observe(URL).await;
        tokio::time::sleep(Duration::from_millis(1)).await;

        let view = probes.observe(URL).await;
        assert!(!view.checked.as_ref().unwrap().result.reachable);
        assert!(view.last_reached_at.is_some());
        assert_eq!(view.json()["error"], "timed out");
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_too_old_to_describe_the_node_is_not_served() {
        let (probes, _) = node(vec![(QUICK, true), (SLOW, true)]);
        probes.observe(URL).await;
        tokio::time::sleep(SERVE_FOR).await;

        let view = probes.observe(URL).await;
        assert!(view.checked.is_none());
        assert!(view.pending);
        assert!(
            view.last_reached_at.is_some(),
            "when it was last reached is still said"
        );
    }
}
