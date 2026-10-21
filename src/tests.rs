use super::*;
use serde_json::json;

#[test]
fn audit_identity_is_source_and_effective_policy_not_version_or_secrets() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let before = status_payload(&runtime);
    assert_eq!(
        before["audit"]["build_source_sha256"]
            .as_str()
            .map(str::len),
        Some(64)
    );
    assert_eq!(
        before["audit"]["policy_sha256"].as_str().map(str::len),
        Some(64)
    );
    let mut config = runtime.policy.config.clone();
    config.credentials[0].key_file = Some("SECRET_KEY_PATH".into());
    config.credentials[0].name = "PRIVATE_CUSTOMER".into();
    config.routes[0].credential_file = Some("SECRET_ROUTE_KEY".into());
    config.aliases.get_mut("qwen38").unwrap().tokenizer = Some(config::TokenizerSpec {
        file: "SECRET_TOKENIZER_PATH".into(),
        sha256: "SECRET_TOKENIZER_HASH".into(),
    });
    assert_eq!(audit_identity::digest(&config), runtime.policy_digest);
    let mut reordered = config.clone();
    let entries: Vec<_> = config.aliases.iter().collect();
    reordered.aliases = entries
        .into_iter()
        .rev()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(audit_identity::digest(&reordered), runtime.policy_digest);
    config.affinity_lease_seconds += 1;
    assert_ne!(audit_identity::digest(&config), runtime.policy_digest);
    let encoded = audit_identity::projection(&config).to_string();
    assert!(!encoded.contains("SECRET") && !encoded.contains("PRIVATE_CUSTOMER"));
}

#[path = "reliability_tests.rs"]
mod reliability_tests;

#[test]
fn audit_identity_tracks_normalized_paths_without_url_secrets() {
    let (_dir, app) = capacity_test_app();
    let mut config = app.current().policy.config.clone();
    config.routes[0].base_url =
        "http://USER:SECRET@localhost:80/v1/../v2?token=SECRET#SECRET".into();
    let original = audit_identity::digest(&config);
    config.routes[0].base_url = "http://localhost/v2".into();
    assert_eq!(original, audit_identity::digest(&config));
    assert!(
        !audit_identity::projection(&config)
            .to_string()
            .contains("SECRET")
    );
    config.routes[0].base_url = "http://localhost/v3".into();
    assert_ne!(original, audit_identity::digest(&config));
    let before_health = audit_identity::digest(&config);
    config.routes[0].health_path = "/healthz".into();
    assert_ne!(before_health, audit_identity::digest(&config));
}

#[path = "audit_integration_tests.rs"]
mod audit_integration_tests;

#[test]
fn assistant_reasoning_content_is_preserved_typed_and_budgeted() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let alias = runtime.policy.config.aliases["qwen38"].clone();
    let base = json!({"model":"qwen38", "max_tokens":16, "messages":[
        {"role":"assistant", "content":"answer", "reasoning_content":"thought"},
        {"role":"user", "content":"continue"}
    ]});
    let before = base.clone();
    let budget = validate_request(&base, &alias, None, false)
        .expect("valid assistant reasoning must be accepted")
        .2;
    assert_eq!(base, before);
    let mut larger = base.clone();
    larger["messages"][0]["reasoning_content"] = json!("x".repeat(1000));
    assert!(validate_request(&larger, &alias, None, false).unwrap().2 > budget);
    for value in [Value::Null, json!("")] {
        let mut request = base.clone();
        request["messages"][0]["reasoning_content"] = value;
        assert!(validate_request(&request, &alias, None, false).is_ok());
    }
    for value in [json!(1), json!(false), json!([]), json!({})] {
        let mut request = base.clone();
        request["messages"][0]["reasoning_content"] = value;
        assert!(validate_request(&request, &alias, None, false).is_err());
    }
    for role in ["system", "developer", "user", "tool"] {
        let mut request = base.clone();
        request["messages"][0]["role"] = json!(role);
        assert!(validate_request(&request, &alias, None, false).is_err());
    }
    let mut unknown = base.clone();
    unknown["messages"][0]["unknown_field"] = json!("no");
    assert!(validate_request(&unknown, &alias, None, false).is_err());
    let mut tight = alias;
    tight.context_limit = budget - 1;
    assert!(validate_request(&base, &tight, None, false).is_err());
}

#[test]
fn reasoning_history_uses_tokenizer_budget_with_conservative_framing() {
    let path = match std::env::var("SLOTLINE_TEST_TOKENIZER") {
        Ok(path) => path,
        Err(_) => return,
    };
    let tokenizer = Tokenizer::from_file(path).unwrap();
    let messages =
        vec![json!({"role":"assistant","content":"answer","reasoning_content":"thought"})];
    let estimate = tokenized_prompt(&messages, None, &tokenizer, false)
        .expect("string reasoning is counted instead of forcing byte fallback");
    let expected_minimum = tokenizer.encode("thought", false).unwrap().len() as u64 + 32;
    assert!(estimate >= expected_minimum);
    let (_dir, app) = capacity_test_app();
    let mut alias = app.current().policy.config.aliases["qwen38"].clone();
    let request = json!({"model":"qwen38","max_tokens":16,"messages":messages});
    let budget = validate_request(&request, &alias, Some(&tokenizer), false)
        .expect("tokenizer-aware reasoning should fit")
        .2;
    assert!(budget < validate_request(&request, &alias, None, false).unwrap().2);
    alias.context_limit = budget - 1;
    assert!(validate_request(&request, &alias, Some(&tokenizer), false).is_err());
}

