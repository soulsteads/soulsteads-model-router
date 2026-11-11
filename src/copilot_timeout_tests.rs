use super::*;
use futures_util::StreamExt;
use std::{convert::Infallible, future::pending};

fn clock(hard: Duration) -> crate::sse::ProgressClock {
    crate::sse::ProgressClock::new(tokio::time::Instant::now(), hard, None, None)
}

fn stalled_response(initial: Option<&'static [u8]>) -> reqwest::Response {
    let stream = async_stream::stream! {
        if let Some(bytes) = initial {
            yield Ok::<_, Infallible>(Bytes::from_static(bytes));
        }
        pending::<()>().await;
    };
    axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .unwrap()
        .into()
}

#[tokio::test]
async fn stalled_header_or_auth_future_obeys_deadline() {
    let deadline = clock(Duration::from_millis(20));
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        deadline::wait(&deadline, pending::<()>()),
    )
    .await;
    assert!(
        result.is_ok(),
        "stalled headers/auth must finish at the application deadline"
    );
    assert_eq!(result.unwrap(), Err("deadline_exceeded"));
}

#[tokio::test]
async fn stalled_body_times_out_and_releases_capacity_before_error_delivery() {
    for initial in [None, Some(b"data: synthetic\n\n".as_slice())] {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let response = stream_response(
            stalled_response(initial),
            permit,
            clock(Duration::from_millis(20)),
            CopilotEndpoint::ChatCompletions,
        );
        let mut body = response.into_body().into_data_stream();
        if initial.is_some() {
            body.next().await.unwrap().unwrap();
        }
        let result = tokio::time::timeout(Duration::from_millis(200), body.next()).await;
        assert!(
            result.is_ok(),
            "stalled body must terminate without client cancellation"
        );
        assert_eq!(
            result.unwrap().unwrap().unwrap_err().to_string(),
            "deadline_exceeded"
        );
        assert_eq!(
            semaphore.available_permits(),
            1,
            "release before yielding the error, not on the next poll"
        );
        assert!(body.next().await.is_none());
    }
}

#[tokio::test]
async fn body_cancellation_without_polling_releases_capacity_offline() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = semaphore.clone().acquire_owned().await.unwrap();
    let response = stream_response(
        stalled_response(None),
        permit,
        clock(Duration::from_secs(1)),
        CopilotEndpoint::Responses,
    );
    assert_eq!(semaphore.available_permits(), 0);
    drop(response);
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test]
async fn first_output_deadline_includes_header_wait_and_ignores_heartbeats() {
    for endpoint in [CopilotEndpoint::ChatCompletions, CopilotEndpoint::Responses] {
        let clock = crate::sse::ProgressClock::new(
            tokio::time::Instant::now() - Duration::from_millis(950),
            Duration::from_secs(5),
            Some(1),
            Some(1),
        );
        let stream = async_stream::stream! {
            loop {
                yield Ok::<_, Infallible>(Bytes::from_static(b": heartbeat\n\ndata: {\"usage\":{}}\n\n"));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let upstream: reqwest::Response = axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap()
            .into();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let response = stream_response(upstream, permit, clock, endpoint);
        let error = tokio::time::timeout(
            Duration::from_millis(300),
            axum::body::to_bytes(response.into_body(), 65536),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.to_string(), "first_output_timeout");
        assert_eq!(semaphore.available_permits(), 1);
    }
}

#[tokio::test]
async fn meaningful_split_events_disarm_first_output_then_obey_idle_budget() {
    for (endpoint, event) in [
        (
            CopilotEndpoint::ChatCompletions,
            "data: {\"choices\":[{\"delta\":{\"content\":\"synthetic\"}}]}\n\n",
        ),
        (
            CopilotEndpoint::Responses,
            "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"synthetic\"}\n\n",
        ),
    ] {
        let stream = async_stream::stream! {
            for chunk in event.as_bytes().chunks(7) {
                yield Ok::<_, Infallible>(Bytes::copy_from_slice(chunk));
            }
            pending::<()>().await;
        };
        let upstream: reqwest::Response = axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap()
            .into();
        let clock = crate::sse::ProgressClock::new(
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Some(1),
            Some(1),
        );
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let response = stream_response(upstream, permit, clock, endpoint);
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), 65536),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.to_string(), "stream_idle_timeout");
        assert_eq!(semaphore.available_permits(), 1);
    }
}

