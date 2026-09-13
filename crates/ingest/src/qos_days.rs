//! qos-reo-nest's daily results into Postgres, one UTC day per nest statement.
//!
//! The nest cannot answer more than a day inside its memory budget: at the 512 MB default one day of
//! the chart statement peaks at 779 MB RSS and eight days are refused. So closed days are computed
//! here once, kept, and read back by the routes and the score; only today is asked of the nest live.
//!
//! A stored day is computed again only when something under it changed:
//!
//! - **today**, on every run, because its buckets are still arriving;
//! - **missing**, a day in the window with no stored state;
//! - **registry changed**, the nest's `registry_hash` or `nid` differs from the stored one, so the views
//!   or decoders that produced the day are not the ones serving now;
//! - **documents changed**, a one-day probe finds a different stored bucket count per topic or a
//!   different closing block: a late document resolved, or a reorg moved or dropped a posting;
//! - **sealed**, the day's closing block was past the nest's `sealed_through` when it was computed and
//!   is covered now, so this is the last answer a reorg could have changed.
//!
//! A day settles, and is not probed again until a registry change reopens it, only on evidence from its
//! probe ([`settles`]): sealing covers its closing block, every document its postings name is stored,
//! and the publisher has posted a bucket starting at or after the day's end. A document the resolver
//! gave up on is never stored, so its day stays open and keeps being probed. `as_of` is stored but
//! never compared, because it moves with every block whether or not the day did.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use futures::future::BoxFuture;
use kittiwake_db::{Pool, QosAllocationDay, QosDayState, QosIndexerDay};
use kittiwake_nest::{Lane, NestGate, SqlAnswer};
use kittiwake_read::sql;
use kittiwake_types::{NestId, Provenance};
use serde_json::Value;

use crate::{IngestError, Ingested, i, page, s};

/// The window the Query Performance card draws, today included.
pub const WINDOW_DAYS: i64 = 90;

/// Where computed days go. Postgres in production; a map in the tests.
pub trait QosDayStore: Send + Sync {
    fn states(&self, from: i64, to: i64) -> BoxFuture<'_, Result<Vec<QosDayState>, IngestError>>;
    fn write<'a>(
        &'a self,
        state: &'a QosDayState,
        indexers: &'a [QosIndexerDay],
        allocations: &'a [QosAllocationDay],
    ) -> BoxFuture<'a, Result<u64, IngestError>>;
    fn settle(&self, day: i64) -> BoxFuture<'_, Result<(), IngestError>>;
}

impl QosDayStore for Pool {
    fn states(&self, from: i64, to: i64) -> BoxFuture<'_, Result<Vec<QosDayState>, IngestError>> {
        Box::pin(async move { Ok(kittiwake_db::qos_day_states(self, from, to).await?) })
    }
    fn write<'a>(
        &'a self,
        state: &'a QosDayState,
        indexers: &'a [QosIndexerDay],
        allocations: &'a [QosAllocationDay],
    ) -> BoxFuture<'a, Result<u64, IngestError>> {
        Box::pin(async move {
            Ok(kittiwake_db::write_qos_day(self, state, indexers, allocations).await?)
        })
    }
    fn settle(&self, day: i64) -> BoxFuture<'_, Result<(), IngestError>> {
        Box::pin(async move { Ok(kittiwake_db::settle_qos_day(self, day).await?) })
    }
}

/// Why a day is being computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recompute {
    Today,
    Missing,
    RegistryChanged,
    DocumentsChanged,
    Sealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Compute(Recompute),
    /// Unsettled: spend a one-day probe to see whether it changed.
    Probe,
    Skip,
}

/// What a one-day probe found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DayProbe {
    /// Buckets whose document is stored, per topic.
    pub indexer_buckets: i64,
    pub query_buckets: i64,
    /// Postings for the day whose document is not stored, given-up documents included.
    pub unresolved_documents: i64,
    /// The highest block holding one of the day's postings.
    pub closing_block: Option<i64>,
    /// The newest bucket the publisher has posted, for any day.
    pub newest_posted_bucket_start: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterProbe {
    Compute(Recompute),
    Settle,
    Wait,
}