#[test]
fn large_tool_request_with_reasoning_uses_token_budget_below_context_limit() {
    let path = match std::env::var("SLOTLINE_TEST_TOKENIZER") {
        Ok(path) => path,
        Err(_) => return,
    };
    let tokenizer = Tokenizer::from_file(path).unwrap();
    let (_dir, app) = capacity_test_app();
    let mut alias = app.current().policy.config.aliases["qwen38"].clone();
    alias.capabilities.push("tools".into());
    let tools: Vec<Value> = (0..105)
        .map(|index| json!({"type":"function","function":{"name":format!("tool_{index}"),"description":"plain description ".repeat(100),"parameters":{"type":"object","properties":{}}}}))
        .collect();
    let request = json!({"model":"qwen38","max_tokens":8192,"messages":[
        {"role":"system","content":"system context ".repeat(3000)},
        {"role":"user","content":"user message ".repeat(300)},
        {"role":"assistant","content":"answer","reasoning_content":"r".repeat(3838)},
        {"role":"user","content":"continue"}
    ],"tools":tools,"tool_choice":"auto"});
    let byte_fallback = serde_json::to_vec(&request["messages"]).unwrap().len() as u64
        + 4 * 8
        + 64
        + serde_json::to_vec(
            &json!({"tools":request["tools"],"tool_choice":request["tool_choice"]}),
        )
        .unwrap()
        .len() as u64
        + 512;
    assert!(byte_fallback + 8192 > 200_000, "{byte_fallback}");
    let (_, _, budget, _) = validate_request(&request, &alias, Some(&tokenizer), false)
        .expect("tokenized tool and reasoning history fits context");
    assert!(budget < 200_000, "{budget}");
    assert!(
        tokenized_prompt_with_tools(
            request["messages"].as_array().unwrap(),
            request.get("tools"),
            request.get("tool_choice"),
            None,
            &tokenizer,
            false,
        )
        .is_some()
    );
}

#[test]
fn private_reasoning_budget_diagnostic_when_opted_in() {
    let (Ok(request_path), Ok(tokenizer_path)) = (
        std::env::var("SLOTLINE_TEST_REQUEST"),
        std::env::var("SLOTLINE_TEST_TOKENIZER"),
    ) else {
        return;
    };
    let request: Value = serde_json::from_slice(&fs::read(request_path).unwrap()).unwrap();
    let tokenizer = Tokenizer::from_file(tokenizer_path).unwrap();
    let policy = config::load(std::path::Path::new("config/slotline.local.yaml"))
        .expect("local Slotline policy");
    let model = request["model"].as_str().expect("captured model");
    let alias = policy
        .config
        .aliases
        .get(model)
        .expect("captured model alias");
    let (_, _, budget, _) = validate_request(&request, alias, Some(&tokenizer), false)
        .expect("private captured request must fit using tokenizer accounting");
    assert!(
        (49_090..191_808).contains(&(budget as u64)),
        "estimated tokens: {budget}"
    );
    println!("estimated_tokens={budget}");
}

