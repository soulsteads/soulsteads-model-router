use super::*;

#[path = "stream_progress_integration_tests.rs"]
mod stream_progress_integration_tests;

#[path = "qualification_load_tests.rs"]
mod qualification_load_tests;

#[tokio::test]
async fn remediation_nonstream_choice_errors_fail_over_without_exposure() {
    for choice in [
        json!({"error":{"message":"private-upstream-text"},"message":{"content":"ok"}}),
        json!({"message":{"error":{"message":"private-upstream-text"},"content":"ok"}}),
    ] {
        let routes = vec![
            mock_route(json!({"model":"fixture","choices":[choice]})).await,
            mock_route(json!({"model":"fixture","choices":[{"message":{"content":"safe"}}]})).await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("private-upstream-text"));
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 1);
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn remediation_stream_errors_and_premature_done_fail_without_replay() {
    for bytes in [
        "data:{\"choices\":[{\"error\":{\"message\":\"private-upstream-text\"},\"delta\":{}}]}\n\ndata:[DONE]\n\n",
        "data:{\"choices\":[{\"message\":{\"error\":{\"message\":\"private-upstream-text\"}},\"delta\":{}}]}\n\ndata:[DONE]\n\n",
        "data:{\"choices\":[{\"delta\":{\"error\":{\"message\":\"private-upstream-text\"}}}]}\n\ndata:[DONE]\n\n",
        "data:[DONE]\n\n",
        ": comment\n\ndata:[DONE]\n\n",
        "data:{\"choices\":[],\"usage\":{}}\n\ndata:[DONE]\n\n",
    ] {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from(bytes),
                true,
            )
            .await,
            mock_route(json!({"model":"fixture","choices":[{"message":{"content":"replayed"}}]}))
                .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        runtime.observations.lock().unwrap()[0].failures = 2;
        let mut stream = response.into_body().into_data_stream();
        let mut failed = false;
        while let Some(chunk) = tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
        {
            match chunk {
                Ok(chunk) => {
                    let text = String::from_utf8_lossy(&chunk);
                    assert!(!text.contains("private-upstream-text"));
                    assert!(!text.contains("[DONE]"));
                }
                Err(error) => {
                    assert!(!error.to_string().contains("private-upstream-text"));
                    failed = true;
                }
            }
        }
        assert!(failed, "{bytes}");
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 1);
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            8
        );
        let observations = runtime.observations.lock().unwrap();
        assert_eq!(observations[0].failures, 3);
        assert!(observations[0].cooldown_until.is_some());
        assert!(observations[0].served.is_none());
    }
}

#[tokio::test]
async fn remediation_terminal_only_fails() {
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(b"data:[DONE]\n\n"),
            true,
        )
        .await,
    ];
    let (_dir, runtime, headers) = fixture(&routes, "chat");
    let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
    assert!(
        axum::body::to_bytes(response.into_body(), 4096)
            .await
            .is_err()
    );
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn remediation_choice_bearing_tool_and_length_streams_succeed() {
    for choice in [
        json!({"error":null,"message":{"error":null},"delta":{"tool_calls":[{"index":0,"id":"call_echo","type":"function","function":{"name":"echo","arguments":"{\"error\":\"model-generated\"}"}}]},"finish_reason":"tool_calls"}),
        json!({"delta":{},"finish_reason":"length"}),
        json!({"delta":{"content":"{\"error\":\"model-generated\"}"}}),
    ] {
        let bytes = format!("data:{}\n\ndata:[DONE]\n\n", json!({"choices":[choice]}));
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from(bytes),
                true,
            )
            .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("[DONE]"));
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 1);
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
    }
}

struct MockRoute {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    calls: Arc<AtomicU64>,
}