/// Decide a day from its stored state and the nest's provenance now, before any probe.
pub fn decide(
    day: i64,
    today: i64,
    stored: Option<&QosDayState>,
    current: &Provenance,
) -> Decision {
    if day == today {
        return Decision::Compute(Recompute::Today);
    }
    let Some(s) = stored else {
        return Decision::Compute(Recompute::Missing);
    };
    if s.registry_hash != current.registry_hash || s.nid != current.nid {
        return Decision::Compute(Recompute::RegistryChanged);
    }
    if s.settled {
        Decision::Skip
    } else {
        Decision::Probe
    }
}

/// A day with no documents has nothing a reorg can reach, so it counts as sealed.
fn sealed(closing_block: Option<i64>, sealed_through: Option<i64>) -> bool {
    closing_block.is_none_or(|c| sealed_through.is_some_and(|t| t >= c))
}

/// Whether a day can no longer change, on the evidence of its probe: sealing covers its closing block,
/// no posting for it is waiting on a document, and the publisher has posted past its end, so no bucket
/// of it is still to come. A day inside a publisher outage fails the last and stays open.
pub fn settles(day: i64, probe: &DayProbe, sealed_through: Option<i64>) -> bool {
    sealed(probe.closing_block, sealed_through)
        && probe.unresolved_documents == 0
        && probe
            .newest_posted_bucket_start
            .is_some_and(|b| b >= (day + 1) * 86_400)
}

/// Decide an unsettled day from what the probe found.
pub fn after_probe(stored: &QosDayState, probe: &DayProbe, current: &Provenance) -> AfterProbe {
    if probe.indexer_buckets != stored.indexer_buckets
        || probe.query_buckets != stored.query_buckets
        || probe.closing_block != stored.closing_block
    {
        return AfterProbe::Compute(Recompute::DocumentsChanged);
    }
    let sealed_then = sealed(stored.closing_block, stored.sealed_through);
    if !sealed_then && sealed(stored.closing_block, current.sealed_through) {
        return AfterProbe::Compute(Recompute::Sealed);
    }
    if settles(stored.day, probe, current.sealed_through) {
        AfterProbe::Settle
    } else {
        AfterProbe::Wait
    }
}

/// Fill every day in the window that is missing, today, or changed.
pub async fn fill_qos_days<S: QosDayStore>(
    store: &S,
    gate: &NestGate,
    lane: Lane,
    now: i64,
) -> Result<Ingested, IngestError> {
    let today = now.div_euclid(86_400);
    let from = today - (WINDOW_DAYS - 1);
    let current = page(
        gate,
        NestId::Qos,
        sql::QOS_PROVENANCE_SQL.into(),
        lane,
        "QoS provenance",
    )
    .await?
    .provenance;
    let stored: BTreeMap<i64, QosDayState> = store
        .states(from, today)
        .await?
        .into_iter()
        .map(|s| (s.day, s))
        .collect();

    let mut rows = 0;
    for day in from..=today {
        let reason = match decide(day, today, stored.get(&day), &current) {
            Decision::Skip => continue,
            Decision::Compute(r) => r,
            Decision::Probe => {
                let Some(state) = stored.get(&day) else {
                    continue;
                };
                let (probe, probe_prov) = probe_day(gate, lane, day).await?;
                match after_probe(state, &probe, &probe_prov) {
                    AfterProbe::Compute(r) => r,
                    AfterProbe::Settle => {
                        store.settle(day).await?;
                        continue;
                    }
                    AfterProbe::Wait => continue,
                }
            }
        };
        tracing::info!(day, ?reason, "computing a QoS day");
        rows += compute_day(store, gate, lane, day).await?;
    }
    Ok(Ingested { rows })
}

