//! Its own test binary: tracing caches whether a callsite is enabled across threads, so a capture
//! installed beside tests that run the router without one misses events it should see.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{StatusCode, header};
use kittiwake_edge::{Limits, router, state};
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The alert manage token travels in a GET query string and in POST bodies; neither may reach
/// a log line, at any level, on success or on failure.
#[tokio::test]
async fn no_query_string_or_body_reaches_the_log_or_the_metrics() {
    use tracing_subscriber::prelude::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let captured = Captured::default();
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_writer(captured.clone()),
            ),
    );
    let rec = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let _metrics = metrics::set_default_local_recorder(&rec);

    let foghorn = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": ID })))
        .mount(&foghorn)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/alerts/subscriptions/{ID}/read")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({ "error": "no such subscription" })),
        )
        .mount(&foghorn)
        .await;
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unreachable = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let app = |foghorn: String| {
        let gate =
            kittiwake_nest::NestGate::new(kittiwake_nest::NestConfig::new("http://127.0.0.1:9"))
                .unwrap();
        let read = kittiwake_read::ReadCtx::new(gate, kittiwake_cache::Store::new(64), 1);
        router(state(
            Limits::new(1000, 1000, 1000, Vec::new()),
            None,
            read,
            None,
            None,
            kittiwake_ext::Ipfs::default(),
            Some(foghorn),
            None,
            None,
            None,
        ))
        .layer(axum::extract::connect_info::MockConnectInfo(
            SocketAddr::from(([203, 0, 113, 9], 1)),
        ))
    };
    let get = || {
        axum::http::Request::get(format!(
            "/api/foghorn/alerts/subscriptions/{ID}?token=SECRET-IN-QUERY"
        ))
        .body(axum::body::Body::empty())
        .unwrap()
    };
    let read = || {
        axum::http::Request::post(format!("/api/foghorn/alerts/subscriptions/{ID}/read"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"token":"SECRET-IN-BODY"}"#))
            .unwrap()
    };

    assert_eq!(
        app(foghorn.uri()).oneshot(get()).await.unwrap().status(),
        StatusCode::OK
    );
    let refused = app(foghorn.uri()).oneshot(read()).await.unwrap();
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(refused.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        &bytes[..],
        br#"{"error":"no such subscription"}"#,
        "Foghorn's own answer"
    );
    let sent = foghorn.received_requests().await.unwrap();
    assert_eq!(
        &sent.last().unwrap().body[..],
        br#"{"token":"SECRET-IN-BODY"}"#
    );
    // A failure is logged by the trace layer at error level, with its span.
    assert!(
        app(unreachable.clone())
            .oneshot(get())
            .await
            .unwrap()
            .status()
            .is_server_error()
    );
    assert!(
        app(unreachable)
            .oneshot(read())
            .await
            .unwrap()
            .status()
            .is_server_error()
    );

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("/api/foghorn/{*path}"),
        "the capture saw nothing: {log}"
    );
    assert!(!log.contains("SECRET"), "{log}");
    assert!(!rec.handle().render().contains("SECRET"));
}
