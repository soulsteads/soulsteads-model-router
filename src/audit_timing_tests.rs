use super::*;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Buffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn audit_handler_cancellation_is_terminal_before_headers() {
    let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let router = axum::Router::new()
            .route(
                "/v1/responses",
                axum::routing::post(|| async {
                    std::future::pending::<()>().await;
                    "never"
                }),
            )
            .layer(axum::middleware::from_fn(correlate));
        let mut pending = Box::pin(
            router.oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        assert!(futures_util::poll!(&mut pending).is_pending());
        drop(pending);
    }
    .with_subscriber(subscriber)
    .await;
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("request handler cancelled"), "{logs}");
    assert!(!logs.contains("request body completed"));
}
#[tokio::test]
async fn audit_complete_has_timing_and_safe_endpoint() {
    let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let router = axum::Router::new()
            .route("/v1/embeddings", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(correlate));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings?private=SECRET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    let complete: serde_json::Value = logs
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["fields"]["message"] == "request body completed")
        .unwrap();
    assert!(complete["fields"]["elapsed_ms"].is_u64(), "{logs}");
    assert!(logs.contains("embeddings"));
    assert!(!logs.contains("SECRET"));
}
#[tokio::test]
async fn audit_stream_error_is_not_completion_or_cancellation() {
    let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let router = axum::Router::new()
            .route(
                "/v1/responses",
                axum::routing::post(|| async {
                    Response::new(Body::from_stream(futures_util::stream::iter([Err::<
                        bytes::Bytes,
                        _,
                    >(
                        std::io::Error::other("PRIVATE_ERROR"),
                    )])))
                }),
            )
            .layer(axum::middleware::from_fn(correlate));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .is_err()
        );
    }
    .with_subscriber(subscriber)
    .await;
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("request stream failed"), "{logs}");
    assert!(!logs.contains("request body completed"));
    assert!(!logs.contains("request body cancelled"));
    assert!(!logs.contains("PRIVATE_ERROR"));
}

#[tokio::test]
async fn audit_drop_before_poll_is_cancelled() {
    let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let router = axum::Router::new()
            .route("/v1/responses", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(correlate));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        drop(response);
    }
    .with_subscriber(subscriber)
    .await;
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("request body cancelled"), "{logs}");
    assert!(!logs.contains("request body completed"));
}
