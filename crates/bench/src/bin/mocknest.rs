//! A stand-in nest with a configurable fold time, so a benchmark is reproducible.
//!
//! The real protocol nest recomputes a whole-history fold per request, measured at 23.5 seconds.
//! Pointing a benchmark at production would both be slow and put load on the one machine the whole
//! design exists to protect, so the fold is simulated and the figure is a parameter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

#[derive(Clone)]
struct St {
    hits: Arc<AtomicU64>,
    fold: Duration,
}

#[tokio::main]
async fn main() {
    let fold_ms: u64 = std::env::var("FOLD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9911);
    let st = St {
        hits: Arc::new(AtomicU64::new(0)),
        fold: Duration::from_millis(fold_ms),
    };

    let app = Router::new()
        .route("/{*rest}", get(handle))
        .route("/", get(handle))
        .with_state(st);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind");
    eprintln!("mock nest on 127.0.0.1:{port}, fold {fold_ms} ms");
    axum::serve(listener, app).await.expect("serve");
}

async fn handle(State(st): State<St>, uri: axum::http::Uri) -> Json<serde_json::Value> {
    let path = uri.path();
    if path.ends_with("/ready") {
        return Json(serde_json::json!({
            "ready": true, "lag_blocks": 2, "tip": 499688638i64,
            "last_block": 499688636i64, "sealed_through": 499685543i64, "stalled": false
        }));
    }
    if path == "/hits" {
        return Json(serde_json::json!({ "sql": st.hits.load(Ordering::SeqCst) }));
    }
    if path == "/reset" {
        st.hits.store(0, Ordering::SeqCst);
        return Json(serde_json::json!({ "sql": 0 }));
    }

    st.hits.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(st.fold).await;
    let rows: Vec<serde_json::Value> = (0..50)
        .map(|i| serde_json::json!({"id": format!("0x{i:040x}"), "tokens": (i * 1000).to_string(), "n": i}))
        .collect();
    Json(serde_json::json!({
        "count": rows.len(), "rows": rows,
        "provenance": {"as_of": 499688638i64, "sealed_through": 499685543i64,
                       "source": "mock", "registry_hash": "0xbfb0"}
    }))
}
