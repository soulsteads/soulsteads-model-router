use super::*;

fn progress_policy(attempt: u64) -> Value {
    json!({"class_timeout_budgets":{"test":{"attempt_seconds":attempt,"total_seconds":attempt+1,"first_output_seconds":1,"stream_idle_seconds":1}}})
}

fn hard_deadline_policy() -> Value {
    json!({"class_timeout_budgets":{"test":{"attempt_seconds":1,"total_seconds":2}}})
}

const PRODUCTIVE: &[u8] = b"data:{\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\ndata:{\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\ndata:[DONE]\n\n";

#[tokio::test]
async fn progress_backpressure_pause_is_not_idle_but_hard_limit_still_applies() {
    for (attempt, succeeds) in [(3, true), (1, false)] {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from_static(PRODUCTIVE),
                true,
            )
            .await,
        ];
        let (_dir, runtime, headers) =
            fixture_with_policy(&routes, "chat", progress_policy(attempt));
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        let mut stream = response.into_body().into_data_stream();
        assert!(stream.next().await.unwrap().is_ok());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let second = stream.next().await.unwrap();
        assert_eq!(second.is_ok(), succeeds);
        if succeeds {
            assert!(stream.next().await.unwrap().is_ok());
            assert!(stream.next().await.is_none());
        }
        drop(stream);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            8
        );
        assert_eq!(
            runtime.route_completions[0].load(Ordering::Relaxed),
            u64::from(succeeds)
        );
    }
}

#[tokio::test]
async fn progress_disconnect_before_or_after_first_poll_releases_both_permits() {
    for poll in [false, true] {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from_static(PRODUCTIVE),
                true,
            )
            .await,
        ];
        let (_dir, runtime, headers) = fixture_with_policy(&routes, "chat", progress_policy(3));
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        let mut stream = response.into_body().into_data_stream();
        if poll {
            assert!(stream.next().await.unwrap().is_ok());
        }
        drop(stream);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            8
        );
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn progress_timeout_does_not_replay_or_clear_fresh_qualification() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(post(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        async {
            let body = async_stream::stream! {
                yield Ok::<Bytes, io::Error>(Bytes::from_static(b": alive\n\n"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                yield Ok(Bytes::from_static(PRODUCTIVE));
            };
            let mut response = Response::new(Body::from_stream(body));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let routes = vec![
        MockRoute {
            address,
            task,
            calls,
        },
        mock_route(json!({"model":"fixture","choices":[{"message":{"content":"replayed"}}]})).await,
    ];
    let (_dir, runtime, headers) = fixture_with_policy(&routes, "chat", progress_policy(3));
    let qualified = runtime.observations.lock().unwrap()[0].qualified;
    let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_err());
    assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(
        runtime.auth.read().unwrap().credentials["fixture"]
            .class_semaphore
            .available_permits(),
        8
    );
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].qualified, qualified);
    assert_eq!(observations[0].status, RouteStatus::Serving);
}

#[tokio::test]
async fn hard_deadline_without_progress_clock_is_local_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(post(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        async {
            let body = async_stream::stream! {
                yield Ok::<Bytes, io::Error>(Bytes::from_static(b"data:{\"choices\":[{\"delta\":{\"content\":\"productive\"}}]}\n\n"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                yield Ok(Bytes::from_static(b"data:[DONE]\n\n"));
            };
            let mut response = Response::new(Body::from_stream(body));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            response
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let routes = vec![
        MockRoute {
            address,
            task,
            calls,
        },
        mock_route(json!({"model":"fixture","choices":[{"message":{"content":"replayed"}}]})).await,
    ];
    let (_dir, runtime, headers) = fixture_with_policy(&routes, "chat", hard_deadline_policy());
    let qualified = runtime.observations.lock().unwrap()[0].qualified;
    let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "deadline_exceeded");
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 1);
    assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(
        runtime.auth.read().unwrap().credentials["fixture"]
            .class_semaphore
            .available_permits(),
        8
    );
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].qualified, qualified);
    assert_eq!(observations[0].status, RouteStatus::Serving);
}

#[tokio::test]
async fn malformed_and_transport_errors_without_progress_clock_invalidate_qualification() {
    for transport in [false, true] {
        let route = if transport {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let calls = Arc::new(AtomicU64::new(0));
            let counter = calls.clone();
            let router = Router::new().fallback(post(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                async {
                    let stream = async_stream::stream! {
                        yield Ok::<Bytes, io::Error>(Bytes::from_static(b"data:{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        yield Err(io::Error::other("synthetic transport close"));
                    };
                    let mut response = Response::new(Body::from_stream(stream));
                    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
                    response
                }
            }));
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            MockRoute {
                address,
                task,
                calls,
            }
        } else {
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from_static(b"data:invalid\n\n"),
                true,
            )
            .await
        };
        let routes = vec![route];
        let (_dir, runtime, headers) = fixture_with_policy(&routes, "chat", hard_deadline_policy());
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        let mut stream = response.into_body().into_data_stream();
        if transport {
            assert!(stream.next().await.unwrap().is_ok());
        }
        assert!(stream.next().await.unwrap().is_err());
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            8
        );
        let observations = runtime.observations.lock().unwrap();
        assert!(observations[0].qualified.is_none());
        assert_eq!(observations[0].failures, 1);
    }
}

#[tokio::test]
async fn progress_enabled_malformed_and_transport_errors_still_invalidate_qualification() {
    for transport in [false, true] {
        let route = if transport {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let calls = Arc::new(AtomicU64::new(0));
            let counter = calls.clone();
            let router = Router::new().fallback(post(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                async {
                    let stream = async_stream::stream! {
                        yield Ok::<Bytes, io::Error>(Bytes::from_static(b"data:{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        yield Err(io::Error::other("synthetic transport close"));
                    };
                    let mut response = Response::new(Body::from_stream(stream));
                    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
                    response
                }
            }));
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            MockRoute {
                address,
                task,
                calls,
            }
        } else {
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from_static(b"data:invalid\n\n"),
                true,
            )
            .await
        };
        let routes = vec![route];
        let (_dir, runtime, headers) = fixture_with_policy(&routes, "chat", progress_policy(3));
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        let mut stream = response.into_body().into_data_stream();
        if transport {
            assert!(stream.next().await.unwrap().is_ok());
        }
        assert!(stream.next().await.unwrap().is_err());
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            8
        );
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
        let observations = runtime.observations.lock().unwrap();
        assert!(observations[0].qualified.is_none());
        assert_eq!(observations[0].failures, 1);
    }
}
