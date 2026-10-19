//! Says so when the nightly backup of the lodestar database stops arriving.
//!
//! The backup box writes nothing here except, through its forced-command key, the time of the last
//! dump it verified (lodestar/scripts/lodestar-dump-stdout.sh). A stale or missing mark is a failed
//! job on /health and a message to the Telegram chat.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kittiwake_sched::{JobDef, JobOutcome};

/// A nightly job, plus the slack for one slow night.
const STALE_AFTER_SECS: i64 = 26 * 3600;
const REMIND_EVERY: Duration = Duration::from_secs(12 * 3600);

/// The age of the last verified backup, or why it is not acceptable.
pub(crate) fn check(mark: Option<&str>, now: i64) -> Result<i64, String> {
    let Some(text) = mark else {
        return Err("no backup has ever been verified".into());
    };
    let at: i64 = text
        .trim()
        .parse()
        .map_err(|_| format!("the backup mark is not a timestamp: {:?}", text.trim()))?;
    let age = now - at;
    if age > STALE_AFTER_SECS {
        return Err(format!(
            "the last verified backup is {} hours old",
            age / 3600
        ));
    }
    Ok(age)
}

pub(crate) fn job(path: String, telegram: Option<kittiwake_config::TelegramSettings>) -> JobDef {
    let last_told: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    JobDef::every(
        "watch-backup",
        Duration::from_secs(3600),
        Arc::new(move |_ctx| {
            let path = path.clone();
            let telegram = telegram.clone();
            let last_told = last_told.clone();
            Box::pin(async move {
                let mark = tokio::fs::read_to_string(&path).await.ok();
                let now = chrono::Utc::now().timestamp();
                match check(mark.as_deref(), now) {
                    Ok(_) => {
                        *last_told.lock().unwrap() = None;
                        Ok(JobOutcome { rows_written: 0 })
                    }
                    Err(why) => {
                        let due = last_told
                            .lock()
                            .unwrap()
                            .is_none_or(|t| t.elapsed() >= REMIND_EVERY);
                        if let (true, Some(t)) = (due, telegram.as_ref()) {
                            let text = format!("Lodestar database backup: {why}.");
                            if kittiwake_edge::telegram::send(t, &text).await.is_ok() {
                                *last_told.lock().unwrap() = Some(Instant::now());
                            }
                        }
                        Err(why)
                    }
                }
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn last_night_is_fine() {
        assert_eq!(check(Some(&format!("{}\n", NOW - 3600)), NOW), Ok(3600));
    }

    #[test]
    fn a_missed_night_is_not() {
        let err = check(Some(&(NOW - 30 * 3600).to_string()), NOW).unwrap_err();
        assert!(err.contains("30 hours"), "{err}");
    }

    #[test]
    fn no_mark_and_a_garbled_mark_both_fail() {
        assert!(check(None, NOW).is_err());
        assert!(check(Some("yesterday"), NOW).is_err());
    }
}