#[test]
fn assistant_tool_call_without_content_is_valid_and_budgeted() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let mut alias = runtime.policy.config.aliases["qwen38"].clone();
    alias.capabilities.push("tools".into());
    let request = json!({"model":"qwen38", "max_tokens":16, "messages":[
        {"role":"user","content":"Call ping then reply OK."},
        {"role":"assistant","tool_calls":[{"id":"test_ping","type":"function","function":{"name":"ping","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"test_ping","content":"OK"}
    ]});
    let result = validate_request(&request, &alias, None, false);
    assert!(result.is_ok(), "tool-call-only assistant must be accepted");
    let (_, _, budget, capabilities) = result.unwrap();
    assert!(capabilities.tools);
    assert!(budget > 16);
    let mut large = request.clone();
    large["messages"][1]["tool_calls"][0]["function"]["arguments"] = json!("x".repeat(32000));
    assert!(validate_request(&large, &alias, None, false).unwrap().2 > budget);
}

#[test]
fn omitted_content_exception_preserves_tool_and_context_safeguards() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let mut alias = runtime.policy.config.aliases["qwen38"].clone();
    alias.capabilities.push("tools".into());
    let call =
        json!({"id":"call", "type":"function", "function":{"name":"ping", "arguments":"{}"}});
    for message in [
        json!({"role":"assistant"}),
        json!({"role":"user"}),
        json!({"role":"system"}),
        json!({"role":"developer"}),
        json!({"role":"tool","tool_call_id":"call"}),
        json!({"role":"assistant","tool_calls":[]}),
        json!({"role":"assistant","tool_calls":null}),
        json!({"role":"assistant","tool_calls":[{}]}),
        json!({"role":"user","tool_calls":[call.clone()]}),
        json!({"role":"assistant","content":123,"tool_calls":[call.clone()]}),
    ] {
        let request = json!({"model":"qwen38", "messages":[message], "max_tokens":16});
        assert!(validate_request(&request, &alias, None, false).is_err());
    }
    let request = json!({"model":"qwen38", "messages":[{"role":"assistant","tool_calls":[call]}], "max_tokens":16});
    let accepted = validate_request(&request, &alias, None, false).unwrap();
    let mut null_request = request.clone();
    null_request["messages"][0]["content"] = Value::Null;
    assert!(validate_request(&null_request, &alias, None, false).is_ok());
    alias.context_limit = accepted.2 - 1;
    assert!(validate_request(&request, &alias, None, false).is_err());
    alias.context_limit = 200000;
    alias.capabilities.retain(|cap| cap != "tools");
    assert!(validate_request(&request, &alias, None, false).is_err());
}

#[test]
fn backend_telemetry_status_is_explicitly_unavailable_before_sampling() {
    let (_dir, app) = capacity_test_app();
    let payload = status_payload(&app.current());
    for route in payload["routes"].as_object().unwrap().values() {
        assert_eq!(route["server_metrics"]["status"], "unavailable");
        assert!(route["server_metrics"]["generation_tokens_per_second"].is_null());
    }
}

#[tokio::test]
async fn backend_telemetry_get_collection_preserves_health_and_exposes_status() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let router = Router::new().route("/metrics", get(move || {
        let counter = counter.clone();
        async move {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            format!("llamacpp:tokens_predicted_total {}\nllamacpp:prompt_tokens_total {}\nhalogen:draft_tokens_total {}\nhalogen:draft_tokens_accepted_total {}\nllamacpp:predicted_tokens_seconds 80\n", 10+n*20, 100+n*100, 20+n*20, 10+n*15)
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, app) = capacity_test_app();
    let mut policy = config::load(&app.path).unwrap();
    policy.routes[0].config.base_url = format!("http://{addr}/v1");
    let runtime = make_runtime(policy, None).unwrap();
    let prior = runtime.observations.lock().unwrap()[0].status;
    crate::backend_telemetry::refresh(runtime.clone(), 0).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    crate::backend_telemetry::refresh(runtime.clone(), 0).await;
    let value = status_payload(&runtime);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        value["routes"]["small"]["server_metrics"]["acceptance_ratio"],
        0.75
    );
    assert!(
        status_cli::render(&value)
            .unwrap()
            .contains("acceptance 75.0%")
    );
    assert!(
        status_cli::render(&value)
            .unwrap()
            .contains("server-reported generation 80.00 tok/s")
    );
    let operator = fs::read_to_string(_dir.path().join("operator.key")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {operator}").parse().unwrap(),
    );
    let response = metrics(State(runtime.clone()), headers).await;
    let body = axum::body::to_bytes(response.into_body(), 1_048_576)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("slotline_backend_acceptance_ratio{route=\"small\"} 0.75"));
    assert!(
        text.contains("slotline_backend_reported_generation_tokens_per_second{route=\"small\"} 80")
    );
    assert!(
        value["routes"]["small"]["server_metrics"]["generation_tokens_per_second"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert_eq!(runtime.observations.lock().unwrap()[0].status, prior);
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    server.abort();
}

#[tokio::test]
async fn backend_telemetry_falls_back_to_slots_when_metrics_disabled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route(
            "/metrics",
            get(|| async { (StatusCode::NOT_IMPLEMENTED, "Start with --metrics") }),
        )
        .route(
            "/slots",
            get(|| async { "[{\"id\":0,\"is_processing\":true}]" }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, app) = capacity_test_app();
    let mut policy = config::load(&app.path).unwrap();
    policy.routes[0].config.base_url = format!("http://{addr}/v1");
    let runtime = make_runtime(policy, None).unwrap();
    crate::backend_telemetry::refresh(runtime.clone(), 0).await;
    let value = status_payload(&runtime);
    assert_eq!(
        value["routes"]["small"]["server_metrics"]["source"],
        "llamacpp_slots"
    );
    assert_eq!(
        value["routes"]["small"]["server_metrics"]["requests_running"],
        1
    );
    server.abort();
}

#[tokio::test]
async fn backend_telemetry_bounds_the_slots_fallback_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/metrics", get(|| async { StatusCode::NOT_IMPLEMENTED }))
        .route("/slots", get(|| async { "X".repeat(1_048_577) }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (_dir, app) = capacity_test_app();
    let mut policy = config::load(&app.path).unwrap();
    policy.routes[0].config.base_url = format!("http://{addr}/v1");
    let runtime = make_runtime(policy, None).unwrap();
    assert_eq!(
        crate::backend_telemetry::fetch(&runtime, 0, "slots")
            .await
            .unwrap_err(),
        "response_too_large"
    );
    crate::backend_telemetry::refresh(runtime.clone(), 0).await;
    let value = status_payload(&runtime);
    assert_eq!(
        value["routes"]["small"]["server_metrics"]["status"],
        "unavailable"
    );
    server.abort();
}

fn capacity_test_app() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\naffinity_lease_seconds: 900\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38], class: test, agent_affinity: true, operator: true}}\nroutes:\n  - {{name: small, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 2, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\n  - {{name: large, kind: local, base_url: 'http://127.0.0.1:18105/v1', model: qwen38, safe_concurrency: 10, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: small, priority: 10}}, {{route: large, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    fs::write(&path, source).unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let runtime = app.current();
    for observation in runtime.observations.lock().unwrap().iter_mut() {
        observation.status = RouteStatus::Serving;
        observation.checked = Some(Instant::now());
        observation.qualified = Some(Instant::now());
    }
    (dir, app)
}

fn select_test_route(
    runtime: &Runtime,
    key: Option<&[u8; 32]>,
) -> Option<(usize, OwnedSemaphorePermit)> {
    select_route(
        runtime,
        "qwen38",
        &runtime.policy.config.aliases["qwen38"],
        false,
        100,
        RequestCapabilities::default(),
        &[],
        key,
        &[],
    )
}

#[tokio::test]
async fn operator_status_reports_live_routes_keys_lease_ages_and_attempt_share() {
    let (dir, app) = capacity_test_app();
    let runtime = app.current();
    runtime.route_attempts[0].store(1, Ordering::Relaxed);
    runtime.route_attempts[1].store(3, Ordering::Relaxed);
    runtime
        .recent_routes
        .lock()
        .unwrap()
        .push_back((Instant::now(), 1));
    runtime.selection.lock().unwrap().leases.insert(
        [42; 32],
        AffinityLease {
            route_idx: 0,
            created_at: Instant::now() - Duration::from_secs(40),
            last_seen: Instant::now() - Duration::from_secs(10),
        },
    );
    let operator = fs::read_to_string(dir.path().join("operator.key")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {operator}").parse().unwrap(),
    );
    let response = status(State(runtime), headers).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["routes"]["small"]["traffic_percent_total"], 25.0);
    assert_eq!(value["routes"]["large"]["traffic_percent_5m"], 100.0);
    assert_eq!(value["routes"]["small"]["state"], "serving");
    assert_eq!(
        value["credentials"]["operator"]["aliases"]["qwen38"][0]["route"],
        "small"
    );
    assert_eq!(value["leases"][0]["route"], "small");
    assert!(value["leases"][0]["idle_seconds"].as_u64().unwrap() >= 10);
    assert!(value["leases"][0]["age_seconds"].as_u64().unwrap() >= 10);
    assert!(value.to_string().find(&operator).is_none());
    assert!(value.to_string().find("424242").is_none());
}

#[test]
fn status_snapshot_reports_bounded_probe_diagnostics_without_request_data() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let value = status_payload(&runtime);
    let route = &value["routes"]["small"];
    assert!(
        route.get("last_probe").is_some(),
        "missing probe diagnostics"
    );
    assert!(route["last_probe"].is_null());
    assert!(!value.to_string().contains("local-test-operator-key"));
}

#[tokio::test]
async fn discovery_http_failure_reports_status_stage_and_latency() {
    let (dir, _) = capacity_test_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/models",
                get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            ),
        )
        .await
        .unwrap();
    });
    let path = dir.path().join("policy.yaml");
    let source = fs::read_to_string(&path)
        .unwrap()
        .replace("18104", &port.to_string());
    fs::write(&path, source).unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let runtime = app.current();
    refresh_one(runtime.clone(), 0).await;
    let probe = &status_payload(&runtime)["routes"]["small"]["last_probe"];
    assert_eq!(probe["stage"], "discovery");
    assert_eq!(probe["outcome"], "http_error");
    assert_eq!(probe["http_status"], 503);
    assert!(probe["latency_ms"].is_u64());
    server.abort();
}