impl Drop for MockRoute {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_route(payload: Value) -> MockRoute {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let router = axum::Router::new().fallback(axum::routing::post(move || {
        let payload = payload.clone();
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::Relaxed);
            axum::Json(payload)
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    MockRoute {
        address,
        task,
        calls,
    }
}

fn fixture(routes: &[MockRoute], endpoint: &str) -> (tempfile::TempDir, Arc<Runtime>, HeaderMap) {
    fixture_with_policy(routes, endpoint, json!({}))
}

fn fixture_with_policy(
    routes: &[MockRoute],
    endpoint: &str,
    policy: Value,
) -> (tempfile::TempDir, Arc<Runtime>, HeaderMap) {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("fixture.key");
    fs::write(&key, "isolated-reliability-key-0123456789").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let route_configs: Vec<_> = routes.iter().enumerate().map(|(index, route)| json!({
        "name":format!("route-{index}"), "kind":if endpoint == "embedding" { "embedding" } else { "local" },
        "base_url":format!("http://{}/v1",route.address), "model":"fixture", "safe_concurrency":1,
        "context_limit":2000, "adapter_version":"mock-v1", "capabilities":if endpoint == "embedding" { json!(["text"]) } else { json!(["text","responses"]) },
        "streaming":endpoint != "embedding", "timeout_seconds":2, "enabled":true
    })).collect();
    let entries: Vec<_> = routes
        .iter()
        .enumerate()
        .map(|(index, _)| json!({"route":format!("route-{index}"),"priority":index}))
        .collect();
    let mut config = json!({"bind":"100.64.0.1","port":14022,"credentials":[{
        "name":"fixture","key_file":key,"aliases":["test"],"class":"test","max_in_flight":8,"burst":100
    }],"routes":route_configs,"aliases":{"test":{
        "endpoint":endpoint,"context_limit":2000,"capabilities":if endpoint == "embedding" { json!(["text"]) } else { json!(["text","responses"]) },
        "streaming":endpoint != "embedding","routes":entries
    }}});
    config
        .as_object_mut()
        .unwrap()
        .extend(policy.as_object().unwrap().clone());
    let path = dir.path().join("config.json");
    fs::write(&path, config.to_string()).unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let runtime = app.current();
    for observation in runtime.observations.lock().unwrap().iter_mut() {
        observation.status = RouteStatus::Serving;
        observation.checked = Some(Instant::now());
        observation.qualified = Some(Instant::now());
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer isolated-reliability-key-0123456789"
            .parse()
            .unwrap(),
    );
    (dir, runtime, headers)
}

async fn invoke(
    runtime: Arc<Runtime>,
    headers: HeaderMap,
    endpoint: &str,
    extra: Value,
) -> Response {
    let mut request = match endpoint {
        "chat" => {
            json!({"model":"test","messages":[{"role":"user","content":"hello"}],"max_tokens":16})
        }
        "responses" => json!({"model":"test","input":"hello","max_output_tokens":16}),
        _ => json!({"model":"test","input":["one","two"]}),
    };
    request
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let body = Bytes::from(request.to_string());
    match endpoint {
        "chat" => chat(State(runtime), headers, body).await,
        "responses" => responses(State(runtime), headers, body).await,
        _ => embeddings(State(runtime), headers, body).await,
    }
}

#[tokio::test]
async fn malformed_nonstream_is_safe_and_not_a_success() {
    for payload in [
        json!({"error":{"message":"private-upstream-text"},"model":"wrong"}),
        json!(7),
        json!({"model":"wrong","choices":[{"message":{"content":"ok"}}]}),
        json!({"model":"fixture","choices":[]}),
    ] {
        let routes = vec![mock_route(payload).await];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("private-upstream-text"));
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn malformed_nonstream_fails_over_before_alias_normalization() {
    let valid = json!({"model":"fixture","choices":[{"message":{"content":null,"tool_calls":[{"id":"call","type":"function","function":{"name":"echo","arguments":"{}"}}]},"finish_reason":"tool_calls"}]});
    let routes = vec![mock_route(json!(7)).await, mock_route(valid).await];
    let (_dir, runtime, headers) = fixture(&routes, "chat");
    let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["model"],
        "test"
    );
    assert_eq!(routes[1].calls.load(Ordering::Relaxed), 1);
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn responses_and_embeddings_reject_invalid_envelopes() {
    for (endpoint, payload) in [
        (
            "responses",
            json!({"object":"response","model":"fixture","status":"failed","output":[],"error":{"message":"private"}}),
        ),
        (
            "responses",
            json!({"object":"response","model":"wrong","status":"completed","output":[]}),
        ),
        (
            "embedding",
            json!({"model":"fixture","data":[{"index":0,"embedding":[0.1]}]}),
        ),
        (
            "embedding",
            json!({"model":"fixture","data":[{"index":0,"embedding":[0.1]},{"index":0,"embedding":[0.2]}]}),
        ),
    ] {
        let routes = vec![mock_route(payload).await];
        let (_dir, runtime, headers) = fixture(
            &routes,
            if endpoint == "embedding" {
                "embedding"
            } else {
                "chat"
            },
        );
        let response = invoke(runtime.clone(), headers, endpoint, json!({})).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{endpoint}");
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn nonstream_contracts_preserve_valid_endpoint_shapes() {
    for (endpoint, payload, extra) in [
        (
            "chat",
            json!({"model":"fixture","choices":[{"message":{"content":"","reasoning_content":"reasoning"},"finish_reason":"length"}]}),
            json!({}),
        ),
        (
            "responses",
            json!({"object":"response","model":"fixture","status":"incomplete","output":[],"incomplete_details":{"reason":"max_output_tokens"}}),
            json!({}),
        ),
        (
            "responses",
            json!({"object":"response","model":"fixture","status":"completed","output":[{"type":"function_call","call_id":"call","name":"echo","arguments":"{}"}]}),
            json!({}),
        ),
        (
            "embedding",
            json!({"model":"fixture","data":[{"index":1,"embedding":[0.1,0.2]},{"index":0,"embedding":[0.3,0.4]}]}),
            json!({}),
        ),
        (
            "embedding",
            json!({"model":"fixture","data":[{"index":0,"embedding":"zczMPc3MTD4="},{"index":1,"embedding":"zczMPc3MTD4="}]}),
            json!({"encoding_format":"base64"}),
        ),
    ] {
        let routes = vec![mock_route(payload).await];
        let (_dir, runtime, headers) = fixture(
            &routes,
            if endpoint == "embedding" {
                "embedding"
            } else {
                "chat"
            },
        );
        let response = invoke(runtime.clone(), headers, endpoint, extra).await;
        assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 1);
    }
}

async fn mock_http(
    status: StatusCode,
    content_type: &'static str,
    bytes: Bytes,
    hold: bool,
) -> MockRoute {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let router = axum::Router::new().fallback(axum::routing::post(move || {
        let bytes = bytes.clone();
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::Relaxed);
            let body = async_stream::stream! {
                yield Ok::<_, io::Error>(bytes);
                if hold { std::future::pending::<()>().await; }
            };
            let mut response = Response::new(Body::from_stream(body));
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    MockRoute {
        address,
        task,
        calls,
    }
}

#[tokio::test]
async fn terminal_sse_releases_permits_before_delivery_without_eof() {
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(b"data:{\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n"),
            true,
        )
        .await,
    ];
    let (_dir, runtime, headers) = fixture(&routes, "chat");
    let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    let first = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("[DONE]"));
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(
        runtime.auth.read().unwrap().credentials["fixture"]
            .class_semaphore
            .available_permits(),
        8
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn sse_framing_and_errors_are_validated_without_replay() {
    for (bytes, valid) in [
        ("data:[DONE]\r\n\r\n", false),
        (
            "data: {\r\ndata:\"model\":\"fixture\",\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\r\n\r\ndata:[DONE]\r\n\r\n",
            true,
        ),
        (
            "data:{\"error\":{\"message\":\"private-data\"}}\n\ndata:[DONE]\n\n",
            false,
        ),
        ("data:garbage\n\ndata:[DONE]\n\n", false),
        ("data:7\n\ndata:[DONE]\n\n", false),
        (
            "data:{\"model\":\"wrong\",\"choices\":[{\"delta\":{}}]}\n\ndata:[DONE]\n\n",
            false,
        ),
    ] {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from(bytes),
                true,
            )
            .await,
            mock_route(json!({"model":"fixture","choices":[{"message":{"content":"replayed"}}]}))
                .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        let body = tokio::time::timeout(
            Duration::from_millis(500),
            axum::body::to_bytes(response.into_body(), 4096),
        )
        .await
        .unwrap();
        assert_eq!(body.is_ok(), valid, "{bytes}");
        if let Ok(body) = body {
            assert!(!String::from_utf8_lossy(&body).contains("fixture"));
        }
        assert_eq!(
            runtime.route_completions[0].load(Ordering::Relaxed),
            u64::from(valid)
        );
        assert_eq!(
            runtime.upstream_errors.load(Ordering::Relaxed),
            u64::from(!valid)
        );
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn responses_sse_preserves_qualified_tails_and_incomplete_outputs() {
    for tail in [false, true] {
        for terminal in ["completed", "incomplete"] {
            let bytes = format!(
                "event:response.{terminal}\r\ndata:{{\"type\":\"response.{terminal}\",\"response\":{{\"object\":\"response\",\"model\":\"fixture\",\"status\":\"{terminal}\",\"output\":[]}}}}\r\n\r\n{}",
                if tail {
                    "event:response.done\ndata:{\"type\":\"response.done\",\"response\":{\"object\":\"response\",\"model\":\"fixture\"}}\n\n"
                } else {
                    ""
                }
            );
            let routes = vec![
                mock_http(
                    StatusCode::OK,
                    "text/event-stream",
                    Bytes::from(bytes),
                    true,
                )
                .await,
            ];
            let (dir, _runtime, headers) = fixture(&routes, "chat");
            let mut policy = config::load(&dir.path().join("config.json")).unwrap();
            policy.routes[0].config.responses_done_tail = tail;
            let runtime = make_runtime(policy, None).unwrap();
            for obs in runtime.observations.lock().unwrap().iter_mut() {
                obs.status = RouteStatus::Serving;
                obs.checked = Some(Instant::now());
                obs.qualified = Some(Instant::now());
            }
            let response = invoke(
                runtime.clone(),
                headers,
                "responses",
                json!({"stream":true}),
            )
            .await;
            let body = tokio::time::timeout(
                Duration::from_millis(500),
                axum::body::to_bytes(response.into_body(), 4096),
            )
            .await
            .unwrap()
            .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert_eq!(text.contains("[DONE]"), tail);
            assert!(!text.contains("fixture"));
            assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 1);
            assert_eq!(runtime.semaphores[0].available_permits(), 1);
        }
    }
}

#[tokio::test]
async fn retries_reach_third_distinct_route_for_every_endpoint() {
    for (endpoint, payload) in [
        (
            "chat",
            json!({"model":"fixture","choices":[{"message":{"content":"third"}}]}),
        ),
        (
            "responses",
            json!({"object":"response","model":"fixture","output":[],"status":"completed"}),
        ),
        (
            "embedding",
            json!({"model":"fixture","data":[{"index":0,"embedding":[0.1]},{"index":1,"embedding":[0.2]}]}),
        ),
    ] {
        let routes = vec![
            mock_http(
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                Bytes::from_static(b"{}"),
                false,
            )
            .await,
            mock_http(
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                Bytes::from_static(b"{}"),
                false,
            )
            .await,
            mock_route(payload).await,
        ];
        let (_dir, runtime, headers) = fixture(
            &routes,
            if endpoint == "embedding" {
                "embedding"
            } else {
                "chat"
            },
        );
        let response = invoke(runtime.clone(), headers, endpoint, json!({})).await;
        assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
        assert_eq!(
            routes
                .iter()
                .map(|route| route.calls.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            vec![1, 1, 1]
        );
        assert_eq!(runtime.route_completions[2].load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn retries_are_bounded_and_do_not_replay_client_errors() {
    for status in [StatusCode::SERVICE_UNAVAILABLE, StatusCode::BAD_REQUEST] {
        let mut routes = Vec::new();
        for _ in 0..4 {
            routes.push(
                mock_http(status, "application/json", Bytes::from_static(b"{}"), false).await,
            );
        }
        let (_dir, runtime, headers) =
            fixture_with_policy(&routes, "chat", json!({"retry_policy":{"max_attempts":3}}));
        let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
        assert_eq!(
            response.status(),
            if status == StatusCode::BAD_REQUEST {
                status
            } else {
                StatusCode::BAD_GATEWAY
            }
        );
        let expected = if status == StatusCode::BAD_REQUEST {
            vec![1, 0, 0, 0]
        } else {
            vec![1, 1, 1, 0]
        };
        assert_eq!(
            routes
                .iter()
                .map(|route| route.calls.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            runtime
                .semaphores
                .iter()
                .all(|semaphore| semaphore.available_permits() == 1)
        );
    }
}

#[tokio::test]
async fn total_deadline_covers_body_reads_for_every_endpoint() {
    for endpoint in ["chat", "responses", "embedding"] {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "application/json",
                Bytes::from_static(b"{}"),
                true,
            )
            .await,
            mock_route(json!({})).await,
        ];
        let (_dir, runtime, headers) = fixture_with_policy(
            &routes,
            if endpoint == "embedding" {
                "embedding"
            } else {
                "chat"
            },
            json!({"class_timeout_budgets":{"test":{"attempt_seconds":1,"total_seconds":1}}}),
        );
        let started = Instant::now();
        let response = invoke(runtime.clone(), headers, endpoint, json!({})).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT, "{endpoint}");
        assert!(started.elapsed() < Duration::from_millis(2500));
        assert_eq!(routes[0].calls.load(Ordering::Relaxed), 1);
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
        assert!(
            runtime
                .semaphores
                .iter()
                .all(|semaphore| semaphore.available_permits() == 1)
        );
    }
}

#[tokio::test]
async fn oversized_nonstream_body_is_bounded_and_fails_over() {
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "application/json",
            Bytes::from(vec![b'x'; MAX_RESPONSE_BYTES + 1]),
            true,
        )
        .await,
        mock_route(json!({"model":"fixture","choices":[{"message":{"content":"ok"}}]})).await,
    ];
    let (_dir, runtime, headers) = fixture(&routes, "chat");
    let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    assert_eq!(routes[1].calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn embedding_vectors_encoding_dimensions_and_indices_are_checked() {
    for (vectors, extra) in [
        (
            json!([{"index":0,"embedding":[]},{"index":1,"embedding":[]}]),
            json!({}),
        ),
        (
            json!([{"index":0,"embedding":[0.1]},{"index":2,"embedding":[0.2]}]),
            json!({}),
        ),
        (
            json!([{"index":0,"embedding":[0.1]},{"index":1,"embedding":[0.2,0.3]}]),
            json!({}),
        ),
        (
            json!([{"index":0,"embedding":["0.1"]},{"index":1,"embedding":[0.2]}]),
            json!({}),
        ),
        (
            json!([{"index":0,"embedding":"AAAAfw=="},{"index":1,"embedding":"AAAAfw=="}]),
            json!({}),
        ),
        (
            json!([{"index":0,"embedding":"AACAfw=="},{"index":1,"embedding":"AACAfw=="}]),
            json!({"encoding_format":"base64"}),
        ),
        (
            json!([{"index":0,"embedding":"AAAA"},{"index":1,"embedding":"AAAA"}]),
            json!({"encoding_format":"base64"}),
        ),
        (
            json!([{"index":0,"embedding":[0.1]},{"index":1,"embedding":[0.2]}]),
            json!({"encoding_format":"base64"}),
        ),
    ] {
        let routes = vec![mock_route(json!({"model":"fixture","data":vectors})).await];
        let (_dir, runtime, headers) = fixture(&routes, "embedding");
        assert_eq!(
            invoke(runtime.clone(), headers, "embedding", extra)
                .await
                .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    }
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn missing_capability_logs_exact_safe_enums_with_request_id() {
    let routes = vec![mock_route(json!({})).await];
    let (_dir, runtime, _headers) = fixture(&routes, "chat");
    let buffer = LogBuffer(Arc::new(Mutex::new(Vec::new())));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("request", request_id = "opaque-fixture-request-id");
        let _entered = span.enter();
        assert!(
            select_route(
                &runtime,
                "test",
                &runtime.policy.config.aliases["test"],
                false,
                100,
                RequestCapabilities {
                    image: true,
                    tools: true,
                    json_object: true,
                    ..Default::default()
                },
                &[],
                None,
                &[]
            )
            .is_none()
        );
    });
    let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    for capability in ["image", "tools", "json_object"] {
        assert!(
            logs.contains(&format!("\"missing_capability\":\"{capability}\"")),
            "{logs}"
        );
    }
    assert!(
        logs.lines()
            .all(|line| line.contains("opaque-fixture-request-id"))
    );
    assert!(!logs.contains("isolated-reliability-key"));
    assert_eq!(runtime.route_skips.lock().unwrap()[&(0, "capability")], 1);
}

#[tokio::test]
async fn nested_errors_and_event_only_responses_failures_cannot_complete() {
    for (endpoint, bytes) in [
        (
            "chat",
            "data:{\"choices\":[{\"delta\":{\"error\":{\"message\":\"private-data\"}}}]}\n\ndata:[DONE]\n\n",
        ),
        ("responses", "event:response.failed\n\ndata:[DONE]\n\n"),
        (
            "responses",
            "data:{\"type\":\"response.done\",\"response\":{\"object\":\"response\",\"model\":\"fixture\"}}\n\n",
        ),
        (
            "responses",
            "data:{\"type\":\"response.completed\",\"response\":{\"object\":\"response\",\"model\":\"wrong\"}}\n\n",
        ),
    ] {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from(bytes),
                false,
            )
            .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, endpoint, json!({"stream":true})).await;
        assert!(
            axum::body::to_bytes(response.into_body(), 4096)
                .await
                .is_err(),
            "{endpoint}"
        );
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn stream_cancellation_releases_class_and_physical_permits() {
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from_static(b"data:{\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n"),
            true,
        )
        .await,
    ];
    let (_dir, runtime, headers) = fixture(&routes, "chat");
    let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(runtime.semaphores[0].available_permits(), 0);
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

#[test]
fn response_validation_rejects_semantic_errors_and_incomplete_results() {
    for payload in [
        json!({"object":"response","model":"fixture","status":"completed","output":[{"type":"message","error":{"message":"private"},"content":[{"type":"output_text","text":"hello"}]}]}),
        json!({"object":"response","model":"fixture","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hello"},{"type":"output_text","error":{"message":"private"}}]}]}),
        json!({"object":"response","model":"fixture"}),
    ] {
        assert!(
            !super::upstream::valid_responses(&payload, "fixture", true),
            "{payload}"
        );
    }
    let control = json!({"object":"response","model":"fixture","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"error: is user text"}]}]});
    assert!(super::upstream::valid_responses(&control, "fixture", true));
}

#[tokio::test]
async fn chat_null_function_call_preserves_text_and_modern_tools_without_failover() {
    for message in [
        json!({"role":"assistant","content":"OK","tool_calls":[],"function_call":null}),
        json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_ping","type":"function","function":{"name":"ping","arguments":"{}"}}],"function_call":null}),
    ] {
        let payload = json!({"object":"chat.completion","model":"fixture","choices":[{"index":0,"message":message,"finish_reason":"stop"}]});
        let routes = vec![
            mock_route(payload.clone()).await,
            mock_route(json!({"model":"fixture","choices":[{"message":{"content":"fallback"}}]}))
                .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let actual: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(actual["choices"], payload["choices"]);
        assert_eq!(routes[0].calls.load(Ordering::Relaxed), 1);
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 1);
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
        let observations = runtime.observations.lock().unwrap();
        assert_eq!(observations[0].failures, 0);
        assert!(observations[0].cooldown_until.is_none());
    }
}

#[test]
fn chat_optional_legacy_function_call_keeps_nonnull_validation() {
    let payload = |message: Value| json!({"model":"fixture","choices":[{"message":message,"finish_reason":"stop"}]});
    for message in [
        json!({"content":"OK"}),
        json!({"content":"OK","function_call":null}),
        json!({"content":null,"function_call":{"name":"ping","arguments":"{}"}}),
    ] {
        assert!(
            super::upstream::valid_chat(&payload(message.clone()), "fixture"),
            "{message}"
        );
    }
    for function in [
        json!(false),
        json!("ping"),
        json!([]),
        json!({}),
        json!({"name":"ping"}),
        json!({"arguments":"{}"}),
        json!({"name":"bad name","arguments":"{}"}),
        json!({"name":"ping","arguments":{}}),
        json!({"name":"ping","arguments":"x".repeat(65537)}),
    ] {
        let message = json!({"content":"OK","function_call":function});
        assert!(
            !super::upstream::valid_chat(&payload(message.clone()), "fixture"),
            "{message}"
        );
    }
    for call in [
        json!({"id":"","type":"function","function":{"name":"ping","arguments":"{}"}}),
        json!({"id":"c1","type":"function","function":{"name":"ping","arguments":{}}}),
        json!({"id":"c1","type":"function","function":null}),
    ] {
        let message = json!({"content":"OK","function_call":null,"tool_calls":[call]});
        assert!(
            !super::upstream::valid_chat(&payload(message.clone()), "fixture"),
            "{message}"
        );
    }
    let semantic_error = json!({"content":"OK","function_call":null,"error":{"message":"failed"}});
    assert!(!super::upstream::valid_chat(
        &payload(semantic_error),
        "fixture"
    ));
}

#[test]
fn chat_tool_calls_require_nonempty_continuation_ids() {
    for call in [
        json!({"id":"","type":"function","function":{"name":"ping","arguments":"{}"}}),
        json!({"type":"function","function":{"name":"ping","arguments":"{}"}}),
    ] {
        let payload = json!({"model":"fixture","choices":[{"message":{"role":"assistant","tool_calls":[call]},"finish_reason":"tool_calls"}]});
        assert!(
            !super::upstream::valid_chat(&payload, "fixture"),
            "{payload}"
        );
    }
}

#[test]
fn output_tools_obey_continuation_request_bounds() {
    for call in [
        json!({"id":"x".repeat(129),"type":"function","function":{"name":"ping","arguments":"{}"}}),
        json!({"id":"c1","type":"function","function":{"name":"bad name","arguments":"{}"}}),
        json!({"id":"c1","type":"function","function":{"name":"ping","arguments":"x".repeat(65537)}}),
    ] {
        let payload = json!({"model":"fixture","choices":[{"message":{"tool_calls":[call]}}]});
        assert!(!super::upstream::valid_chat(&payload, "fixture"));
    }
}

#[tokio::test]
async fn streaming_tools_cannot_invent_missing_metadata() {
    for call in [
        json!({"index":0,"id":"c1","function":{"name":"ping","arguments":"{}"}}),
        json!({"index":0,"id":"c1","type":"function","function":{"name":"ping"}}),
    ] {
        let bytes = format!(
            "data:{}\n\ndata:[DONE]\n\n",
            json!({"choices":[{"delta":{"tool_calls":[call]}}]})
        );
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from(bytes),
                true,
            )
            .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
        assert!(
            axum::body::to_bytes(response.into_body(), 4096)
                .await
                .is_err()
        );
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
    }
}

#[tokio::test]
async fn stream_tool_assembly_has_a_global_call_limit() {
    let choices:Vec<Value>=(0..129).map(|i|json!({"index":i/128,"delta":{"tool_calls":[{"index":i%128,"id":format!("c{i}"),"type":"function","function":{"name":"ping","arguments":"{}"}}]}})).collect();
    let bytes = format!("data:{}\n\ndata:[DONE]\n\n", json!({"choices":choices}));
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "text/event-stream",
            Bytes::from(bytes),
            true,
        )
        .await,
    ];
    let (_dir, runtime, headers) = fixture(&routes, "chat");
    let response = invoke(runtime.clone(), headers, "chat", json!({"stream":true})).await;
    assert!(
        axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .is_err()
    );
    assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
}

#[test]
fn response_tools_match_responses_continuation_bounds() {
    let payload = |id: &str, args: &str| json!({"object":"response","model":"fixture","status":"completed","output":[{"type":"function_call","call_id":id,"name":"ping","arguments":args}]});
    assert!(!super::upstream::valid_responses(
        &payload(&"x".repeat(129), "{}"),
        "fixture",
        true
    ));
    assert!(super::upstream::valid_responses(
        &payload("c1", &"x".repeat(70000)),
        "fixture",
        true
    ));
    assert!(!super::upstream::valid_responses(
        &payload("c1", &"x".repeat(512 * 1024 + 1)),
        "fixture",
        true
    ));
}

#[test]
fn response_tool_identity_is_continuable() {
    let call = json!({"type":"function_call","call_id":"c1","name":"ping","arguments":"{}"});
    let mut bad_id = call.clone();
    bad_id["id"] = Value::String("x".repeat(129));
    for items in [vec![call.clone(), call], vec![bad_id]] {
        assert!(!super::upstream::valid_responses(
            &json!({"object":"response","model":"fixture","status":"completed","output":items}),
            "fixture",
            true
        ));
    }
}

#[tokio::test]
async fn responses_event_content_errors_are_rejected_before_exposure() {
    let bad = json!({"type":"message","content":[{"type":"output_text","error":{"message":"private-nested-event"}}]});
    for event in [
        json!({"type":"response.output_item.added","item":bad}),
        json!({"type":"response.created","response":{"object":"response","model":"fixture","output":[bad]}}),
    ] {
        let bytes = format!("data:{event}\n\n");
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "text/event-stream",
                Bytes::from(bytes),
                true,
            )
            .await,
        ];
        let (_dir, runtime, headers) = fixture(&routes, "chat");
        let response = invoke(
            runtime.clone(),
            headers,
            "responses",
            json!({"stream":true}),
        )
        .await;
        let mut stream = response.into_body().into_data_stream();
        assert!(stream.next().await.unwrap().is_err());
        drop(stream);
        assert_eq!(runtime.route_completions[0].load(Ordering::Relaxed), 0);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
    }
}