fn f(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(x) => x.parse().ok(),
        _ => None,
    }
}

async fn probe_day(
    gate: &NestGate,
    lane: Lane,
    day: i64,
) -> Result<(DayProbe, Provenance), IngestError> {
    let answer = page(
        gate,
        NestId::Qos,
        sql::qos_day_probe_sql(day),
        lane,
        "QoS day probe",
    )
    .await?;
    let row = answer.rows.first().cloned().unwrap_or(Value::Null);
    let probe = DayProbe {
        indexer_buckets: i(&row, "indexer_buckets").unwrap_or(0),
        query_buckets: i(&row, "query_buckets").unwrap_or(0),
        unresolved_documents: i(&row, "unresolved_documents").unwrap_or(0),
        closing_block: i(&row, "closing_block"),
        newest_posted_bucket_start: i(&row, "newest_posted_bucket_start"),
    };
    Ok((probe, answer.provenance))
}

/// The oldest of several answers' provenance: a day is only as sealed as its least sealed part.
fn oldest(answers: &[&SqlAnswer]) -> Provenance {
    let min = |pick: fn(&Provenance) -> Option<i64>| {
        answers
            .iter()
            .map(|a| pick(&a.provenance))
            .try_fold(i64::MAX, |acc, v| v.map(|v| acc.min(v)))
    };
    let first = answers
        .first()
        .map(|a| a.provenance.clone())
        .unwrap_or_default();
    Provenance {
        as_of: min(|p| p.as_of),
        sealed_through: min(|p| p.sealed_through),
        ..first
    }
}

async fn compute_day<S: QosDayStore>(
    store: &S,
    gate: &NestGate,
    lane: Lane,
    day: i64,
) -> Result<u64, IngestError> {
    let (probe, probe_prov) = probe_day(gate, lane, day).await?;
    let indexer = page(
        gate,
        NestId::Qos,
        sql::qos_indexer_day_sql(day),
        lane,
        "QoS indexer day",
    )
    .await?;
    let behind = page(
        gate,
        NestId::Qos,
        sql::qos_seconds_behind_day_sql(day),
        lane,
        "QoS seconds behind",
    )
    .await?;
    let alloc = page(
        gate,
        NestId::Qos,
        sql::qos_network_rows_sql(day),
        lane,
        "QoS allocation day",
    )
    .await?;
    let probe_answer = SqlAnswer {
        rows: vec![],
        count: 0,
        provenance: probe_prov,
    };
    let provenance = oldest(&[&probe_answer, &indexer, &behind, &alloc]);

    let allocations = allocation_rows(&alloc.rows, day);
    let indexers = indexer_rows(&indexer.rows, &behind.rows, &allocations, day);
    let state = QosDayState {
        day,
        registry_hash: provenance.registry_hash.clone(),
        nid: provenance.nid.clone(),
        as_of: provenance.as_of,
        sealed_through: provenance.sealed_through,
        closing_block: probe.closing_block,
        indexer_buckets: probe.indexer_buckets,
        query_buckets: probe.query_buckets,
        settled: settles(day, &probe, provenance.sealed_through),
    };
    store.write(&state, &indexers, &allocations).await
}

/// `qos_allocation_daily` rows for one day.
pub fn allocation_rows(rows: &[Value], day: i64) -> Vec<QosAllocationDay> {
    rows.iter()
        .filter_map(|r| {
            Some(QosAllocationDay {
                indexer: s(r, "indexer")?.to_lowercase(),
                deployment: s(r, "deployment")?,
                chain: s(r, "chain").filter(|c| !c.is_empty()),
                gateway: s(r, "gateway").unwrap_or_default(),
                day,
                query_count: f(r, "query_count").unwrap_or(0.0),
                num_200: f(r, "num_200"),
                latency_ms: f(r, "latency_ms"),
                blocks_behind: f(r, "blocks_behind"),
                total_query_fees: f(r, "total_query_fees").unwrap_or(0.0),
                buckets: i(r, "buckets").unwrap_or(0),
            })
        })
        .collect()
}

