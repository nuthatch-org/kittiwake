//! The ingest scheduler: seventeen jobs that used to be HTTP requests on somebody else's timer.
//!
//! As Vercel crons they had no ordering, no overlap protection and no shared back-pressure, so two
//! of them landing on the same five-minute boundary raced for the nest's permits and one came back
//! `server busy`. Here they are jobs in a process that also owns the gate, which is what makes a
//! collision structurally impossible rather than merely unlikely.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use kittiwake_nest::{Lane, NestGate};

/// When a job fires.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Every `d`, from start-up.
    Every(Duration),
}

/// What happens when a job is still running as its next fire comes round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlap {
    /// Record the skip and do nothing. Right for a re-poller: the next tick catches up anyway,
    /// and piling a second copy on top of a slow one is how a slow job becomes an outage.
    Skip,
}

pub struct JobOutcome {
    pub rows_written: u64,
}

pub type JobBody =
    Arc<dyn Fn(JobCtx) -> BoxFuture<'static, Result<JobOutcome, String>> + Send + Sync>;

#[derive(Clone)]
pub struct JobCtx {
    pub gate: NestGate,
    pub lane: Lane,
}

pub struct JobDef {
    pub name: &'static str,
    pub schedule: Schedule,
    /// A fixed offset inside the period, so two jobs on the same boundary never fire together.
    pub stagger: Duration,
    /// Random ± on each fire, so they do not re-converge after a restart.
    pub jitter: Duration,
    pub lane: Lane,
    pub overlap: Overlap,
    pub timeout: Duration,
    pub body: JobBody,
}

impl JobDef {
    pub fn every(name: &'static str, period: Duration, body: JobBody) -> Self {
        Self {
            name,
            schedule: Schedule::Every(period),
            stagger: Duration::ZERO,
            jitter: Duration::from_secs(10),
            lane: Lane::Cron,
            overlap: Overlap::Skip,
            timeout: Duration::from_secs(300),
            body,
        }
    }

    pub fn staggered(mut self, by: Duration) -> Self {
        self.stagger = by;
        self
    }
}

/// The record of one run. Written to `cron_runs`, which is the only place a failure that predates
/// the current deployment can still be read.
#[derive(Debug, Clone)]
pub struct JobRun {
    pub job: &'static str,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration: Duration,
    pub success: bool,
    pub skipped: bool,
    pub rows_written: u64,
    pub error: Option<String>,
}

pub trait RunRecorder: Send + Sync {
    fn record(&self, run: JobRun) -> BoxFuture<'static, ()>;
}

/// For tests, and for a deployment with no database configured.
pub struct NoopRecorder;

impl RunRecorder for NoopRecorder {
    fn record(&self, _run: JobRun) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// A job this process actually started, and how often it fires.
///
/// Read by `/health`, which judges a job against its own cadence rather than against a table of
/// thresholds. That table existed, in the other repository, describing this schedule from a
/// distance and kept in step by hand; it had already drifted. Anything derived from here cannot,
/// because this is written by the act of spawning rather than beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledJob {
    /// The bare job name. `cron_runs` records it prefixed - see `kittiwake_db::CRON_RUN_PREFIX`.
    pub name: &'static str,
    pub period: Duration,
    /// Paid once at start-up, so the first run of a job is this much later than the rest.
    pub stagger: Duration,
}

static SCHEDULED: OnceLock<RwLock<Vec<ScheduledJob>>> = OnceLock::new();

fn registry() -> &'static RwLock<Vec<ScheduledJob>> {
    SCHEDULED.get_or_init(|| RwLock::new(Vec::new()))
}

/// What is running, in spawn order. Empty in a process with no scheduler, which is a true answer:
/// a read-only replica schedules nothing and has nothing to go quiet.
pub fn scheduled_jobs() -> Vec<ScheduledJob> {
    registry().read().map(|g| g.clone()).unwrap_or_default()
}

pub struct Scheduler {
    gate: NestGate,
    recorder: Arc<dyn RunRecorder>,
    running: HashMap<&'static str, Arc<AtomicBool>>,
}

impl Scheduler {
    pub fn new(gate: NestGate, recorder: Arc<dyn RunRecorder>) -> Self {
        Self {
            gate,
            recorder,
            running: HashMap::new(),
        }
    }

    /// Spawn every job on its own timer. Returns the handles so shutdown can abort them.
    pub fn spawn_all(&mut self, jobs: Vec<JobDef>) -> Vec<tokio::task::JoinHandle<()>> {
        jobs.into_iter().map(|j| self.spawn(j)).collect()
    }

