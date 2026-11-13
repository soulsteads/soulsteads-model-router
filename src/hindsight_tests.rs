use super::*;

fn hindsight_config(extra: Value) -> Result<config::Config, serde_json::Error> {
    let mut value = json!({"bind":"127.0.0.1","port":4000,"routes":[],"aliases":{}});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(value)
}

#[test]
fn hindsight_disabled_configuration_is_optional() {
    assert!(hindsight_config(json!({})).is_ok());
    assert!(hindsight_config(json!({"hindsight":{"enabled":false}})).is_ok());
}

#[test]
fn hindsight_connection_and_server_owned_bank_mapping_parse() {
    assert!(
        hindsight_config(json!({"hindsight":{
            "enabled":true,"endpoint":"http://127.0.0.1:9180",
            "api_key_env":"SLOTLINE_TEST_HINDSIGHT_KEY","timeout_ms":500,
            "bindings":{"fixture":{"bank_id":"agent-fixture","allow_retain":false}}
        }}))
        .is_ok()
    );
}

#[test]
fn hindsight_inline_secrets_are_not_configuration() {
    assert!(hindsight_config(json!({"hindsight":{"api_key":"not-supported"}})).is_err());
}

use axum::response::IntoResponse;
use tower::ServiceExt;

struct MemoryMock {
    endpoint: String,
    calls: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MemoryMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn memory_stub() -> MemoryMock {
    MemoryMock {
        endpoint: "http://127.0.0.1:9".into(),
        calls: Arc::new(Mutex::new(Vec::new())),
        task: tokio::spawn(std::future::pending()),
    }
}

async fn memory_mock(
    status: StatusCode,
    content_type: &'static str,
    body: String,
    delay_ms: u64,
) -> MemoryMock {
    memory_mock_with_handler(move |path| {
        let body = body.clone();
        async move {
            let health = path == "/health";
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            Response::builder()
                .status(if health { StatusCode::OK } else { status })
                .header(
                    header::CONTENT_TYPE,
                    if health {
                        "application/json"
                    } else {
                        content_type
                    },
                )
                .header(header::LOCATION, "http://127.0.0.1:1/credential-sentinel")
                .header("x-upstream-private", "not-forwarded")
                .body(Body::from(if health { "{}".into() } else { body }))
                .unwrap()
        }
    })
    .await
}

async fn memory_mock_with_handler<Handler, Reply>(handler: Handler) -> MemoryMock
where
    Handler: Fn(String) -> Reply + Clone + Send + Sync + 'static,
    Reply: std::future::Future<Output = Response> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let captured = captured.clone();
        let handler = handler.clone();
        async move {
            let (parts, request_body) = request.into_parts();
            let path = parts.uri.path().to_owned();
            let bytes = axum::body::to_bytes(request_body, 2 * 1024 * 1024)
                .await
                .unwrap();
            let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            captured.lock().unwrap().push((
                format!("{} {}", parts.method, parts.uri),
                parts.headers,
                value,
            ));
            handler(path).await
        }
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MemoryMock {
        endpoint,
        calls,
        task,
    }
}

fn memory_fixture(mock: &MemoryMock, changes: Value) -> (tempfile::TempDir, Arc<AppState>) {
    memory_fixture_with_admission(mock, changes, 2, false)
}