#[tokio::test]
async fn downstream_backpressure_does_not_extend_hard_deadline() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = semaphore.clone().acquire_owned().await.unwrap();
    let response = stream_response(
        stalled_response(Some(b"data: synthetic\n\n")),
        permit,
        clock(Duration::from_millis(20)),
        CopilotEndpoint::Responses,
    );
    let mut body = response.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        body.next().await.unwrap().unwrap_err().to_string(),
        "deadline_exceeded"
    );
    assert_eq!(semaphore.available_permits(), 1);
}

struct DropProbe(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn observed_response(
    initial: bool,
    repeat: bool,
) -> (
    reqwest::Response,
    Arc<std::sync::atomic::AtomicBool>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = DropProbe(dropped.clone());
    let observed_reads = reads.clone();
    let stream = async_stream::stream! {
        let _probe = probe;
        if initial {
            loop {
                observed_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                yield Ok::<_, Infallible>(Bytes::from_static(b"data: synthetic\n\n"));
                if !repeat {
                    break;
                }
            }
        }
        pending::<()>().await;
    };
    let response = axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .unwrap()
        .into();
    (response, dropped, reads)
}

#[tokio::test]
async fn phase2_deadline_without_initial_poll_releases_upstream_and_capacity() {
    let (upstream, dropped, _) = observed_response(false, false);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let response = stream_response(
        upstream,
        semaphore.clone().acquire_owned().await.unwrap(),
        clock(Duration::from_millis(40)),
        CopilotEndpoint::ChatCompletions,
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        semaphore.available_permits(),
        1,
        "release before any body poll"
    );
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    let mut body = response.into_body().into_data_stream();
    assert_eq!(
        body.next().await.unwrap().unwrap_err().to_string(),
        "deadline_exceeded"
    );
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn phase2_deadline_after_chunk_releases_upstream_and_capacity() {
    let (upstream, dropped, _) = observed_response(true, false);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let response = stream_response(
        upstream,
        semaphore.clone().acquire_owned().await.unwrap(),
        clock(Duration::from_millis(40)),
        CopilotEndpoint::Responses,
    );
    let mut body = response.into_body().into_data_stream();
    assert_eq!(
        body.next().await.unwrap().unwrap(),
        b"data: synthetic\n\n".as_slice()
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        semaphore.available_permits(),
        1,
        "release before resume or drop"
    );
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        body.next().await.unwrap().unwrap_err().to_string(),
        "deadline_exceeded"
    );
    assert!(body.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase2_full_queue_expires_without_stale_success_and_stops_reads() {
    for consume in [false, true] {
        let (upstream, dropped, reads) = observed_response(true, true);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let response = stream_response(
            upstream,
            semaphore.clone().acquire_owned().await.unwrap(),
            clock(Duration::from_millis(150)),
            CopilotEndpoint::Responses,
        );
        let mut body = response.into_body().into_data_stream();
        if consume {
            body.next().await.unwrap().unwrap();
        }
        let expected_reads = if consume { 2 } else { 1 };
        tokio::time::timeout(Duration::from_millis(100), async {
            while reads.load(std::sync::atomic::Ordering::SeqCst) < expected_reads {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            expected_reads,
            "bounded one-chunk read-ahead"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(semaphore.available_permits(), 1);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        let replacement = semaphore
            .clone()
            .try_acquire_owned()
            .expect("shared class capacity usable while old body retained");
        assert_eq!(
            body.next().await.unwrap().unwrap_err().to_string(),
            "deadline_exceeded"
        );
        assert!(body.next().await.is_none());
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            expected_reads
        );
        assert_eq!(
            semaphore.available_permits(),
            0,
            "old body cannot release replacement request capacity"
        );
        drop(replacement);
        assert_eq!(semaphore.available_permits(), 1);
    }
}

#[test]
fn phase2_refusal_classification_excludes_empty_heartbeat_and_usage() {
    assert_eq!(
        crate::audit::meaningful(
            &json!({"choices":[{"delta":{"refusal":"synthetic"}}]}),
            false
        ),
        Some("text")
    );
    for value in [
        json!({"choices":[{"delta":{"refusal":""}}]}),
        json!({"choices":[{"delta":{"refusal":null}}]}),
        json!({"choices":[{"delta":{"role":"assistant"}}]}),
        json!({"choices":[],"usage":{"completion_tokens":1}}),
        json!({"type":"heartbeat"}),
    ] {
        assert_eq!(crate::audit::meaningful(&value, false), None);
    }
}

#[tokio::test]
async fn phase2_refusal_disarms_first_output_with_actual_consumption() {
    let event = b"data: {\"choices\":[{\"delta\":{\"refusal\":\"synthetic\"}}]}\n\n";
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Bytes::from_static(event));
        tokio::time::sleep(Duration::from_millis(150)).await;
    };
    let upstream = axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .unwrap()
        .into();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let response = stream_response(
        upstream,
        semaphore.clone().acquire_owned().await.unwrap(),
        crate::sse::ProgressClock::new(
            tokio::time::Instant::now() - Duration::from_millis(900),
            Duration::from_secs(5),
            Some(1),
            Some(1),
        ),
        CopilotEndpoint::ChatCompletions,
    );
    let bytes = tokio::time::timeout(
        Duration::from_secs(1),
        axum::body::to_bytes(response.into_body(), 65536),
    )
    .await
    .unwrap()
    .expect("refusal must disarm first-output timeout");
    assert_eq!(bytes, event.as_slice());
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test]
async fn phase2_refusal_refreshes_armed_idle_with_actual_consumption() {
    let refusal = b"data: {\"choices\":[{\"delta\":{\"refusal\":\"synthetic\"}}]}\n\n";
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"start\"}}]}\n\n"));
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            yield Ok(Bytes::from_static(refusal));
        }
        pending::<()>().await;
    };
    let upstream = axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .unwrap()
        .into();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let response = stream_response(
        upstream,
        semaphore.clone().acquire_owned().await.unwrap(),
        crate::sse::ProgressClock::new(
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Some(1),
            Some(1),
        ),
        CopilotEndpoint::ChatCompletions,
    );
    let mut body = response.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    for _ in 0..3 {
        let bytes = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .expect("sustained refusal must refresh the already armed idle clock");
        assert_eq!(bytes, refusal.as_slice());
    }
    let last_progress = tokio::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "stream_idle_timeout");
    assert!(last_progress.elapsed() >= Duration::from_millis(850));
    assert_eq!(semaphore.available_permits(), 1);
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn phase2_backpressure_pauses_progress_without_polling_upstream() {
    for initially_meaningful in [false, true] {
        let event = if initially_meaningful {
            b"data: {\"choices\":[{\"delta\":{\"content\":\"start\"}}]}\n\n".as_slice()
        } else {
            b": heartbeat\n\n".as_slice()
        };
        let stream = async_stream::stream! {
            yield Ok::<_, Infallible>(Bytes::from_static(event));
            tokio::time::sleep(Duration::from_millis(30)).await;
            yield Ok(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"refusal\":\"synthetic\"}}]}\n\n"));
        };
        let upstream = axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap()
            .into();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let response = stream_response(
            upstream,
            semaphore.clone().acquire_owned().await.unwrap(),
            crate::sse::ProgressClock::new(
                tokio::time::Instant::now(),
                Duration::from_secs(4),
                Some(1),
                Some(1),
            ),
            CopilotEndpoint::ChatCompletions,
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(semaphore.available_permits(), 0);
        let bytes = tokio::time::timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(response.into_body(), 65536),
        )
        .await
        .unwrap()
        .expect("downstream backpressure pauses first-output and idle budgets");
        assert!(bytes.starts_with(event));
        assert!(bytes.ends_with(b"synthetic\"}}]}\n\n"));
        assert_eq!(semaphore.available_permits(), 1);
    }
}

