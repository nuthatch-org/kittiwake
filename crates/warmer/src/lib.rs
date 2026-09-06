//! Keeping the expensive folds hot, so no reader ever meets one cold.
//!
//! This is the whole answer to the 23.5-second `/api/indexers`. The nest computes a whole-history
//! fold and we are not allowed to change that, so the fold is moved off the request path: it runs
//! on the dataset's own cadence, on the background lane, and every reader gets the result of a
//! fold that already happened.

use std::time::Instant;

use kittiwake_read::{ReadCtx, routes};

#[derive(Debug, Default)]
pub struct WarmReport {
    pub warmed: u64,
    pub failed: u64,
}

/// Warm the whole-list folds. Per-address profiles are deliberately not warmed wholesale: there
/// are hundreds of indexers and four permits on a nest with a two-gigabyte ceiling, so warming
/// every profile would spend the entire budget on pages nobody is reading. Following the cache's
/// own access frequency to warm only the hot ones is the next step, and wants real traffic to
/// choose N from.
pub async fn warm_all(ctx: &ReadCtx) -> WarmReport {
    let began = Instant::now();
    let mut report = WarmReport::default();

    let indexers = routes::indexers(
        ctx,
        routes::IndexersParams {
            first: 100,
            skip: 0,
            order_by: "stakedTokens".into(),
            descending: true,
        },
    )
    .await;
    tally(&mut report, "indexers", indexers.is_ok(), indexers.err());

    let network = routes::network_stats(ctx).await;
    tally(&mut report, "network-stats", network.is_ok(), network.err());

    let epochs = routes::epochs(ctx, 20).await;
    tally(&mut report, "epochs", epochs.is_ok(), epochs.err());

    metrics::histogram!("warmer_duration_seconds").record(began.elapsed().as_secs_f64());
    tracing::debug!(
        warmed = report.warmed,
        failed = report.failed,
        "warm pass complete"
    );
    report
}

fn tally(
    report: &mut WarmReport,
    route: &'static str,
    ok: bool,
    err: Option<kittiwake_types::ApiError>,
) {
    if ok {
        report.warmed += 1;
        metrics::counter!("warmer_total", "route" => route, "outcome" => "ok").increment(1);
    } else {
        report.failed += 1;
        metrics::counter!("warmer_total", "route" => route, "outcome" => "error").increment(1);
        // A failed warm is not a failed request: nobody was waiting. It matters only if it keeps
        // happening, which is what the counter is for.
        if let Some(e) = err {
            tracing::debug!(route, error = %e, "warm failed");
        }
    }
}