#[tokio::test]
async fn serving_probe_distinguishes_http_failure_from_invalid_payload() {
    for (status_code, body, expected) in [
        (StatusCode::SERVICE_UNAVAILABLE, json!({}), "http_error"),
        (
            StatusCode::OK,
            json!({"model":"wrong","choices":[{}]}),
            "invalid_response",
        ),
    ] {
        let (dir, _) = capacity_test_app();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/v1/models",
                        get(|| async { axum::Json(json!({"data":[{"id":"qwen38"}]})) }),
                    )
                    .route("/health", get(|| async { StatusCode::OK }))
                    .route(
                        "/v1/chat/completions",
                        post(move || async move { (status_code, axum::Json(body)) }),
                    ),
            )
            .await
            .unwrap();
        });
        let path = dir.path().join("policy.yaml");
        let source = fs::read_to_string(&path)
            .unwrap()
            .replace("18104", &port.to_string());
        fs::write(&path, source).unwrap();
        let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
        let runtime = app.current();
        refresh_one(runtime.clone(), 0).await;
        let value = status_payload(&runtime);
        let route = &value["routes"]["small"];
        assert_eq!(route["last_probe"]["stage"], "serving");
        assert_eq!(route["last_probe"]["outcome"], expected);
        assert_eq!(route["last_probe"]["http_status"], status_code.as_u16());
        assert_eq!(route["state"], "stale");
        server.abort();
    }
}

#[tokio::test]
async fn discovery_transport_failure_is_not_reported_as_http_error() {
    let (dir, _) = capacity_test_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let path = dir.path().join("policy.yaml");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("18104", &port.to_string()),
    )
    .unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let runtime = app.current();
    refresh_one(runtime.clone(), 0).await;
    let probe = &status_payload(&runtime)["routes"]["small"]["last_probe"];
    assert_eq!(probe["outcome"], "transport_error");
    assert!(probe["http_status"].is_null());
}

#[test]
fn status_cli_explains_latest_probe_failure_without_body_or_secret() {
    let mut value = json!({"routes":{"r1":{"state":"stale","enabled":true,"base_url":"http://127.0.0.1:8080/v1","model":"qwen38","active":0,"limit":2,"last_checked_age_seconds":4,"attempts_total":0,"attempts_5m":0,"traffic_percent_total":0.0,"traffic_percent_5m":0.0,"last_probe":{"stage":"serving","outcome":"http_error","http_status":503,"latency_ms":123,"age_seconds":9}}},"credentials":{},"leases":[],"lease_ttl_seconds":900,"requests_total":0,"rejects_total":0,"upstream_errors_total":0});
    let output = status_cli::render(&value).unwrap();
    assert!(output.contains("serving"));
    assert!(output.contains("http_error"));
    assert!(output.contains("503"));
    assert!(output.contains("123ms"));
    value["routes"]["r1"]["last_probe"] = Value::Null;
    assert!(status_cli::render(&value).is_ok());
}

#[test]
fn status_retains_last_successful_qualification_after_cooldown() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    {
        let mut observations = runtime.observations.lock().unwrap();
        observations[0].qualified = Some(Instant::now() - Duration::from_secs(10));
    }
    mark_failure(&runtime, 0);
    let value = status_payload(&runtime);
    assert!(value["routes"]["small"]["last_successful_qualification_age_seconds"].is_u64());
    assert!(value["routes"]["small"]["last_qualified_age_seconds"].is_null());
}