/// `qos_indexer_daily` joined, in memory, to `qos_indexer_seconds_behind` and to the gateways the
/// day's allocation rows name. The nest is asked for each half separately because together they are
/// the statement that ran out of memory.
pub fn indexer_rows(
    daily: &[Value],
    behind: &[Value],
    allocations: &[QosAllocationDay],
    day: i64,
) -> Vec<QosIndexerDay> {
    let behind: HashMap<String, &Value> = behind
        .iter()
        .filter_map(|r| Some((s(r, "indexer")?.to_lowercase(), r)))
        .collect();
    let mut gateways: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for a in allocations {
        gateways
            .entry(a.indexer.as_str())
            .or_default()
            .insert(a.gateway.as_str());
    }
    daily
        .iter()
        .filter_map(|r| {
            let indexer = s(r, "indexer")?.to_lowercase();
            let b = behind.get(&indexer).copied().unwrap_or(&Value::Null);
            let gw = gateways
                .get(indexer.as_str())
                .map(|g| g.iter().map(|x| x.to_string()).collect())
                .unwrap_or_default();
            Some(QosIndexerDay {
                day,
                query_count: f(r, "query_count").unwrap_or(0.0),
                num_200: f(r, "num_200"),
                success_rate: f(r, "success_rate"),
                latency_ms: f(r, "latency_ms"),
                blocks_behind: f(r, "blocks_behind"),
                total_query_fees: f(r, "total_query_fees").unwrap_or(0.0),
                avg_query_fee: f(r, "avg_query_fee"),
                allocations: i(r, "allocations"),
                buckets: i(r, "buckets").unwrap_or(0),
                bad_buckets: i(r, "bad_buckets").unwrap_or(0),
                worst_bucket_start: i(r, "worst_bucket_start"),
                worst_bucket_success_rate: f(r, "worst_bucket_success_rate"),
                worst_bucket_queries: f(r, "worst_bucket_queries"),
                seconds_behind_peers: f(b, "seconds_behind_peers"),
                share_queries_over_5min_behind: f(b, "share_queries_over_5min_behind"),
                share_with_block_time: f(b, "share_with_block_time"),
                gateways: gw,
                indexer,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

    const TODAY: i64 = 20_709;
    const NOW: i64 = TODAY * 86_400 + 3_600;

    fn prov(registry: &str, sealed_through: i64) -> Provenance {
        Provenance {
            registry_hash: Some(registry.into()),
            sealed_through: Some(sealed_through),
            as_of: Some(sealed_through + 100),
            ..Provenance::default()
        }
    }

    fn state(day: i64, sealed_through: i64, closing: i64, settled: bool) -> QosDayState {
        QosDayState {
            day,
            registry_hash: Some("r1".into()),
            sealed_through: Some(sealed_through),
            closing_block: Some(closing),
            indexer_buckets: 288,
            query_buckets: 288,
            settled,
            ..QosDayState::default()
        }
    }

    /// A probe of a finished day: every document stored, and the publisher well into the next day.
    fn probe(closing: i64) -> DayProbe {
        DayProbe {
            indexer_buckets: 288,
            query_buckets: 288,
            unresolved_documents: 0,
            closing_block: Some(closing),
            newest_posted_bucket_start: Some(TODAY * 86_400),
        }
    }

    #[test]
    fn today_is_always_computed_and_a_missing_day_is_computed_once() {
        let settled = state(TODAY, 900, 800, true);
        assert_eq!(
            decide(TODAY, TODAY, Some(&settled), &prov("r1", 900)),
            Decision::Compute(Recompute::Today)
        );
        assert_eq!(
            decide(TODAY - 5, TODAY, None, &prov("r1", 900)),
            Decision::Compute(Recompute::Missing)
        );
    }

    #[test]
    fn a_settled_day_is_skipped_until_the_registry_changes() {
        let s = state(TODAY - 10, 900, 800, true);
        assert_eq!(
            decide(s.day, TODAY, Some(&s), &prov("r1", 5_000)),
            Decision::Skip
        );
        assert_eq!(
            decide(s.day, TODAY, Some(&s), &prov("r2", 5_000)),
            Decision::Compute(Recompute::RegistryChanged)
        );
        let moved_nid = Provenance {
            nid: Some("other".into()),
            ..prov("r1", 5_000)
        };
        assert_eq!(
            decide(s.day, TODAY, Some(&s), &moved_nid),
            Decision::Compute(Recompute::RegistryChanged)
        );
    }

    #[test]
    fn as_of_moving_changes_nothing() {
        let s = state(TODAY - 1, 700, 800, false);
        let later = Provenance {
            as_of: Some(99_999),
            ..prov("r1", 700)
        };
        assert_eq!(decide(s.day, TODAY, Some(&s), &later), Decision::Probe);
        assert_eq!(after_probe(&s, &probe(800), &later), AfterProbe::Wait);
    }

    #[test]
    fn a_late_document_or_a_moved_posting_recomputes() {
        let s = state(TODAY - 1, 900, 800, false);
        let late = DayProbe {
            indexer_buckets: 289,
            ..probe(800)
        };
        assert_eq!(
            after_probe(&s, &late, &prov("r1", 900)),
            AfterProbe::Compute(Recompute::DocumentsChanged)
        );
        assert_eq!(
            after_probe(&s, &probe(812), &prov("r1", 900)),
            AfterProbe::Compute(Recompute::DocumentsChanged)
        );
    }

    #[test]
    fn a_day_computed_unsealed_is_computed_once_more_when_sealing_covers_it() {
        let unsealed = state(TODAY - 1, 700, 800, false);
        assert_eq!(
            after_probe(&unsealed, &probe(800), &prov("r1", 799)),
            AfterProbe::Wait,
            "not sealed: open, however complete it looks"
        );
        assert_eq!(
            after_probe(&unsealed, &probe(800), &prov("r1", 800)),
            AfterProbe::Compute(Recompute::Sealed)
        );
        let sealed = state(TODAY - 1, 800, 800, false);
        assert_eq!(
            after_probe(&sealed, &probe(800), &prov("r1", 900)),
            AfterProbe::Settle
        );
    }

    /// The resolver gave up on one of the day's documents, or has not fetched it yet. Nothing about
    /// the day's age settles it.
    #[test]
    fn a_day_with_an_unresolved_document_never_settles() {
        let s = state(TODAY - 60, 800, 800, false);
        let waiting = DayProbe {
            unresolved_documents: 1,
            ..probe(800)
        };
        assert_eq!(
            after_probe(&s, &waiting, &prov("r1", 1_000_000)),
            AfterProbe::Wait
        );
        assert!(!settles(s.day, &waiting, Some(1_000_000)));
    }

    #[test]
    fn a_day_settles_once_its_last_document_arrives() {
        let day = TODAY - 3;
        let before = state(day, 800, 800, false);
        let stored_287 = QosDayState {
            indexer_buckets: 287,
            ..before.clone()
        };
        let one_missing = DayProbe {
            indexer_buckets: 287,
            unresolved_documents: 1,
            ..probe(800)
        };
        assert_eq!(
            after_probe(&stored_287, &one_missing, &prov("r1", 900)),
            AfterProbe::Wait
        );
        let arrived = probe(800);
        assert_eq!(
            after_probe(&stored_287, &arrived, &prov("r1", 900)),
            AfterProbe::Compute(Recompute::DocumentsChanged),
            "the document changes the day's figures"
        );
        assert!(
            settles(day, &arrived, Some(900)),
            "and the recompute stores it settled"
        );
        assert_eq!(
            after_probe(&before, &arrived, &prov("r1", 900)),
            AfterProbe::Settle
        );
    }

    /// The publisher's last post for the day may simply not have happened: inside an outage, the rest
    /// of the day's buckets arrive when it resumes, if ever.
    #[test]
    fn a_day_with_no_posting_past_its_end_stays_open() {
        let day = TODAY - 2;
        let s = state(day, 800, 800, false);
        let day_end = (day + 1) * 86_400;
        for newest in [None, Some(day_end - 300)] {
            let quiet = DayProbe {
                newest_posted_bucket_start: newest,
                ..probe(800)
            };
            assert_eq!(
                after_probe(&s, &quiet, &prov("r1", 900)),
                AfterProbe::Wait,
                "{newest:?}"
            );
        }
        let resumed = DayProbe {
            newest_posted_bucket_start: Some(day_end),
            ..probe(800)
        };
        assert_eq!(
            after_probe(&s, &resumed, &prov("r1", 900)),
            AfterProbe::Settle
        );
    }

    #[derive(Default)]
    struct MemStore {
        days: Mutex<BTreeMap<i64, QosDayState>>,
        written: Mutex<Vec<i64>>,
    }

    impl QosDayStore for MemStore {
        fn states(
            &self,
            from: i64,
            to: i64,
        ) -> BoxFuture<'_, Result<Vec<QosDayState>, IngestError>> {
            let out = self
                .days
                .lock()
                .unwrap()
                .range(from..=to)
                .map(|(_, v)| v.clone())
                .collect();
            Box::pin(async move { Ok(out) })
        }
        fn write<'a>(
            &'a self,
            state: &'a QosDayState,
            indexers: &'a [QosIndexerDay],
            allocations: &'a [QosAllocationDay],
        ) -> BoxFuture<'a, Result<u64, IngestError>> {
            self.days.lock().unwrap().insert(state.day, state.clone());
            self.written.lock().unwrap().push(state.day);
            let n = (indexers.len() + allocations.len()) as u64;
            Box::pin(async move { Ok(n) })
        }
        fn settle(&self, day: i64) -> BoxFuture<'_, Result<(), IngestError>> {
            if let Some(s) = self.days.lock().unwrap().get_mut(&day) {
                s.settled = true;
            }
            Box::pin(async { Ok(()) })
        }
    }

    /// A stand-in nest: sealed through block 5,000, registry `r1`, and for one day a probe that
    /// finds a document the stored state did not have.
    struct Nest {
        seen: Arc<Mutex<Vec<String>>>,
        changed_day: i64,
    }

    impl Respond for Nest {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if req.url.path().ends_with("/ready") {
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
            self.seen.lock().unwrap().push(q.clone());
            let rows = if q.contains("indexer_buckets") {
                let extra = sql::days_named(&q) == vec![self.changed_day];
                serde_json::json!([{
                    "indexer_buckets": if extra { 289 } else { 288 },
                    "query_buckets": 288, "closing_block": 800, "unresolved_documents": 0,
                    "newest_posted_bucket_start": TODAY * 86_400 + 1_800
                }])
            } else if q.contains("qos_allocation_daily") {
                serde_json::json!([{"indexer": "0xA", "deployment": "QmX", "chain": "mainnet",
                    "gateway": "0xgw", "unix_day": 0, "query_count": 10.0, "num_200": 9.0,
                    "latency_ms": 5.0, "blocks_behind": 1.0, "total_query_fees": 0.1, "buckets": 3}])
            } else if q.contains("qos_indexer_daily") {
                serde_json::json!([{"indexer": "0xa", "query_count": 10.0, "num_200": 9.0,
                    "success_rate": 0.9, "buckets": 3, "bad_buckets": 0, "total_query_fees": 0.1}])
            } else {
                serde_json::json!([])
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "count": rows.as_array().map(Vec::len).unwrap_or(0), "rows": rows,
                "provenance": {"registry_hash": "r1", "sealed_through": 5000, "as_of": 5100}
            }))
        }
    }

    async fn run(store: &MemStore, changed_day: i64) -> Vec<String> {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("GET"))
            .respond_with(Nest {
                seen: seen.clone(),
                changed_day,
            })
            .mount(&server)
            .await;
        let mut cfg = kittiwake_nest::NestConfig::new(server.uri());
        for n in NestId::ALL {
            cfg.base_paths.insert(n, format!("/{}", n.as_str()));
        }
        let gate = NestGate::new(cfg).unwrap();
        fill_qos_days(store, &gate, Lane::Cron, NOW)
            .await
            .expect("the fill runs");
        seen.lock().unwrap().clone()
    }

    fn settled_window(store: &MemStore) {
        let mut days = store.days.lock().unwrap();
        for day in TODAY - (WINDOW_DAYS - 1)..TODAY {
            days.insert(day, state(day, 5_000, 800, true));
        }
    }

    #[tokio::test]
    async fn a_settled_window_asks_the_nest_only_about_today() {
        let store = MemStore::default();
        settled_window(&store);
        let seen = run(&store, -1).await;
        assert_eq!(*store.written.lock().unwrap(), vec![TODAY]);
        let days: BTreeSet<i64> = seen.iter().flat_map(|q| sql::days_named(q)).collect();
        assert_eq!(days, BTreeSet::from([TODAY]), "{seen:#?}");
    }

    #[tokio::test]
    async fn a_changed_day_is_recomputed_and_no_other() {
        let store = MemStore::default();
        settled_window(&store);
        let changed = TODAY - 1;
        store
            .days
            .lock()
            .unwrap()
            .insert(changed, state(changed, 5_000, 800, false));
        store
            .days
            .lock()
            .unwrap()
            .insert(TODAY - 2, state(TODAY - 2, 5_000, 800, false));
        run(&store, changed).await;
        assert_eq!(*store.written.lock().unwrap(), vec![changed, TODAY]);
        assert!(
            store.days.lock().unwrap()[&(TODAY - 2)].settled,
            "unchanged, sealed, resolved and posted past: settled on the evidence, not rewritten"
        );
    }

    #[tokio::test]
    async fn an_empty_store_computes_every_day_of_the_window_and_no_statement_spans_two() {
        let store = MemStore::default();
        let seen = run(&store, -1).await;
        assert_eq!(store.written.lock().unwrap().len(), WINDOW_DAYS as usize);
        for q in &seen {
            assert!(sql::days_named(q).len() <= 1, "spans days: {q}");
            assert!(
                !q.contains("BETWEEN") && !q.contains("day >") && !q.contains("day <"),
                "a range: {q}"
            );
        }
        let stored = store.days.lock().unwrap();
        assert!(
            stored[&(TODAY - 10)].settled,
            "sealed and old: settled on first compute"
        );
        assert!(!stored[&TODAY].settled);
    }

    #[test]
    fn a_days_rows_carry_the_gateways_its_allocations_name() {
        let alloc = allocation_rows(
            &[
                serde_json::json!({"indexer": "0xA", "deployment": "Qm1", "gateway": "0xg2", "query_count": 1.0, "buckets": 1}),
                serde_json::json!({"indexer": "0xa", "deployment": "Qm2", "gateway": "0xg1", "chain": "", "query_count": 1.0, "buckets": 1}),
            ],
            TODAY,
        );
        assert_eq!(alloc[1].chain, None);
        let rows = indexer_rows(
            &[
                serde_json::json!({"indexer": "0xA", "query_count": 2.0, "buckets": 1, "bad_buckets": 0}),
            ],
            &[serde_json::json!({"indexer": "0xa", "seconds_behind_peers": 12.0})],
            &alloc,
            TODAY,
        );
        assert_eq!(rows[0].indexer, "0xa");
        assert_eq!(rows[0].gateways, vec!["0xg1", "0xg2"]);
        assert_eq!(rows[0].seconds_behind_peers, Some(12.0));
    }
}
