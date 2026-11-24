//! Says so in the Telegram chat when scheduled jobs keep failing, and when they recover.
//!
//! On 2026-11-23 the nests behind Helsinki's Caddy began refusing its `Host` and every job failed
//! for 33 hours. `failing_jobs` on /health listed all of it; nothing read it.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kittiwake_db::{FailingJob, Pool};
use kittiwake_sched::{JobDef, JobOutcome};

/// Three in a row, as /health counts them: one shed request is not an outage.
const MIN_FAILURES: i64 = 3;
const REMIND_EVERY: Duration = Duration::from_secs(6 * 3600);
const STEP: &str = "watch-jobs";

#[derive(Default)]
pub(crate) struct State {
    failing: BTreeSet<String>,
    last_told: Option<Instant>,
}

/// The message to send for this tick, if any, given the steps failing now.
pub(crate) fn decide(
    state: &mut State,
    now: &[FailingJob],
    remind_due: impl FnOnce(Instant) -> bool,
) -> Option<String> {
    let current: BTreeSet<String> = now
        .iter()
        .map(|j| bare(&j.step).to_string())
        .filter(|s| s != STEP)
        .collect();
    let started: Vec<&String> = current.difference(&state.failing).collect();
    let recovered: Vec<&String> = state.failing.difference(&current).collect();

    let mut lines = Vec::new();
    if !started.is_empty() || (!current.is_empty() && state.last_told.is_none_or(remind_due)) {
        lines.push(format!("Kittiwake: {} job(s) failing.", current.len()));
        for j in now.iter().filter(|j| current.contains(bare(&j.step))) {
            let err: String = j
                .last_error
                .as_deref()
                .unwrap_or("no error recorded")
                .chars()
                .take(160)
                .collect();
            lines.push(format!(
                "- {} ({} in a row): {err}",
                bare(&j.step),
                j.consecutive_failures
            ));
        }
    }
    if !recovered.is_empty() {
        let names: Vec<&str> = recovered.iter().map(|s| s.as_str()).collect();
        lines.push(format!("Kittiwake: recovered: {}.", names.join(", ")));
    }

    state.failing = current;
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

fn bare(step: &str) -> &str {
    step.strip_prefix(kittiwake_db::CRON_RUN_PREFIX)
        .unwrap_or(step)
}

pub(crate) fn job(pool: Pool, telegram: kittiwake_config::TelegramSettings) -> JobDef {
    let state: Arc<Mutex<State>> = Arc::default();
    JobDef::every(
        STEP,
        Duration::from_secs(300),
        Arc::new(move |_ctx| {
            let pool = pool.clone();
            let telegram = telegram.clone();
            let state = state.clone();
            Box::pin(async move {
                let now = kittiwake_db::failing_jobs(&pool, MIN_FAILURES)
                    .await
                    .map_err(|e| format!("reading failing jobs: {e}"))?;
                let text = {
                    let mut s = state.lock().unwrap();
                    decide(&mut s, &now, |t| t.elapsed() >= REMIND_EVERY)
                };
                if let Some(text) = text {
                    kittiwake_edge::telegram::send(&telegram, &text).await?;
                    let mut s = state.lock().unwrap();
                    if !s.failing.is_empty() {
                        s.last_told = Some(Instant::now());
                    } else {
                        s.last_told = None;
                    }
                }
                Ok(JobOutcome { rows_written: 0 })
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failing(step: &str, n: i64) -> FailingJob {
        FailingJob {
            step: format!("{}{step}", kittiwake_db::CRON_RUN_PREFIX),
            consecutive_failures: n,
            last_success: None,
            last_error: Some("nest returned HTTP 403".into()),
        }
    }

    #[test]
    fn a_new_failure_is_told_once_then_quiet_until_the_reminder() {
        let mut s = State::default();
        let now = [failing("refresh", 3)];
        let msg = decide(&mut s, &now, |_| false).unwrap();
        assert!(
            msg.contains("1 job(s) failing") && msg.contains("refresh (3 in a row)"),
            "{msg}"
        );
        s.last_told = Some(Instant::now());
        assert_eq!(decide(&mut s, &now, |_| false), None);
        assert!(decide(&mut s, &now, |_| true).is_some());
    }

    #[test]
    fn another_job_joining_is_told_and_recovery_is_told() {
        let mut s = State::default();
        decide(&mut s, &[failing("refresh", 3)], |_| false);
        s.last_told = Some(Instant::now());
        let msg = decide(
            &mut s,
            &[failing("refresh", 4), failing("ingest-rav", 3)],
            |_| false,
        )
        .unwrap();
        assert!(msg.contains("2 job(s) failing"), "{msg}");
        let msg = decide(&mut s, &[failing("ingest-rav", 4)], |_| false).unwrap();
        assert!(
            msg.contains("recovered: refresh") && !msg.contains("failing"),
            "{msg}"
        );
        let msg = decide(&mut s, &[], |_| false).unwrap();
        assert!(msg.contains("recovered: ingest-rav"), "{msg}");
        assert_eq!(decide(&mut s, &[], |_| false), None);
    }

    #[test]
    fn its_own_step_is_not_counted() {
        let mut s = State::default();
        assert_eq!(decide(&mut s, &[failing(STEP, 5)], |_| false), None);
    }
}