#[tokio::test]
async fn readiness_http_failure_preserves_stage_and_status() {
    let (dir, _) = capacity_test_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/v1/models",
                    get(|| async { axum::Json(json!({"data":[{"id":"qwen38"}]})) }),
                )
                .route("/health", get(|| async { StatusCode::SERVICE_UNAVAILABLE })),
        )
        .await
        .unwrap();
    });
    let path = dir.path().join("policy.yaml");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("18104", &port.to_string()),
    )
    .unwrap();
    let runtime = AppState::new(path.clone(), config::load(&path).unwrap())
        .unwrap()
        .current();
    refresh_one(runtime.clone(), 0).await;
    let probe = &status_payload(&runtime)["routes"]["small"]["last_probe"];
    assert_eq!(probe["stage"], "readiness");
    assert_eq!(probe["outcome"], "http_error");
    assert_eq!(probe["http_status"], 503);
    server.abort();
}

#[tokio::test]
async fn malformed_models_payload_is_invalid_response_not_missing_model() {
    let (dir, _) = capacity_test_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/models",
                get(|| async { axum::Json(json!({"unexpected":"body"})) }),
            ),
        )
        .await
        .unwrap();
    });
    let path = dir.path().join("policy.yaml");
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("18104", &port.to_string()),
    )
    .unwrap();
    let runtime = AppState::new(path.clone(), config::load(&path).unwrap())
        .unwrap()
        .current();
    refresh_one(runtime.clone(), 0).await;
    let probe = &status_payload(&runtime)["routes"]["small"]["last_probe"];
    assert_eq!(probe["outcome"], "invalid_response");
    assert_eq!(probe["http_status"], 200);
    server.abort();
}

#[test]
fn latest_success_does_not_erase_prior_probe_failure_diagnostic() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    record_probe(&runtime, 0, "serving", "timeout", None, Instant::now());
    record_probe(
        &runtime,
        0,
        "discovery",
        "success",
        Some(200),
        Instant::now(),
    );
    let route = &status_payload(&runtime)["routes"]["small"];
    assert_eq!(route["last_probe"]["outcome"], "success");
    assert_eq!(route["last_failed_probe"]["outcome"], "timeout");
    let output = status_cli::render(&status_payload(&runtime)).unwrap();
    assert!(output.contains("last failure: serving timeout"));
}

#[test]
fn status_cli_renders_health_keys_leases_and_traffic_without_secret_material() {
    let value = json!({"routes":{"r1":{"state":"serving","enabled":true,"base_url":"http://127.0.0.1:8080/v1","model":"qwen38","active":1,"limit":2,"last_checked_age_seconds":4,"last_qualified_age_seconds":5,"attempts_total":10,"attempts_5m":2,"traffic_percent_total":66.7,"traffic_percent_5m":50.0}},"credentials":{"profile":{"class":"test","agent_affinity":true,"operator":false,"aliases":{"qwen38":[{"route":"r1","endpoint":"http://127.0.0.1:8080/v1","priority":10,"fallback_only":false}]} }},"leases":[{"route":"r1","age_seconds":30,"idle_seconds":8,"remaining_seconds":892}],"lease_ttl_seconds":900,"traffic_window_seconds":300,"requests_total":11,"rejects_total":1,"upstream_errors_total":0});
    let output = status_cli::render(&value).unwrap();
    for expected in [
        "r1",
        "serving",
        "profile",
        "qwen38",
        "30s",
        "8s",
        "66.7%",
        "50.0%",
        "http://127.0.0.1:8080/v1",
    ] {
        assert!(output.contains(expected), "missing {expected}");
    }
    assert!(status_cli::render(&json!({"routes":{}})).is_err());
}

#[tokio::test]
async fn status_requires_operator_and_excludes_expired_leases() {
    let (dir, app) = capacity_test_app();
    let runtime = app.current();
    let denied = status(State(runtime.clone()), HeaderMap::new()).await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    runtime.selection.lock().unwrap().leases.insert(
        [7; 32],
        AffinityLease {
            route_idx: 0,
            created_at: Instant::now() - Duration::from_secs(1000),
            last_seen: Instant::now() - Duration::from_secs(901),
        },
    );
    let operator = fs::read_to_string(dir.path().join("operator.key")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {operator}").parse().unwrap(),
    );
    let response = status(State(runtime), headers).await;
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["leases"].as_array().unwrap().len(), 0);
}

#[test]
fn empty_machine_precedes_partly_occupied_machine() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    assert_eq!(select_test_route(&runtime, None).unwrap().0, 1);
    drop(held);
}

#[test]
fn same_occupancy_chooses_most_free_slots() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let small_held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    let large_held = runtime.semaphores[1].clone().try_acquire_owned().unwrap();
    assert_eq!(select_test_route(&runtime, None).unwrap().0, 1);
    drop((small_held, large_held));
}

#[test]
fn zero_occupancy_precedes_sticky_partly_occupied_route() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let key = [17u8; 32];
    runtime.selection.lock().unwrap().leases.insert(
        key,
        AffinityLease {
            route_idx: 0,
            created_at: Instant::now(),
            last_seen: Instant::now(),
        },
    );
    let held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    assert_eq!(select_test_route(&runtime, Some(&key)).unwrap().0, 1);
    drop(held);
}

#[test]
fn fewer_occupied_slots_precede_lower_utilization() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let small_held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    let large_held: Vec<_> = (0..2)
        .map(|_| runtime.semaphores[1].clone().try_acquire_owned().unwrap())
        .collect();
    assert_eq!(select_test_route(&runtime, None).unwrap().0, 0);
    drop((small_held, large_held));
}