    fn spawn(&mut self, job: JobDef) -> tokio::task::JoinHandle<()> {
        let flag = Arc::new(AtomicBool::new(false));
        self.running.insert(job.name, flag.clone());
        {
            let Schedule::Every(period) = &job.schedule;
            let period = *period;
            if let Ok(mut reg) = registry().write() {
                reg.retain(|j| j.name != job.name);
                reg.push(ScheduledJob {
                    name: job.name,
                    period,
                    stagger: job.stagger,
                });
            }
        }
        let gate = self.gate.clone();
        let recorder = self.recorder.clone();

        tokio::spawn(async move {
            let Schedule::Every(period) = job.schedule;
            // The stagger is paid once, up front: two jobs on the same period then stay offset for
            // the life of the process instead of colliding on every boundary.
            if !job.stagger.is_zero() {
                tokio::time::sleep(job.stagger).await;
            }
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                ticker.tick().await;
                if !job.jitter.is_zero() {
                    tokio::time::sleep(jitter_for(job.jitter)).await;
                }
                run_once(&job, &gate, &recorder, &flag).await;
            }
        })
    }
}

/// A random offset in `[0, jitter)`.
fn jitter_for(jitter: Duration) -> Duration {
    let millis = jitter.as_millis().max(1) as u64;
    Duration::from_millis(rand::random::<u64>() % millis)
}