#[test]
fn response_tool_calls_require_complete_continuable_fields() {
    for output_item in [
        json!({"type":"function_call","name":"echo","arguments":"{}"}),
        json!({"type":"function_call","call_id":"c1","arguments":"{}"}),
        json!({"type":"function_call","call_id":"c1","name":"echo"}),
    ] {
        let payload = json!({"object":"response","model":"fixture","status":"completed","output":[output_item]});
        assert!(
            !super::upstream::valid_responses(&payload, "fixture", true),
            "{payload}"
        );
    }
}

#[tokio::test]
async fn timeout_fix_retry_floor_prevents_late_replay_and_releases_permits() {
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "application/json",
            Bytes::from_static(b"{}"),
            true,
        )
        .await,
        mock_route(json!({"model":"fixture","choices":[{"message":{"content":"ok"}}]})).await,
    ];
    let (_dir, runtime, headers) = fixture_with_policy(
        &routes,
        "chat",
        json!({"class_timeout_budgets":{"test":{"attempt_seconds":2,"total_seconds":3,"min_retry_seconds":2}}}),
    );
    let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
        "timeout_budget"
    );
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 1);
    assert_eq!(routes[1].calls.load(Ordering::Relaxed), 0);
    assert!(
        runtime
            .semaphores
            .iter()
            .all(|s| s.available_permits() == 1)
    );
    assert_eq!(
        runtime.auth.read().unwrap().credentials["fixture"]
            .class_semaphore
            .available_permits(),
        8
    );
}