#[test]
fn busy_when_all_physical_permits_are_occupied() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let held: Vec<_> = runtime
        .semaphores
        .iter()
        .flat_map(|semaphore| {
            (0..semaphore.available_permits())
                .map(|_| semaphore.clone().try_acquire_owned().unwrap())
        })
        .collect();
    assert!(select_test_route(&runtime, None).is_none());
    drop(held);
    assert!(select_test_route(&runtime, None).is_some());
}

#[test]
fn profile_identity_ignores_session_values_but_rejects_malformed_header() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let credential = &runtime.policy.config.credentials[0];
    let mut first = HeaderMap::new();
    first.insert("x-slotline-session", HeaderValue::from_static("one"));
    let mut second = HeaderMap::new();
    second.insert("x-slotline-session", HeaderValue::from_static("two"));
    assert_eq!(
        affinity_key(&first, credential, "qwen38").unwrap(),
        affinity_key(&second, credential, "qwen38").unwrap()
    );
    let mut other_credential = credential.clone();
    other_credential.name = "other-profile".into();
    assert_ne!(
        affinity_key(&first, credential, "qwen38").unwrap(),
        affinity_key(&first, &other_credential, "qwen38").unwrap()
    );
    second.append("x-slotline-session", HeaderValue::from_static("duplicate"));
    assert!(affinity_key(&second, credential, "qwen38").is_err());
}

#[test]
fn lease_expires_at_configured_boundary_without_releasing_inflight_permit() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let key = [19u8; 32];
    let held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    let almost_expired = Instant::now() - Duration::from_secs(899);
    runtime.selection.lock().unwrap().leases.insert(
        key,
        AffinityLease {
            route_idx: 0,
            created_at: almost_expired,
            last_seen: almost_expired,
        },
    );
    assert!(select_test_route(&runtime, Some(&key)).is_some());
    assert_eq!(runtime.affinity_expired.load(Ordering::Relaxed), 0);
    runtime
        .selection
        .lock()
        .unwrap()
        .leases
        .get_mut(&key)
        .unwrap()
        .last_seen = Instant::now() - Duration::from_secs(900);
    assert!(select_test_route(&runtime, Some(&key)).is_some());
    assert_eq!(runtime.affinity_expired.load(Ordering::Relaxed), 1);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    drop(held);
    assert_eq!(runtime.semaphores[0].available_permits(), 2);
}

#[test]
fn reload_rejects_changed_physical_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38], class: test}}\nroutes:\n  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 2, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: qwen-4, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    fs::write(&path, &source).unwrap();
    let current = config::load(&path).unwrap();
    fs::write(
        &path,
        source.replace("safe_concurrency: 2", "safe_concurrency: 3"),
    )
    .unwrap();
    let candidate = config::load(&path).unwrap();
    assert!(validate_reload(&current, &candidate).is_err());
}

#[tokio::test]
async fn reload_publishes_catalog_with_shared_admission() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38], class: test}}\nroutes:\n  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: qwen-4, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    fs::write(&path, &source).unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let old = app.current();
    let held = old.semaphores[0].clone().try_acquire_owned().unwrap();
    fs::write(
        &path,
        source.replace(
            "context_limit: 200000, capabilities",
            "context_limit: 100000, capabilities",
        ),
    )
    .unwrap();
    reload_policy(&app).await.unwrap();
    let new = app.current();
    assert_eq!(new.policy.config.aliases["qwen38"].context_limit, 100000);
    assert_eq!(old.policy.config.aliases["qwen38"].context_limit, 200000);
    assert!(Arc::ptr_eq(&old.semaphores[0], &new.semaphores[0]));
    assert!(new.semaphores[0].clone().try_acquire_owned().is_err());
    drop(held);
    assert!(new.semaphores[0].clone().try_acquire_owned().is_ok());
}

#[tokio::test]
async fn policy_control_requires_private_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("policy.yaml");
    assert!(bind_policy_control(&path, None).is_err());
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let (listener, socket) = match bind_policy_control(&path, None) {
        Ok(bound) => bound,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("policy control bind: {error}"),
    };
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(listener);
    fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn policy_control_uses_private_store_directory_for_shared_policy_directory() {
    let dir = tempfile::tempdir().unwrap();
    let policy = dir.path().join("config/policy.yaml");
    let store = dir.path().join("private/keys.sqlite3");
    fs::create_dir_all(policy.parent().unwrap()).unwrap();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    fs::set_permissions(policy.parent().unwrap(), fs::Permissions::from_mode(0o775)).unwrap();
    fs::set_permissions(store.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = policy_control_path(&policy, Some(&store));
    assert_eq!(socket.parent(), store.parent());
    let (listener, bound) = bind_policy_control(&policy, Some(&store)).unwrap();
    assert_eq!(bound, socket);
    drop(listener);
    fs::remove_file(bound).unwrap();
}

#[tokio::test]
async fn socket_guard_does_not_unlink_replacement_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("policy.yaml");
    let (listener, socket) = bind_policy_control(&path, None).unwrap();
    let guard = ControlSocketGuard::new(socket.clone()).unwrap();
    fs::remove_file(&socket).unwrap();
    fs::write(&socket, b"replacement").unwrap();
    drop(guard);
    assert_eq!(fs::read(&socket).unwrap(), b"replacement");
    drop(listener);
}

#[test]
fn reload_rejects_changed_legacy_key_material() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38], class: test}}\nroutes:\n  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: qwen-4, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    fs::write(&path, source).unwrap();
    let current = config::load(&path).unwrap();
    fs::write(&key, b"another-local-operator-key-1234567890").unwrap();
    let candidate = config::load(&path).unwrap();
    assert!(validate_reload(&current, &candidate).is_err());
}

