//! Logs, metrics and the two health endpoints' underlying facts.

use metrics_exporter_prometheus::PrometheusBuilder;

/// Re-exported so the edge can hold a handle without taking its own dependency on the exporter.
pub use metrics_exporter_prometheus::PrometheusHandle;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// JSON to stdout, which journald captures. Level from `RUST_LOG`, defaulting to something that
/// says what happened without narrating every cache hit.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,kittiwake=debug,tower_http=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true),
        )
        .init();
}

/// A pull-based Prometheus registry. Grafana already reads this box, so a scrape endpoint costs
/// nothing to consume.
pub fn init_metrics() -> Result<PrometheusHandle, String> {
    PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| e.to_string())
}