#[tokio::test]
async fn timeout_fix_nonstream_deadline_preserves_existing_qualification() {
    let routes = vec![
        mock_http(
            StatusCode::OK,
            "application/json",
            Bytes::from_static(b"{}"),
            true,
        )
        .await,
    ];
    let (_dir, runtime, headers) = fixture_with_policy(
        &routes,
        "chat",
        json!({"class_timeout_budgets":{"test":{"attempt_seconds":1,"total_seconds":1}}}),
    );
    let qualified = runtime.observations.lock().unwrap()[0].qualified;
    let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    drop(response);
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].qualified, qualified);
    assert_eq!(observations[0].status, RouteStatus::Serving);
    assert_eq!(observations[0].failures, 0);
    assert!(observations[0].cooldown_until.is_none());
    assert_eq!(runtime.route_failure_events[0].load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(
        runtime.auth.read().unwrap().credentials["fixture"]
            .class_semaphore
            .available_permits(),
        8
    );
}

#[tokio::test]
async fn timeout_fix_early_failures_still_retry_with_sufficient_budget() {
    for code in [
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let routes = vec![
            mock_http(code, "application/json", Bytes::from_static(b"{}"), false).await,
            mock_route(json!({"model":"fixture","choices":[{"message":{"content":"ok"}}]})).await,
        ];
        let (_dir, runtime, headers) = fixture_with_policy(
            &routes,
            "chat",
            json!({"class_timeout_budgets":{"test":{"attempt_seconds":2,"total_seconds":3,"min_retry_seconds":2}}}),
        );
        let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        drop(response);
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            runtime.observations.lock().unwrap()[0].status,
            RouteStatus::CoolingDown
        );
        assert!(
            runtime
                .semaphores
                .iter()
                .all(|s| s.available_permits() == 1)
        );
    }
}

#[tokio::test]
async fn timeout_fix_retry_floor_is_disabled_by_default() {
    for policy in
        [json!({"class_timeout_budgets":{"test":{"attempt_seconds":1,"total_seconds":2}}})]
    {
        let routes = vec![
            mock_http(
                StatusCode::OK,
                "application/json",
                Bytes::from_static(b"{}"),
                true,
            )
            .await,
            mock_route(json!({"model":"fixture","choices":[{"message":{"content":"ok"}}]})).await,
        ];
        let (_dir, runtime, headers) = fixture_with_policy(&routes, "chat", policy);
        let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        drop(response);
        assert_eq!(routes[1].calls.load(Ordering::Relaxed), 1);
    }
}