#[test]
fn reload_rejects_provider_alias_on_local_route() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38, gpt-6-luna], class: test}}\nroutes:\n  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: qwen-4, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    // Start from a valid local catalog whose fixed credential policy has
    // already reserved the alias; a reload cannot publish that provider name.
    let current_source = source.replace("[qwen38, gpt-6-luna]", "[qwen38]");
    fs::write(&path, &current_source).unwrap();
    let current = config::load(&path).unwrap();
    let candidate_source = source.replace("[qwen38, gpt-6-luna]", "[qwen38]")
        + "  gpt-6-luna: {context_limit: 200000, capabilities: [text], routes: [{route: qwen-4, priority: 10}]}\n";
    fs::write(&path, candidate_source).unwrap();
    let candidate = config::load(&path).unwrap();
    assert!(validate_reload(&current, &candidate).is_err());
}

#[test]
fn reload_rejects_codex_adapter_disguised_as_qwen() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38], class: test}}\nroutes:\n  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: codex-proxy-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: qwen-4, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    fs::write(&path, &source).unwrap();
    let current = config::load(&path).unwrap();
    let candidate = config::load(&path).unwrap();
    assert!(validate_reload(&current, &candidate).is_err());
}

#[tokio::test]
async fn invalid_candidate_keeps_generation_auth_and_health() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("operator.key");
    fs::write(&key, b"local-test-operator-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let source = format!(
        "bind: 100.64.0.1\nport: 4012\ncredentials:\n  - {{name: operator, key_file: {}, aliases: [qwen38], class: test}}\nroutes:\n  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: mock-v1, capabilities: [text], enabled: true}}\naliases:\n  qwen38: {{context_limit: 200000, capabilities: [text], routes: [{{route: qwen-4, priority: 10}}]}}\n",
        key.display()
    );
    let path = dir.path().join("policy.yaml");
    fs::write(&path, &source).unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let old = app.current();
    set_status(&old, 0, RouteStatus::Serving, "test");
    let old_auth = old.auth.read().unwrap().clone();
    fs::write(&path, source.replace("route: qwen-4", "route: missing")).unwrap();
    assert!(reload_policy(&app).await.is_err());
    let active = app.current();
    assert!(Arc::ptr_eq(&old, &active));
    assert!(Arc::ptr_eq(&old_auth, &*active.auth.read().unwrap()));
    assert_eq!(
        active.observations.lock().unwrap()[0].status,
        RouteStatus::Serving
    );
}

#[test]
fn embedding_context_byte_heuristic_checks_each_item_and_route_limit() {
    // Boundary: this ASCII item fits a 35-token alias but not a 34-token route.
    assert_eq!(embedding_input_context_tokens("abc"), 35);
    assert_eq!(embedding_input_context_tokens("你好"), 38);
    assert_eq!(embedding_input_context_tokens("é"), 34);
    assert_eq!(embedding_batch_context_tokens(&["a", "b"]), 33);
    assert_eq!(embedding_batch_context_tokens(&["a", "你好"]), 38);
    assert_eq!(embedding_batch_context_tokens(&[]), 0);
    let alias_limit = 100;
    let route_limit = 37;
    let batch = ["ok", "你好"];
    let needed = embedding_batch_context_tokens(&batch);
    assert!(needed <= alias_limit as u64);
    assert!(needed > route_limit as u64);
}

#[test]
fn qwen_context_counter_matches_live_fixture_counts() {
    let Ok(path) = std::env::var("SLOTLINE_TEST_TOKENIZER") else {
        return;
    };
    let tokenizer = Tokenizer::from_file(path).expect("pinned Qwen tokenizer");
    let one = vec![json!({"role":"user","content":"Hello"})];
    let two = vec![
        json!({"role":"system","content":"Rules."}),
        json!({"role":"user","content":"Hello"}),
    ];
    let assistant = vec![
        json!({"role":"user","content":"Hi"}),
        json!({"role":"assistant","content":"Hello"}),
    ];
    assert_eq!(
        tokenized_prompt(
            &one,
            Some(&json!({"enable_thinking":false})),
            &tokenizer,
            true
        ),
        Some(13 + 32)
    );
    assert_eq!(
        tokenized_prompt(&one, None, &tokenizer, true),
        Some(53 + 32)
    );
    assert_eq!(
        tokenized_prompt(
            &two,
            Some(&json!({"enable_thinking":false})),
            &tokenizer,
            true
        ),
        Some(20 + 32)
    );
    assert_eq!(
        tokenized_prompt(
            &assistant,
            Some(&json!({"enable_thinking":false})),
            &tokenizer,
            true
        ),
        Some(23 + 32)
    );
    assert_eq!(
        tokenized_prompt(
            &one,
            Some(&json!({"other_template_arg":true})),
            &tokenizer,
            true
        ),
        None
    );
    let tool = json!({"type":"function","function":{"name":"get_status","description":"Return current status","parameters":{"type":"object","properties":{},"additionalProperties":false}}});
    let tool_estimate = tokenized_prompt_with_tools(
        &one,
        Some(&json!([tool])),
        Some(&json!("required")),
        Some(&json!({"enable_thinking":false})),
        &tokenizer,
        true,
    )
    .expect("tool prompt estimate");
    // All three live Qwen routes reported 266 prompt tokens for this fixture.
    assert!((266..1000).contains(&tool_estimate), "{tool_estimate}");
    let long_tool = json!({"type":"function","function":{"name":"read_context","description":"plain ".repeat(1000),"parameters":{"type":"object","properties":{}}}});
    let tool_alias = config::Alias {
        passthrough_route: None,
        context_limit: 3000,
        tokenizer: None,
        endpoint: Endpoint::Chat,
        routes: Vec::new(),
        capabilities: vec!["text".into(), "tools".into()],
        streaming: true,
    };
    let long_request = json!({"model":"test","messages":one.clone(),"max_tokens":100,"tools":[long_tool],"chat_template_kwargs":{"enable_thinking":false}});
    assert!(validate_request(&long_request, &tool_alias, Some(&tokenizer), true).is_ok());
    assert!(validate_request(&long_request, &tool_alias, None, true).is_err());
    let alias = config::Alias {
        passthrough_route: None,
        context_limit: 55,
        tokenizer: None,
        endpoint: Endpoint::Chat,
        routes: Vec::new(),
        capabilities: vec!["text".into()],
        streaming: true,
    };
    let within = json!({"model":"test","messages":one,"max_tokens":10,"chat_template_kwargs":{"enable_thinking":false}});
    assert!(validate_request(&within, &alias, Some(&tokenizer), true).is_ok());
    let mut above = within.clone();
    above["max_tokens"] = json!(11);
    assert!(validate_request(&above, &alias, Some(&tokenizer), true).is_err());
}