fn memory_fixture_with_admission(
    mock: &MemoryMock,
    changes: Value,
    max_in_flight: usize,
    dedicated_model_credential: bool,
) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let workload_key = dir.path().join("workload.key");
    let upstream_key = dir.path().join("hindsight.key");
    for (path, value) in [
        (&workload_key, "test-workload-key-0123456789"),
        (&upstream_key, "test-upstream-key-0123456789"),
    ] {
        fs::write(path, value).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut integration = json!({"enabled":true,"endpoint":mock.endpoint,"api_key_file":upstream_key,
        "timeout_ms":200,"bindings":{"fixture":{"bank_id":"agent-fixture"}}});
    integration
        .as_object_mut()
        .unwrap()
        .extend(changes.as_object().unwrap().clone());
    let mut value = json!({"bind":"100.127.255.254","port":14022,"credentials":[{
        "name":"fixture","key_file":workload_key,"aliases":["test"],"class":"test","burst":1000,"max_in_flight":max_in_flight
    }],"routes":[{"name":"mock","kind":"local","base_url":format!("{}/v1",mock.endpoint),"model":"fixture",
        "safe_concurrency":1,"context_limit":2000,"adapter_version":"mock-v1","capabilities":["text"],"streaming":false,"timeout_seconds":2,"enabled":true}],
        "aliases":{"test":{"endpoint":"chat","context_limit":2000,"capabilities":["text"],"streaming":false,"routes":[{"route":"mock","priority":0}]}},
        "hindsight":integration});
    if dedicated_model_credential {
        let model_key = dir.path().join("model.key");
        fs::write(&model_key, "test-model-key-0123456789").unwrap();
        fs::set_permissions(&model_key, fs::Permissions::from_mode(0o600)).unwrap();
        value["credentials"].as_array_mut().unwrap().push(json!({
            "name":"hindsight-model","key_file":model_key,"aliases":["test"],
            "class":"test","burst":1000,"max_in_flight":max_in_flight
        }));
    }
    let path = dir.path().join("config.json");
    fs::write(&path, value.to_string()).unwrap();
    let app = Arc::new(AppState::new(path.clone(), config::load(&path).unwrap()).unwrap());
    (dir, app)
}

fn memory_request(operation: &str, body: Body) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(format!("/v1/memory/{operation}"))
        .header(header::AUTHORIZATION, "Bearer test-workload-key-0123456789")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

async fn memory_call(app: Arc<AppState>, operation: &str, value: Value) -> (StatusCode, Value) {
    let response = hindsight::routes()
        .with_state(app)
        .oneshot(memory_request(operation, Body::from(value.to_string())))
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn memory_model_input() -> Value {
    json!({"model":"test","messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":false})
}

fn memory_model_output() -> Value {
    json!({"model":"fixture","choices":[{"message":{"role":"assistant","content":"ordinary inference"},"finish_reason":"stop"}]})
}

fn memory_model_alias_output() -> Value {
    let mut output = memory_model_output();
    output["model"] = json!("test");
    output
}

fn memory_json_response(value: Value) -> Response {
    axum::Json(value).into_response()
}

fn memory_fixture_router(app: Arc<AppState>) -> Router {
    hindsight::routes()
        .route("/v1/chat/completions", post(current_chat))
        .with_state(app)
}

fn qualify_memory_model(app: &AppState) {
    let runtime = app.current();
    let mut observations = runtime.observations.lock().unwrap();
    observations[0].status = RouteStatus::Serving;
    observations[0].checked = Some(Instant::now());
    observations[0].qualified = Some(Instant::now());
}

async fn memory_model_call(app: Arc<AppState>) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer test-workload-key-0123456789")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(memory_model_input().to_string()))
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        memory_fixture_router(app).oneshot(request),
    )
    .await
    .expect("ordinary inference must not wait for memory")
    .unwrap();
    let status = response.status();
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .expect("model response must complete while memory is pending")
    .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn gated_memory_mock() -> (
    MemoryMock,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    Arc<Semaphore>,
) {
    let (entered, arrivals) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let mock = memory_mock_with_handler(move |path| {
        let entered = entered.clone();
        let gate = gate.clone();
        async move {
            if path == "/v1/chat/completions" {
                return memory_json_response(memory_model_output());
            }
            let output = match path.as_str() {
                "/v1/default/banks/agent-fixture/memories/recall" => json!({"results":[]}),
                "/v1/default/banks/agent-fixture/reflect" => json!({"text":"reflection"}),
                "/v1/default/banks/agent-fixture/memories" => {
                    json!({"success":true,"bank_id":"agent-fixture","items_count":1,"async":false})
                }
                _ => panic!("unexpected mock path: {path}"),
            };
            entered.send(path).unwrap();
            gate.acquire_owned().await.unwrap().forget();
            memory_json_response(output)
        }
    })
    .await;
    (mock, arrivals, release)
}