#[tokio::test]
async fn phase2_heartbeats_and_usage_do_not_refresh_armed_idle() {
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"start\"}}]}\n\n"));
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            yield Ok(Bytes::from_static(b": heartbeat\n\ndata: {\"choices\":[],\"usage\":{\"completion_tokens\":1}}\n\ndata: {\"choices\":[{\"delta\":{\"refusal\":\"\"}}]}\n\n"));
        }
    };
    let upstream = axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .unwrap()
        .into();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let response = stream_response(
        upstream,
        semaphore.clone().acquire_owned().await.unwrap(),
        crate::sse::ProgressClock::new(
            tokio::time::Instant::now(),
            Duration::from_secs(5),
            Some(1),
            Some(1),
        ),
        CopilotEndpoint::ChatCompletions,
    );
    let mut body = response.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_millis(1600), async {
        loop {
            if let Err(error) = body.next().await.expect("expected idle failure") {
                break error;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(error.to_string(), "stream_idle_timeout");
    assert!(started.elapsed() >= Duration::from_millis(850));
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test]
async fn phase2_cancellation_releases_capacity_synchronously_and_stops_reads() {
    for consume in [false, true] {
        let (upstream, dropped, reads) = observed_response(true, true);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let response = stream_response(
            upstream,
            semaphore.clone().acquire_owned().await.unwrap(),
            clock(Duration::from_secs(5)),
            CopilotEndpoint::Responses,
        );
        let mut body = response.into_body().into_data_stream();
        if consume {
            body.next().await.unwrap().unwrap();
        }
        drop(body);
        assert_eq!(
            semaphore.available_permits(),
            1,
            "synchronous release on drop"
        );
        tokio::time::timeout(Duration::from_millis(200), async {
            while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("upstream promptly dropped after cancellation");
        let reads_after_drop = reads.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            reads_after_drop
        );
        if !consume {
            assert_eq!(reads_after_drop, 0, "no reads after immediate client drop");
        }
    }
}

#[tokio::test]
async fn phase2_normal_eof_preserves_bytes_status_and_header_allowlist() {
    let chunks = [
        Bytes::from_static(b"\x00partial"),
        Bytes::from_static(b"\xff\n\nend"),
    ];
    let upstream = axum::http::Response::builder()
        .status(StatusCode::CREATED)
        .header("content-type", "application/octet-stream")
        .header("cache-control", "no-cache")
        .header("retry-after", "2")
        .header("x-request-id", "synthetic-request")
        .header("x-github-request-id", "synthetic-github")
        .header("set-cookie", "synthetic-private")
        .header("authorization", "synthetic-private")
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, Infallible>),
        )))
        .unwrap()
        .into();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let response = stream_response(
        upstream,
        semaphore.clone().acquire_owned().await.unwrap(),
        clock(Duration::from_secs(5)),
        CopilotEndpoint::Responses,
    );
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().len(), 5);
    for (name, value) in [
        ("content-type", "application/octet-stream"),
        ("cache-control", "no-cache"),
        ("retry-after", "2"),
        ("x-request-id", "synthetic-request"),
        ("x-github-request-id", "synthetic-github"),
    ] {
        assert_eq!(response.headers()[name], value);
    }
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert_eq!(bytes, b"\x00partial\xff\n\nend".as_slice());
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test]
async fn phase2_transport_and_decoder_errors_are_redacted_and_release_upstream() {
    for invalid_sse in [false, true] {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = DropProbe(dropped.clone());
        let stream = async_stream::stream! {
            let _probe = probe;
            if invalid_sse {
                yield Ok(Bytes::from(vec![b'x'; crate::sse::MAX_EVENT_BYTES + 1]));
            } else {
                yield Err(std::io::Error::other("PRIVATE synthetic upstream failure"));
            }
            pending::<()>().await;
        };
        let upstream = axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap()
            .into();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let response = stream_response(
            upstream,
            semaphore.clone().acquire_owned().await.unwrap(),
            crate::sse::ProgressClock::new(
                tokio::time::Instant::now(),
                Duration::from_secs(5),
                Some(1),
                Some(1),
            ),
            CopilotEndpoint::Responses,
        );
        let mut body = response.into_body().into_data_stream();
        let error = body.next().await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "Copilot response stream failed");
        assert_eq!(semaphore.available_permits(), 1);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(body.next().await.is_none());
    }
}