#[test]
fn ordinary_success_cannot_restore_serving_after_qualification_expires() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    {
        let mut observations = runtime.observations.lock().unwrap();
        observations[0].status = RouteStatus::Stale;
        observations[0].qualified = Some(Instant::now() - Duration::from_secs(901));
    }
    status::mark_success(&runtime, 0);
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].status, RouteStatus::Ready);
    assert!(observations[0].qualified.unwrap().elapsed() > Duration::from_secs(900));
}

#[test]
fn timeout_fix_overlapping_success_does_not_bypass_invalidated_qualification() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    {
        let mut observations = runtime.observations.lock().unwrap();
        observations[0].status = RouteStatus::Serving;
        observations[0].qualified = Some(Instant::now());
        observations[0].checked = Some(Instant::now());
    }
    mark_failure(&runtime, 0);
    mark_success(&runtime, 0);
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].status, RouteStatus::Ready);
    assert!(observations[0].qualified.is_none());
    assert!(observations[0].last_successful_qualification.is_some());
}

#[test]
fn timeout_fix_ordinary_success_preserves_fresh_qualification_without_renewing_it() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let original = Instant::now() - Duration::from_secs(60);
    runtime.observations.lock().unwrap()[0].qualified = Some(original);
    mark_success(&runtime, 0);
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].status, RouteStatus::Serving);
    assert_eq!(observations[0].qualified, Some(original));
}

#[test]
fn progress_settings_change_policy_identity() {
    let (_dir, app) = capacity_test_app();
    let mut config = app.current().policy.config.clone();
    let class = config.credentials[0].class.clone();
    config.class_timeout_budgets.insert(
        class.clone(),
        config::ClassTimeoutBudget {
            attempt_seconds: 3,
            total_seconds: 4,
            ..Default::default()
        },
    );
    let original = audit_identity::digest(&config);
    config
        .class_timeout_budgets
        .get_mut(&class)
        .unwrap()
        .first_output_seconds = Some(1);
    let first = audit_identity::digest(&config);
    assert_ne!(original, first);
    config
        .class_timeout_budgets
        .get_mut(&class)
        .unwrap()
        .stream_idle_seconds = Some(1);
    assert_ne!(first, audit_identity::digest(&config));
}

#[test]
fn timeout_fix_effective_policy_identity_tracks_retry_floor() {
    let (_dir, app) = capacity_test_app();
    let mut config = app.current().policy.config.clone();
    let class = config.credentials[0].class.clone();
    config.class_timeout_budgets.insert(
        class.clone(),
        config::ClassTimeoutBudget {
            attempt_seconds: 540,
            total_seconds: 570,
            min_retry_seconds: 0,
            ..Default::default()
        },
    );
    let before = audit_identity::digest(&config);
    config
        .class_timeout_budgets
        .get_mut(&class)
        .unwrap()
        .min_retry_seconds = 120;
    assert_ne!(before, audit_identity::digest(&config));
}

#[tokio::test]
async fn timeout_fix_qualification_probe_restores_serving_from_invalidated_state() {
    let (dir, _) = capacity_test_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/v1/models",
                    get(|| async { axum::Json(json!({"data":[{"id":"qwen38"}]})) }),
                )
                .route("/health", get(|| async { StatusCode::OK }))
                .route(
                    "/v1/chat/completions",
                    post(|| async {
                        axum::Json(
                            json!({"model":"qwen38","choices":[{"message":{"content":"ok"}}]}),
                        )
                    }),
                ),
        )
        .await
        .unwrap();
    });
    let path = dir.path().join("policy.yaml");
    let source = fs::read_to_string(&path)
        .unwrap()
        .replace("18104", &port.to_string());
    fs::write(&path, source).unwrap();
    let app = AppState::new(path.clone(), config::load(&path).unwrap()).unwrap();
    let runtime = app.current();
    mark_failure(&runtime, 0);
    runtime.observations.lock().unwrap()[0].cooldown_until =
        Some(Instant::now() - Duration::from_secs(1));
    refresh_one(runtime.clone(), 0).await;
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].status, RouteStatus::Serving);
    assert!(observations[0].qualified.unwrap().elapsed() < Duration::from_secs(2));
    assert_eq!(
        observations[0].qualified,
        observations[0].last_successful_qualification
    );
    assert_eq!(observations[0].failures, 0);
    assert!(observations[0].cooldown_until.is_none());
    server.abort();
}