#[test]
fn hindsight_config_hygiene_and_defaults() {
    let defaults = hindsight_config(json!({})).unwrap().hindsight;
    assert!(!defaults.enabled);
    assert_eq!(defaults.timeout_ms, 10000);
    for endpoint in [
        "http://user:private@localhost",
        "http://localhost/?key=private",
        "http://localhost/#private",
        "file:///private",
        "http://localhost/prefix",
        "http://localhost/%2e%2e/private",
        " http://localhost",
        "http://localhost\\private",
    ] {
        let cfg = hindsight_config(json!({"hindsight":{"endpoint":endpoint}})).unwrap();
        let error = cfg.hindsight.validate().unwrap_err();
        assert!(!error.contains("private"));
    }
    for edit in [
        json!({"timeout_ms":0}),
        json!({"timeout_ms":120001}),
        json!({"enabled":true}),
        json!({"api_key_env":"KEY","api_key_file":"/tmp/test"}),
        json!({"api_key_file":"relative"}),
        json!({"api_key_env":"BAD=NAME"}),
        json!({"bindings":{"fixture":{"bank_id":"../escape"}}}),
    ] {
        let cfg = hindsight_config(json!({"hindsight":edit})).unwrap();
        assert!(cfg.hindsight.validate().is_err());
    }
    let cfg = hindsight_config(json!({"hindsight":{"bindings":{"fixture":{"bank_id":"agent"}}}}))
        .unwrap();
    assert!(!cfg.hindsight.bindings["fixture"].allow_retain);
}