async fn run_once(
    job: &JobDef,
    gate: &NestGate,
    recorder: &Arc<dyn RunRecorder>,
    flag: &Arc<AtomicBool>,
) {
    let started_at = chrono::Utc::now();

    // `swap` rather than load-then-store: two fires arriving together must not both see `false`.
    if flag.swap(true, Ordering::SeqCst) {
        metrics::counter!("cron_runs_total", "job" => job.name, "status" => "skipped").increment(1);
        recorder
            .record(JobRun {
                job: job.name,
                started_at,
                duration: Duration::ZERO,
                success: true,
                skipped: true,
                rows_written: 0,
                error: Some("previous run still in flight".into()),
            })
            .await;
        return;
    }

    let began = std::time::Instant::now();
    let ctx = JobCtx {
        gate: gate.clone(),
        lane: job.lane,
    };
    let result = tokio::time::timeout(job.timeout, (job.body)(ctx)).await;
    flag.store(false, Ordering::SeqCst);

    let (success, rows, error) = match result {
        Ok(Ok(outcome)) => (true, outcome.rows_written, None),
        Ok(Err(e)) => (false, 0, Some(e)),
        Err(_) => (false, 0, Some(format!("timed out after {:?}", job.timeout))),
    };

    let status = if success { "ok" } else { "error" };
    metrics::counter!("cron_runs_total", "job" => job.name, "status" => status).increment(1);
    metrics::histogram!("cron_duration_seconds", "job" => job.name)
        .record(began.elapsed().as_secs_f64());

    if let Some(ref e) = error {
        tracing::warn!(job = job.name, error = %e, "cron run failed");
    }

    recorder
        .record(JobRun {
            job: job.name,
            started_at,
            duration: began.elapsed(),
            success,
            skipped: false,
            rows_written: rows,
            error,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collect(Arc<Mutex<Vec<JobRun>>>);

    impl RunRecorder for Collect {
        fn record(&self, run: JobRun) -> BoxFuture<'static, ()> {
            self.0.lock().unwrap().push(run);
            Box::pin(async {})
        }
    }

    fn gate() -> NestGate {
        NestGate::new(kittiwake_nest::NestConfig::new("http://127.0.0.1:1")).unwrap()
    }

    /// An empty registry is a real state, not an error: a read-only replica schedules nothing. It
    /// must be distinguishable from a healthy fleet though, which is why `/health` publishes the
    /// count beside the verdict rather than only an empty list. What this pins is that the count
    /// can only ever come from spawning.
    ///
    /// Asserted on this job rather than on a count delta: the registry is process-wide and the
    /// other tests spawn into it in parallel, so a delta is a race. The first draft of this test
    /// had exactly that bug.
    #[tokio::test]
    async fn only_spawning_puts_a_job_in_the_registry() {
        const NAME: &str = "a-job-that-only-this-test-spawns";
        assert!(
            !scheduled_jobs().iter().any(|j| j.name == NAME),
            "nothing may be in the registry before it is spawned"
        );

        let recorder: Arc<dyn RunRecorder> = Arc::new(Collect::default());
        let mut sched = Scheduler::new(gate(), recorder);
        let handles = sched.spawn_all(vec![JobDef::every(
            NAME,
            Duration::from_secs(300),
            Arc::new(|_| Box::pin(async { Ok(JobOutcome { rows_written: 0 }) })),
        )]);

        assert!(
            scheduled_jobs().iter().any(|j| j.name == NAME),
            "spawning must be what puts it there"
        );
        for h in handles {
            h.abort();
        }
    }

    /// `/health` judges a job by the period recorded here, so a job that is running and not
    /// registered is a job nothing can tell has stopped. Asserting on this one job rather than on
    /// the whole list, because the registry is process-wide and the other tests spawn into it too.
    #[tokio::test]
    async fn spawning_a_job_records_its_cadence() {
        let recorder: Arc<dyn RunRecorder> = Arc::new(Collect::default());
        let mut sched = Scheduler::new(gate(), recorder);
        let handles = sched.spawn_all(vec![
            JobDef::every(
                "a-test-job-with-a-name-nothing-else-uses",
                Duration::from_secs(900),
                Arc::new(|_| Box::pin(async { Ok(JobOutcome { rows_written: 0 }) })),
            )
            .staggered(Duration::from_secs(60)),
        ]);

        let found = scheduled_jobs()
            .into_iter()
            .find(|j| j.name == "a-test-job-with-a-name-nothing-else-uses")
            .expect("a spawned job must appear in the registry");
        assert_eq!(found.period, Duration::from_secs(900));
        assert_eq!(found.stagger, Duration::from_secs(60));

        for h in handles {
            h.abort();
        }
    }

    /// A slow job must not have a second copy piled on top of it.
    #[tokio::test]
    async fn a_run_still_in_flight_is_skipped_not_doubled() {
        let runs = Arc::new(Mutex::new(Vec::new()));
        let recorder: Arc<dyn RunRecorder> = Arc::new(Collect(runs.clone()));
        let flag = Arc::new(AtomicBool::new(false));

        let job = JobDef::every(
            "slow",
            Duration::from_secs(60),
            Arc::new(|_ctx| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    Ok(JobOutcome { rows_written: 7 })
                })
            }),
        );

        let g = gate();
        let (a, b) = tokio::join!(run_once(&job, &g, &recorder, &flag), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            run_once(&job, &g, &recorder, &flag).await;
        });
        let _ = (a, b);

        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs.iter().filter(|r| r.skipped).count(),
            1,
            "one fire must be skipped"
        );
        let done = runs.iter().find(|r| !r.skipped).unwrap();
        assert!(done.success);
        assert_eq!(done.rows_written, 7);
    }

    #[tokio::test]
    async fn a_failing_job_is_recorded_rather_than_swallowed() {
        let runs = Arc::new(Mutex::new(Vec::new()));
        let recorder: Arc<dyn RunRecorder> = Arc::new(Collect(runs.clone()));
        let flag = Arc::new(AtomicBool::new(false));
        let job = JobDef::every(
            "broken",
            Duration::from_secs(60),
            Arc::new(|_| Box::pin(async { Err("nest unready: stalled".to_string()) })),
        );

        run_once(&job, &gate(), &recorder, &flag).await;

        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].success);
        assert_eq!(runs[0].error.as_deref(), Some("nest unready: stalled"));
    }

    #[tokio::test]
    async fn a_job_that_hangs_is_cut_off_and_the_flag_released() {
        let runs = Arc::new(Mutex::new(Vec::new()));
        let recorder: Arc<dyn RunRecorder> = Arc::new(Collect(runs.clone()));
        let flag = Arc::new(AtomicBool::new(false));
        let mut job = JobDef::every(
            "hangs",
            Duration::from_secs(60),
            Arc::new(|_| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(JobOutcome { rows_written: 0 })
                })
            }),
        );
        job.timeout = Duration::from_millis(50);

        run_once(&job, &gate(), &recorder, &flag).await;

        assert!(
            !flag.load(Ordering::SeqCst),
            "a timed-out job must not wedge its own overlap guard"
        );
        let runs = runs.lock().unwrap();
        assert!(runs[0].error.as_deref().unwrap().contains("timed out"));
    }

    #[test]
    fn jitter_stays_inside_its_window() {
        for _ in 0..500 {
            assert!(jitter_for(Duration::from_secs(10)) < Duration::from_secs(10));
        }
    }

    #[test]
    fn every_job_can_be_given_its_own_offset() {
        let body: JobBody = Arc::new(|_| Box::pin(async { Ok(JobOutcome { rows_written: 0 }) }));
        let a = JobDef::every("refresh", Duration::from_secs(300), body.clone());
        let b = JobDef::every("snapshot-network", Duration::from_secs(300), body)
            .staggered(Duration::from_secs(90));
        assert_ne!(
            a.stagger, b.stagger,
            "same period must not mean same firing moment"
        );
    }
}