#[tokio::test]
async fn hindsight_disabled_has_no_network_or_secret_dependency() {
    let mock = memory_stub();
    let (_dir, app) = memory_fixture(
        &mock,
        json!({"enabled":false,"api_key_file":"/does-not-exist"}),
    );
    assert_eq!(
        memory_call(app.clone(), "recall", json!({"query":"hello"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let report = hindsight::check(&app.current().policy.config.hindsight).await;
    assert_eq!(report["state"], "disabled");
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hindsight_default_deny_and_read_vs_retain() {
    let mock = memory_mock(
        StatusCode::OK,
        "application/json",
        "{\"results\":[]}".into(),
        0,
    )
    .await;
    let (_dir, app) = memory_fixture(&mock, json!({}));
    let mut request = memory_request("recall", Body::from("{\"query\":\"hello\"}"));
    request.headers_mut().remove(header::AUTHORIZATION);
    assert_eq!(
        hindsight::routes()
            .with_state(app.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        memory_call(app.clone(), "retain", json!({"items":[{"content":"text"}]}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(mock.calls.lock().unwrap().is_empty());
    assert_eq!(
        memory_call(app, "recall", json!({"query":"hello"})).await.0,
        StatusCode::OK
    );
    let (_dir, app) = memory_fixture(&mock, json!({"bindings":{}}));
    assert_eq!(
        memory_call(app, "reflect", json!({"query":"hello"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn hindsight_contract_paths_credentials_and_header_isolation() {
    for (operation, suffix, input, output) in [
        (
            "recall",
            "memories/recall",
            json!({"query":"hello","rerank":{"prune":false,"dedup":true},"tags":["owned"],"include":{"chunks":{"max_tokens":30}}}),
            json!({"results":[{"text":"authorized memory"}]}),
        ),
        (
            "reflect",
            "reflect",
            json!({"query":"hello","context":"explicit","response_schema":{"type":"object"}}),
            json!({"text":"authorized reflection","structured_output":{"answer":true}}),
        ),
        (
            "retain",
            "memories",
            json!({"items":[{"content":"text","metadata":{"source":"mock"}}],"async":true,"operation_id":"op-test"}),
            json!({"success":true,"bank_id":"agent-fixture","items_count":1,"async":true,"operation_id":"op-test"}),
        ),
    ] {
        let mock = memory_mock(
            StatusCode::ACCEPTED,
            "application/json",
            output.to_string(),
            0,
        )
        .await;
        let (_dir, app) = memory_fixture(
            &mock,
            json!({"bindings":{"fixture":{"bank_id":"agent-fixture","allow_retain":true}}}),
        );
        let mut request = memory_request(operation, Body::from(input.to_string()));
        for name in [
            "x-api-key",
            "x-bank-id",
            "x-tenant-id",
            "x-forwarded-host",
            "cookie",
            "host",
        ] {
            request
                .headers_mut()
                .insert(name, "caller-authority-sentinel".parse().unwrap());
        }
        let response = hindsight::routes()
            .with_state(app)
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(!response.headers().contains_key("x-upstream-private"));
        assert!(!response.headers().contains_key(header::LOCATION));
        let actual: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(actual, output);
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            format!("POST /v1/default/banks/agent-fixture/{suffix}")
        );
        assert_eq!(
            calls[0].1[header::AUTHORIZATION],
            "Bearer test-upstream-key-0123456789"
        );
        assert!(!format!("{:?}", calls[0].1).contains("caller-authority-sentinel"));
        assert_eq!(calls[0].2, input);
    }
}

#[tokio::test]
async fn hindsight_rejects_overrides_unknown_shapes_and_streaming() {
    let mock = memory_stub();
    let (_dir, app) = memory_fixture(
        &mock,
        json!({"bindings":{"fixture":{"bank_id":"agent","allow_retain":true}}}),
    );
    for operation in ["recall", "reflect", "retain"] {
        for field in [
            "bank",
            "bank_id",
            "tenant",
            "tenant_id",
            "endpoint",
            "api_key",
            "authorization",
            "stream",
            "stream_options",
            "extra_body",
        ] {
            let mut input = if operation == "retain" {
                json!({"items":[{"content":"text"}]})
            } else {
                json!({"query":"hello"})
            };
            input[field] = json!(false);
            assert_eq!(
                memory_call(app.clone(), operation, input).await.0,
                StatusCode::BAD_REQUEST,
                "{operation} {field}"
            );
        }
    }
    for input in [
        json!([]),
        json!({}),
        json!({"query":3}),
        json!({"query":"q","tags":true}),
        json!({"query":"q","max_tokens":-1}),
        json!({"query":"q","include":[]}),
    ] {
        assert_eq!(
            memory_call(app.clone(), "recall", input).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    for input in [
        json!({"items":[]}),
        json!({"items":[{}]}),
        json!({"items":[{"content":"x","bank_id":"evil"}]}),
        json!({"items":[{"content":[{"type":"image","url":"http://private"}]}]}),
    ] {
        assert_eq!(
            memory_call(app.clone(), "retain", input).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hindsight_missing_secret_does_not_block_inference_or_status() {
    let mock = memory_mock(StatusCode::OK, "application/json", json!({"model":"fixture","choices":[{"message":{"role":"assistant","content":"ordinary inference"},"finish_reason":"stop"}]}).to_string(), 0).await;
    let (_dir, app) = memory_fixture(&mock, json!({"api_key_file":"/does-not-exist"}));
    assert_eq!(
        memory_call(app.clone(), "recall", json!({"query":"hello"}))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        hindsight::status(&app.current().policy.config.hindsight)["state"],
        "enabled"
    );
    let runtime = app.current();
    {
        let mut observations = runtime.observations.lock().unwrap();
        observations[0].status = RouteStatus::Serving;
        observations[0].checked = Some(Instant::now());
        observations[0].qualified = Some(Instant::now());
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer test-workload-key-0123456789".parse().unwrap(),
    );
    let response = chat(State(runtime), headers, Bytes::from(json!({"model":"test","messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":false}).to_string())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        memory_model_alias_output()
    );
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "only model inference may reach upstream");
    assert_eq!(calls[0].0, "POST /v1/chat/completions");
    assert_eq!(calls[0].2["model"], "fixture");
}

#[tokio::test]
async fn hindsight_pending_memory_does_not_consume_inference_capacity() {
    for (operation, suffix, input) in [
        ("recall", "memories/recall", json!({"query":"hello"})),
        ("reflect", "reflect", json!({"query":"hello"})),
        ("retain", "memories", json!({"items":[{"content":"hello"}]})),
    ] {
        let (mock, mut arrivals, release) = gated_memory_mock().await;
        let (_dir, app) = memory_fixture_with_admission(
            &mock,
            json!({"timeout_ms":30000,"bindings":{"fixture":{"bank_id":"agent-fixture","allow_retain":true}}}),
            1,
            false,
        );
        qualify_memory_model(&app);
        let memory = tokio::spawn(memory_call(app.clone(), operation, input));
        let path = tokio::time::timeout(Duration::from_secs(5), arrivals.recv())
            .await
            .expect("memory request must reach the gated upstream")
            .unwrap();
        assert_eq!(path, format!("/v1/default/banks/agent-fixture/{suffix}"));
        assert!(!memory.is_finished(), "memory must remain pending");
        let inference = memory_model_call(app).await;
        assert!(!memory.is_finished(), "inference must finish before memory");
        release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), memory)
            .await
            .expect("released memory request must finish")
            .unwrap();
        assert_eq!(result.0, StatusCode::OK);
        assert_eq!(
            inference.0,
            StatusCode::OK,
            "pending {operation} must not occupy inference capacity"
        );
        assert_eq!(inference.1, memory_model_alias_output());
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "POST /v1/chat/completions");
        assert_eq!(calls[1].2["model"], "fixture");
    }
}

#[tokio::test]
async fn hindsight_reflect_same_class_dedicated_model_callback_succeeds() {
    let callback_endpoint = Arc::new(tokio::sync::OnceCell::<String>::new());
    let endpoint = callback_endpoint.clone();
    let (completed, mut callbacks) = tokio::sync::mpsc::unbounded_channel();
    let mock = memory_mock_with_handler(move |path| {
        let endpoint = endpoint.clone();
        let completed = completed.clone();
        async move {
            match path.as_str() {
                "/v1/chat/completions" => memory_json_response(memory_model_output()),
                "/v1/default/banks/agent-fixture/reflect" => {
                    let response = reqwest::Client::new()
                        .post(format!("{}/v1/chat/completions", endpoint.get().unwrap()))
                        .bearer_auth("test-model-key-0123456789")
                        .timeout(Duration::from_secs(5))
                        .json(&memory_model_input())
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let output: Value = response.json().await.unwrap();
                    completed.send((status, output.clone())).unwrap();
                    if status != StatusCode::OK {
                        return (
                            StatusCode::BAD_GATEWAY,
                            axum::Json(json!({"detail":"model callback failed"})),
                        )
                            .into_response();
                    }
                    memory_json_response(json!({"text":output["choices"][0]["message"]["content"]}))
                }
                _ => panic!("unexpected mock path: {path}"),
            }
        }
    })
    .await;
    let (_dir, app) = memory_fixture_with_admission(&mock, json!({"timeout_ms":30000}), 1, true);
    qualify_memory_model(&app);
    {
        let runtime = app.current();
        let auth = runtime.auth.read().unwrap();
        assert!(Arc::ptr_eq(
            &auth.credentials["fixture"].class_semaphore,
            &auth.credentials["hindsight-model"].class_semaphore,
        ));
        assert_eq!(auth.credentials["hindsight-model"].max_in_flight, 1);
    }
    let mut denied = memory_request("reflect", Body::from("{\"query\":\"hello\"}"));
    denied.headers_mut().insert(
        header::AUTHORIZATION,
        "Bearer test-model-key-0123456789".parse().unwrap(),
    );
    assert_eq!(
        memory_fixture_router(app.clone())
            .oneshot(denied)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(mock.calls.lock().unwrap().is_empty());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    callback_endpoint
        .set(format!("http://{}", listener.local_addr().unwrap()))
        .unwrap();
    let facade = memory_fixture_router(app.clone());
    let facade_task = tokio::spawn(async move { axum::serve(listener, facade).await.unwrap() });
    let reflection = memory_call(app, "reflect", json!({"query":"hello"})).await;
    facade_task.abort();
    let callback = tokio::time::timeout(Duration::from_secs(5), callbacks.recv())
        .await
        .expect("reflect must call the dedicated model credential")
        .unwrap();
    assert_eq!(
        callback.0,
        StatusCode::OK,
        "reflect must not hold its callback's inference permit"
    );
    assert_eq!(callback.1, memory_model_alias_output());
    assert_eq!(
        reflection,
        (StatusCode::OK, json!({"text":"ordinary inference"}))
    );
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "POST /v1/default/banks/agent-fixture/reflect");
    assert_eq!(calls[1].0, "POST /v1/chat/completions");
    assert_eq!(calls[1].2["model"], "fixture");
}

#[tokio::test]
async fn hindsight_memory_admission_remains_bounded_and_recovers() {
    let (mock, mut arrivals, release) = gated_memory_mock().await;
    let (_dir, app) = memory_fixture_with_admission(&mock, json!({"timeout_ms":30000}), 1, false);
    let first = tokio::spawn(memory_call(app.clone(), "recall", json!({"query":"first"})));
    tokio::time::timeout(Duration::from_secs(5), arrivals.recv())
        .await
        .expect("first memory request must reach upstream")
        .unwrap();
    assert!(!first.is_finished());
    let rejected = tokio::time::timeout(
        Duration::from_secs(5),
        memory_call(app.clone(), "reflect", json!({"query":"over capacity"})),
    )
    .await
    .expect("memory overload must be rejected promptly");
    assert_eq!(rejected.0, StatusCode::TOO_MANY_REQUESTS);
    assert!(matches!(
        arrivals.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
    release.add_permits(1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .unwrap()
            .unwrap()
            .0,
        StatusCode::OK
    );
    release.add_permits(1);
    assert_eq!(
        memory_call(app, "reflect", json!({"query":"recovered"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        arrivals.recv().await.unwrap(),
        "/v1/default/banks/agent-fixture/reflect"
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn hindsight_safe_errors_redirects_and_response_caps() {
    for (code, content_type, body) in [
        (
            StatusCode::UNAUTHORIZED,
            "application/json",
            "upstream-private-sentinel".into(),
        ),
        (
            StatusCode::FORBIDDEN,
            "application/json",
            "upstream-private-sentinel".into(),
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            "upstream-private-sentinel".into(),
        ),
        (
            StatusCode::TEMPORARY_REDIRECT,
            "application/json",
            "{}".into(),
        ),
        (
            StatusCode::OK,
            "text/event-stream",
            "data: upstream-private-sentinel".into(),
        ),
        (
            StatusCode::OK,
            "text/html",
            "upstream-private-sentinel".into(),
        ),
        (
            StatusCode::OK,
            "application/json",
            "not-json-upstream-private-sentinel".into(),
        ),
        (StatusCode::OK, "application/json", "[]".into()),
        (
            StatusCode::OK,
            "application/json",
            format!(
                "{{\"text\":\"{}\"}}",
                "x".repeat(hindsight::MAX_RESPONSE_BYTES + 1)
            ),
        ),
    ] {
        let mock = memory_mock(code, content_type, body, 0).await;
        let (_dir, app) = memory_fixture(&mock, json!({}));
        let (status, output) = memory_call(app, "recall", json!({"query":"hello"})).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{code} {content_type}");
        assert!(!output.to_string().contains("sentinel"));
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn hindsight_deadline_includes_request_upload() {
    let mock = memory_mock(StatusCode::OK, "application/json", "{}".into(), 400).await;
    let (_dir, app) = memory_fixture(&mock, json!({"timeout_ms":100}));
    let start = Instant::now();
    assert_eq!(
        memory_call(app.clone(), "recall", json!({"query":"q"}))
            .await
            .0,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert!(start.elapsed() < Duration::from_millis(350));
    let stream = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok::<_, io::Error>(Bytes::from_static(b"{}"))
    });
    let response = hindsight::routes()
        .with_state(app)
        .oneshot(memory_request("recall", Body::from_stream(stream)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn hindsight_request_cap_and_json_content_type() {
    let mock = memory_stub();
    let (_dir, app) = memory_fixture(&mock, json!({}));
    let response = hindsight::routes()
        .with_state(app.clone())
        .oneshot(memory_request(
            "recall",
            Body::from("x".repeat(hindsight::MAX_REQUEST_BYTES + 1)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let mut request = memory_request("recall", Body::from("{}"));
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    assert_eq!(
        hindsight::routes()
            .with_state(app)
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert!(mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hindsight_offline_auth_mapping_and_missing_secret() {
    let mock = memory_stub();
    let (_dir, app) = memory_fixture(&mock, json!({"api_key_file":"/does-not-exist"}));
    for auth in [None, Some("Bearer wrong-key"), Some("Basic invalid")] {
        let mut request = memory_request("recall", Body::from("{\"query\":\"hello\"}"));
        request.headers_mut().remove(header::AUTHORIZATION);
        if let Some(auth) = auth {
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, auth.parse().unwrap());
        }
        assert_eq!(
            hindsight::routes()
                .with_state(app.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        memory_call(app.clone(), "retain", json!({"items":[{"content":"text"}]}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        memory_call(app.clone(), "recall", json!({"query":"hello"}))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let (_dir, denied) = memory_fixture(
        &mock,
        json!({"bindings":{"other":{"bank_id":"elsewhere","allow_retain":true}}}),
    );
    assert_eq!(
        memory_call(denied, "reflect", json!({"query":"hello"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer test-workload-key-0123456789".parse().unwrap(),
    );
    assert_eq!(
        models(State(app.current()), headers).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn hindsight_offline_status_never_resolves_secret_and_reload_is_restart_only() {
    let mock = memory_stub();
    let (_dir, app) = memory_fixture(&mock, json!({"api_key_file":"/does-not-exist"}));
    let report = hindsight::status(&app.current().policy.config.hindsight);
    assert_eq!(report["state"], "enabled");
    assert_eq!(report["health"], "not_checked");
    assert!(!report.to_string().contains("does-not-exist"));
    let current = app.current();
    let mut candidate = config::load(&app.path).unwrap();
    candidate.config.hindsight.enabled = false;
    assert!(
        validate_reload(&current.policy, &candidate)
            .unwrap_err()
            .contains("Hindsight")
    );
    candidate.config.hindsight = current.policy.config.hindsight.clone();
    let digest = audit_identity::digest(&candidate.config);
    candidate.config.hindsight.api_key_file = Some("/different-private-path".into());
    assert_eq!(digest, audit_identity::digest(&candidate.config));
    candidate
        .config
        .hindsight
        .bindings
        .get_mut("fixture")
        .unwrap()
        .allow_retain = true;
    assert_ne!(digest, audit_identity::digest(&candidate.config));
}

#[tokio::test]
async fn hindsight_upload_deadline_and_cancellation_release_memory_capacity() {
    for cancel in [false, true] {
        let (mock, mut arrivals, release) = gated_memory_mock().await;
        let (_dir, app) = memory_fixture_with_admission(
            &mock,
            json!({"timeout_ms": if cancel { 2000 } else { 500 }}),
            1,
            false,
        );
        let (started, received) = tokio::sync::oneshot::channel();
        let stream = futures_util::stream::once(async move {
            let _ = started.send(());
            std::future::pending::<Result<Bytes, io::Error>>().await
        });
        let task_app = app.clone();
        let task = tokio::spawn(async move {
            hindsight::routes()
                .with_state(task_app)
                .oneshot(memory_request("recall", Body::from_stream(stream)))
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            memory_call(app.clone(), "recall", json!({"query":"blocked"}))
                .await
                .0,
            StatusCode::TOO_MANY_REQUESTS,
            "pending upload must occupy the sole memory permit"
        );
        assert!(mock.calls.lock().unwrap().is_empty());
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(task.await.unwrap().status(), StatusCode::GATEWAY_TIMEOUT);
        }
        let replacement = tokio::spawn(async move {
            memory_call(app, "recall", json!({"query":"replacement"})).await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap()
                .unwrap(),
            "/v1/default/banks/agent-fixture/memories/recall",
            "upload cancellation/timeout must free memory admission"
        );
        release.add_permits(1);
        assert_eq!(replacement.await.unwrap().0, StatusCode::OK);
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn hindsight_offline_rejects_query_and_event_stream_accept() {
    let mock = memory_stub();
    let (_dir, app) = memory_fixture(&mock, json!({}));
    let mut request = memory_request("recall", Body::from("{\"query\":\"q\"}"));
    *request.uri_mut() = "/v1/memory/recall?bank=other".parse().unwrap();
    assert_eq!(
        hindsight::routes()
            .with_state(app.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut request = memory_request("reflect", Body::from("{\"query\":\"q\"}"));
    request
        .headers_mut()
        .insert(header::ACCEPT, "text/event-stream".parse().unwrap());
    assert_eq!(
        hindsight::routes()
            .with_state(app)
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn hindsight_cli_health_is_not_bank_authorization() {
    let mock = memory_mock(
        StatusCode::UNAUTHORIZED,
        "application/json",
        "upstream-private-sentinel".into(),
        0,
    )
    .await;
    let (_dir, app) = memory_fixture(&mock, json!({}));
    let report = hindsight::check(&app.current().policy.config.hindsight).await;
    assert_eq!(report["banks"]["agent-fixture"], "unauthorized");
    assert_eq!(report["health"], "reachable");
    assert!(!report.to_string().contains("sentinel"));
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "GET /health");
    assert!(!calls[0].1.contains_key(header::AUTHORIZATION));
    assert_eq!(calls[1].0, "GET /v1/default/banks/agent-fixture/stats");
    assert_eq!(
        calls[1].1[header::AUTHORIZATION],
        "Bearer test-upstream-key-0123456789"
    );
}

#[tokio::test]
async fn hindsight_cancellation_releases_capacity() {
    for cancel in [false, true] {
        let (mock, mut arrivals, release) = gated_memory_mock().await;
        let (_dir, app) = memory_fixture_with_admission(
            &mock,
            json!({"timeout_ms": if cancel { 2000 } else { 500 }}),
            1,
            false,
        );
        let task_app = app.clone();
        let task = tokio::spawn(async move {
            memory_call(task_app, "recall", json!({"query":"pending"})).await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap()
                .unwrap(),
            "/v1/default/banks/agent-fixture/memories/recall"
        );
        assert_eq!(
            memory_call(app.clone(), "recall", json!({"query":"blocked"}))
                .await
                .0,
            StatusCode::TOO_MANY_REQUESTS,
            "pending upstream response must occupy the sole memory permit"
        );
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(task.await.unwrap().0, StatusCode::GATEWAY_TIMEOUT);
        }
        let replacement = tokio::spawn(async move {
            memory_call(app, "recall", json!({"query":"replacement"})).await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap()
                .unwrap(),
            "/v1/default/banks/agent-fixture/memories/recall",
            "response cancellation/timeout must free memory admission"
        );
        // A disconnected request may still be waiting in the mock handler.
        release.add_permits(2);
        assert_eq!(replacement.await.unwrap().0, StatusCode::OK);
        assert_eq!(mock.calls.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn hindsight_offline_verifier_store_identity_and_revocation() {
    let mock = memory_stub();
    let (directory, app) = memory_fixture(
        &mock,
        json!({"api_key_file":"/does-not-exist","timeout_ms":10000}),
    );
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = directory.path().join("keys.sqlite");
    key_manager::init(&store).unwrap();
    let policy = key_manager::KeyPolicy {
        id: "fixture".into(),
        aliases: vec!["test".into()],
        class: "test".into(),
        requests_per_minute: 60,
        burst: 10,
        max_in_flight: 2,
        operator: false,
        agent_affinity: false,
        allowed_routes: Vec::new(),
        cloud_allowed: false,
        expires_at: None,
    };
    let raw = key_manager::issue(&store, &policy, "test").unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&app.path).unwrap()).unwrap();
    value["credentials"] = json!([]);
    value["key_store"] = json!(store);
    fs::write(&app.path, value.to_string()).unwrap();
    let app = Arc::new(AppState::new(app.path.clone(), config::load(&app.path).unwrap()).unwrap());
    let make_request = || {
        let mut request = memory_request("recall", Body::from("{\"query\":\"hello\"}"));
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {raw}").parse().unwrap(),
        );
        request
    };
    assert_eq!(
        hindsight::routes()
            .with_state(app.clone())
            .oneshot(make_request())
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    key_manager::revoke_credential(&store, "fixture", "test").unwrap();
    assert!(reload_auth(&app, &store).await);
    assert_eq!(
        hindsight::routes()
            .with_state(app)
            .oneshot(make_request())
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
