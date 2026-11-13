use super::*;

#[path = "semantic_http_tests.rs"]
mod http;

fn configured() -> Value {
    let mut value = minimal_config();
    value["routes"] = json!([{"name":"native", "kind":"local", "base_url":"http://127.0.0.1:9/v1",
        "model":"synthetic", "safe_concurrency":1, "context_limit":8192, "adapter_version":"mock-v1",
        "capabilities":["text","tools","json_schema","json_object","responses","responses_json_schema","responses_json_object"], "enabled":true}]);
    let alias = json!({"context_limit":8192,"capabilities":["text","tools","json_schema","json_object","responses","responses_json_schema","responses_json_object"],"routes":[{"route":"native","priority":1}]});
    value["aliases"] = json!({"small":alias,"large":alias,"judge":alias});
    value["semantic_routing"] = json!({"enabled":true,"max_in_flight":1,
        "profiles":{"auto":{"targets":["small","large"],"default_target":"small",
        "rules":[{"id":"code","priority":10,"target":"large","regex":"code|debug|refactor|function|stack trace"}],
        "decision":{"classifier":"judge","confidence_threshold":0.8,"timeout_ms":200,"max_output_tokens":64,"on_failure":"default"}}}});
    value["credentials"] = json!([{"name":"fixture","class":"test","aliases":["auto","judge","small","large"],
        "max_in_flight":1,"burst":100,"requests_per_minute":600,"allowed_routes":["native"]}]);
    value
}

fn runtime_from(value: Value) -> Arc<Runtime> {
    let config: config::Config = serde_json::from_value(value).unwrap();
    semantic::validate(&config).unwrap();
    let policy = Policy {
        credentials: config
            .credentials
            .iter()
            .map(|credential| config::LoadedCredential {
                config: credential.clone(),
                key: Some(b"synthetic-semantic-test-key-1234567890".to_vec()),
            })
            .collect(),
        routes: config
            .routes
            .iter()
            .map(|route| config::LoadedRoute {
                config: route.clone(),
                key: None,
            })
            .collect(),
        config,
        matchers: Vec::new(),
        tokenizers: HashMap::new(),
        key_snapshot: None,
    };
    let runtime = make_runtime(policy, None).unwrap();
    for observation in runtime.observations.lock().unwrap().iter_mut() {
        observation.status = RouteStatus::Serving;
        observation.checked = Some(Instant::now());
        observation.qualified = Some(Instant::now());
    }
    runtime
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer synthetic-semantic-test-key-1234567890"),
    );
    headers
}

fn decision_payload(content: &str) -> Value {
    json!({"model":"synthetic","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":content}}]})
}

fn minimal_config() -> Value {
    json!({"bind":"100.127.255.254", "port":4000, "routes":[], "aliases":{}})
}

fn repair_v2_codex_config(default: bool) -> Value {
    let mut value = configured();
    value["codex"] = json!({"enabled":true,"alias":"cloud","model":"synthetic","cli_auth_file":"/nonexistent/repair-v2-codex-auth.json"});
    value["credentials"][0]["aliases"] = json!(["auto", "judge", "small", "large", "cloud"]);
    value["credentials"][0]["allowed_routes"] = json!(["native", "codex"]);
    value["credentials"][0]["cloud_allowed"] = json!(true);
    value["semantic_routing"]["profiles"]["auto"]["compatibility_preflight"] = json!(true);
    value["semantic_routing"]["profiles"]["auto"]["targets"] = json!(["small", "cloud"]);
    value["semantic_routing"]["profiles"]["auto"]["rules"][0]["target"] = json!("cloud");
    value["semantic_routing"]["profiles"]["auto"]["default_target"] =
        json!(if default { "cloud" } else { "small" });
    value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!("error");
    value
}

fn repair_v2_codex_request(responses: bool, extra_body: bool) -> Value {
    let mut request = if responses {
        json!({"model":"auto","input":"hello"})
    } else {
        json!({"model":"auto","messages":[{"role":"user","content":"hello"}]})
    };
    let field = if responses {
        "max_output_tokens"
    } else {
        "max_tokens"
    };
    if extra_body {
        request["extra_body"] = json!({});
        request["extra_body"][field] = json!(16);
    } else {
        request[field] = json!(16);
    }
    request
}

fn repair_v3_request(responses: bool, depth: usize, mut payload: Value) -> Value {
    let mut request = if responses {
        json!({"model":"auto","input":"hello"})
    } else {
        json!({"model":"auto","messages":[{"role":"user","content":"hello"}]})
    };
    for _ in 0..depth {
        payload = json!({"extra_body":payload});
    }
    request
        .as_object_mut()
        .unwrap()
        .extend(payload.as_object().unwrap().clone());
    request
}

fn repair_v3_codex_compatible(request: &Value, responses: bool) -> bool {
    let mut projected = request.clone();
    normalize_extra_body(&mut projected, false).unwrap();
    if responses {
        codex::native_request(&projected).is_ok()
    } else {
        codex::chat_request(&projected).is_ok_and(|body| codex::native_request(&body).is_ok())
    }
}

#[tokio::test]
async fn repair_v3_nested_codex_default_rejects_before_admission() {
    for responses in [false, true] {
        for payload in [json!({}), json!({"stream":false})] {
            for on_failure in ["default", "error"] {
                let request = repair_v3_request(responses, 2, payload.clone());
                let original = request.clone();
                assert!(!repair_v3_codex_compatible(&request, responses));
                let mut value = repair_v2_codex_config(true);
                value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] =
                    json!(on_failure);
                let runtime = runtime_from(value);
                let held = runtime
                    .semantic_admission
                    .clone()
                    .try_acquire_owned()
                    .unwrap();
                let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
                let response = semantic::dispatch(
                    runtime.clone(),
                    credential,
                    headers(),
                    request.clone(),
                    responses,
                )
                .await;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                assert_eq!(
                    response.headers()["x-slotline-semantic-outcome"],
                    "incompatible"
                );
                assert!(
                    response
                        .headers()
                        .get("x-slotline-semantic-target")
                        .is_none()
                );
                assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
                assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
                assert_eq!(request, original);
                drop(held);
            }
        }
    }
}

#[test]
fn repair_v3_original_context_matches_once_normalized_codex_validation() {
    let config: config::Config = serde_json::from_value(repair_v2_codex_config(false)).unwrap();
    let profile = &config.semantic_routing.profiles["auto"];
    let rules = semantic::compile_rules(profile).unwrap();
    let tokenizers = HashMap::new();
    for responses in [false, true] {
        for depth in 0..=2 {
            for payload in [json!({}), json!({"stream":false})] {
                let request = repair_v3_request(responses, depth, payload);
                let original = request.clone();
                let compatible = repair_v3_codex_compatible(&request, responses);
                assert_eq!(compatible, depth < 2);
                let context = semantic::Context {
                    config: &config,
                    credential: &config.credentials[0],
                    profile,
                    rules: &rules,
                    request: &request,
                    responses,
                    tokenizers: &tokenizers,
                };
                assert_eq!(context.compatibility("cloud") != "incompatible", compatible);
                let selection = context.resolve("hello", semantic::ModelResult::NotRun);
                assert_eq!(
                    selection.candidates.contains(&"cloud".to_owned()),
                    compatible
                );
                let recorded = decision_payload(r#"{"target":"cloud","confidence":0.95}"#);
                let selected = context.resolve("hello", semantic::ModelResult::Recorded(&recorded));
                assert_eq!(
                    selected.outcome,
                    if compatible { "model" } else { "invalid" }
                );
                let ruled = context.resolve("debug code", semantic::ModelResult::NotRun);
                assert_eq!(
                    ruled.outcome,
                    if compatible { "rule" } else { "incompatible" }
                );
                assert_eq!(request, original);
            }
        }
    }
}

#[tokio::test]
async fn repair_v2_codex_invalid_default_precedes_classifier_admission() {
    for responses in [false, true] {
        for extra_body in [false, true] {
            for on_failure in ["default", "error"] {
                let mut value = repair_v2_codex_config(true);
                value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] =
                    json!(on_failure);
                let runtime = runtime_from(value);
                let held = runtime
                    .semantic_admission
                    .clone()
                    .try_acquire_owned()
                    .unwrap();
                let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
                let response = semantic::dispatch(
                    runtime.clone(),
                    credential,
                    headers(),
                    repair_v2_codex_request(responses, extra_body),
                    responses,
                )
                .await;
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "known-invalid Codex default must reject before classifier admission"
                );
                assert_eq!(
                    response.headers()["x-slotline-semantic-outcome"],
                    "incompatible"
                );
                assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
                assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
                drop(held);
            }
        }
    }
}

#[test]
fn repair_v2_codex_invalid_candidates_cannot_be_selected() {
    for responses in [false, true] {
        for extra_body in [false, true] {
            let config: config::Config =
                serde_json::from_value(repair_v2_codex_config(false)).unwrap();
            let profile = &config.semantic_routing.profiles["auto"];
            let rules = semantic::compile_rules(profile).unwrap();
            let request = repair_v2_codex_request(responses, extra_body);
            let tokenizers = HashMap::new();
            let context = semantic::Context {
                config: &config,
                credential: &config.credentials[0],
                profile,
                rules: &rules,
                request: &request,
                responses,
                tokenizers: &tokenizers,
            };
            assert!(semantic::allowed_alias(
                &config,
                &config.credentials[0],
                "cloud"
            ));
            let selection = context.resolve("hello", semantic::ModelResult::NotRun);
            assert_eq!(
                selection.candidates,
                vec!["small"],
                "known-invalid Codex candidate must be hidden"
            );
            let recorded = decision_payload(r#"{"target":"cloud","confidence":0.95}"#);
            let selection = context.resolve("hello", semantic::ModelResult::Recorded(&recorded));
            assert_eq!(selection.outcome, "invalid");
            assert!(selection.target.is_none());
            let selection = context.resolve("debug code", semantic::ModelResult::NotRun);
            assert_eq!(selection.outcome, "incompatible");
            assert!(selection.target.is_none());
        }
    }
}

#[test]
fn repair_v2_codex_endpoint_validators_and_supported_runtime_deferral() {
    let config: config::Config = serde_json::from_value(repair_v2_codex_config(false)).unwrap();
    let profile = &config.semantic_routing.profiles["auto"];
    let rules = semantic::compile_rules(profile).unwrap();
    let tokenizers = HashMap::new();
    for (responses, request, expected) in [
        (
            false,
            json!({"model":"auto","messages":[{"role":"user","content":"hello"}],"reasoning_effort":"high"}),
            "static_compatible_runtime_deferred",
        ),
        (
            true,
            json!({"model":"auto","input":"hello","instructions":"be brief","service_tier":"default"}),
            "static_compatible_runtime_deferred",
        ),
        (
            true,
            json!({"model":"auto","input":"hello","extra_body":{"service_tier":"default"}}),
            "static_compatible_runtime_deferred",
        ),
        (
            true,
            json!({"model":"auto","input":"hello","max_output_tokens":16}),
            "incompatible",
        ),
        (
            true,
            json!({"model":"auto","input":"hello","service_tier":"invalid"}),
            "incompatible",
        ),
        (
            false,
            json!({"model":"auto","messages":[{"role":"user","content":"hello"}],"extra_body":false}),
            "incompatible",
        ),
    ] {
        let context = semantic::Context {
            config: &config,
            credential: &config.credentials[0],
            profile,
            rules: &rules,
            request: &request,
            responses,
            tokenizers: &tokenizers,
        };
        assert_eq!(context.compatibility("cloud"), expected);
        if expected != "incompatible" {
            assert_eq!(
                context
                    .resolve("hello", semantic::ModelResult::NotRun)
                    .candidates,
                vec!["small", "cloud"]
            );
        }
    }
}

#[test]
fn repair_v2_offline_key_store_matchers_defer_only_membership() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("policy.json");
    let mut value = configured();
    value["credentials"] = json!([]);
    value["key_store"] = json!(directory.path().join("nonexistent-store"));
    value["metadata_matchers"] = json!([{"id":"fixture","class":"unavailable-class","source":"user_agent","regex":"^synthetic$"}]);
    fs::write(&path, value.to_string()).unwrap();
    let loaded = config::load_static(&path);
    assert!(
        loaded.is_ok(),
        "offline key-store class membership is unavailable: {:?}",
        loaded.err()
    );
    assert!(!directory.path().join("nonexistent-store").exists());
    for regex in ["[".to_owned(), "x".repeat(257)] {
        value["metadata_matchers"][0]["regex"] = json!(regex);
        fs::write(&path, value.to_string()).unwrap();
        assert!(
            config::load_static(&path).is_err(),
            "offline regex validation must remain active"
        );
    }
}

#[test]
fn repair_v2_offline_inline_matchers_enforce_available_classes_and_regex() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("policy.json");
    let mut value = configured();
    value["credentials"][0]["key_file"] = json!(directory.path().join("nonexistent-key"));
    for (class, regex, valid) in [
        ("test", "^synthetic$".to_owned(), true),
        ("unknown", "^synthetic$".to_owned(), false),
        ("test", "[".to_owned(), false),
        ("test", "x".repeat(257), false),
    ] {
        value["metadata_matchers"] =
            json!([{"id":"fixture","class":class,"source":"user_agent","regex":regex}]);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(config::load_static(&path).is_ok(), valid);
    }
}

#[test]
fn increment_profile_options_are_validated_and_identity_bearing() {
    let original: config::Config = serde_json::from_value(configured()).unwrap();
    for (field, option) in [
        ("mode", json!("shadow")),
        ("compatibility_preflight", json!(true)),
        ("policy_version", json!("synthetic-v1")),
    ] {
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"][field] = option;
        let parsed = serde_json::from_value::<config::Config>(value);
        assert!(parsed.is_ok(), "supported profile option: {field}");
        let candidate = parsed.unwrap();
        semantic::validate(&candidate).unwrap();
        assert_ne!(
            audit_identity::digest(&original),
            audit_identity::digest(&candidate)
        );
    }
}

#[tokio::test]
async fn increment_preflight_rejects_default_before_classifier_admission() {
    let mut value = configured();
    value["semantic_routing"]["profiles"]["auto"]["compatibility_preflight"] = json!(true);
    value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!("error");
    value["aliases"]["small"]["capabilities"] = json!(["text"]);
    let parsed = serde_json::from_value::<config::Config>(value.clone());
    assert!(parsed.is_ok(), "preflight config must be accepted");
    let runtime = runtime_from(value);
    let held = runtime
        .semantic_admission
        .clone()
        .try_acquire_owned()
        .unwrap();
    let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
    let response = semantic::dispatch(runtime, credential, headers(), json!({
        "model":"auto", "messages":[{"role":"user","content":"hello"}],
        "extra_body":{"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]}
    }), false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(held);
}

#[test]
fn increment_versions_modes_and_unsafe_shadow_options_fail_load() {
    for (field, option) in [
        ("policy_version", json!("bad\nsecret")),
        ("policy_version", json!("x".repeat(65))),
        ("mode", json!("automatic")),
        ("shadow_classifier", json!(true)),
        ("shadow_baseline", json!("large")),
    ] {
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"][field] = option;
        let parsed = serde_json::from_value::<config::Config>(value);
        assert!(
            parsed.is_err() || semantic::validate(&parsed.unwrap()).is_err(),
            "{field}"
        );
    }
}

#[test]
fn increment_semantic_options_require_restart_without_changing_running_gate() {
    let current = runtime_from(configured());
    for (field, option) in [
        ("mode", json!("shadow")),
        ("policy_version", json!("release-v2")),
        ("compatibility_preflight", json!(true)),
    ] {
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"][field] = option;
        let candidate = runtime_from(value);
        assert!(
            validate_reload(&current.policy, &candidate.policy)
                .unwrap_err()
                .contains("restart")
        );
        assert_ne!(current.policy_digest, candidate.policy_digest);
        assert_eq!(current.semantic_admission.available_permits(), 1);
        assert_eq!(
            current.policy.config.semantic_routing.profiles["auto"].mode,
            semantic::Mode::Active
        );
    }
}

#[test]
fn increment_preflight_uses_full_normalized_contract_and_authorized_routes() {
    let mut value = configured();
    value["aliases"]["small"]["streaming"] = json!(false);
    value["aliases"]["small"]["capabilities"] = json!(["text"]);
    let config: config::Config = serde_json::from_value(value).unwrap();
    let profile = &config.semantic_routing.profiles["auto"];
    let rules = semantic::compile_rules(profile).unwrap();
    let tokenizers = HashMap::new();
    for request in [
        json!({"model":"auto","messages":[{"role":"user","content":"hello"}],"extra_body":{"stream":true}}),
        json!({"model":"auto","messages":[{"role":"user","content":"hello"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]}),
        json!({"model":"auto","messages":[{"role":"user","content":"hello"}],"response_format":{"type":"json_schema","json_schema":{"name":"result","schema":{"type":"object"}}}}),
        json!({"model":"auto","messages":[{"role":"invalid","content":"history"},{"role":"user","content":"hello"}]}),
        json!({"model":"auto","messages":[{"role":"user","content":"x".repeat(10000)}]}),
        json!({"model":"auto","messages":[{"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]}]}),
    ] {
        let context = semantic::Context {
            config: &config,
            credential: &config.credentials[0],
            profile,
            rules: &rules,
            request: &request,
            responses: false,
            tokenizers: &tokenizers,
        };
        assert_eq!(context.compatibility("small"), "incompatible");
    }
}

#[test]
fn increment_static_configuration_never_reads_credentials_or_tokenizers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("policy.json");
    let mut value = configured();
    value["credentials"][0]["key_file"] = json!("/nonexistent/PRIVATE_KEY");
    value["routes"][0]["credential_file"] = json!("/nonexistent/PRIVATE_UPSTREAM");
    value["aliases"]["small"]["tokenizer"] =
        json!({"file":"/nonexistent/PRIVATE_TOKENIZER", "sha256":"a".repeat(64)});
    fs::write(&path, value.to_string()).unwrap();
    assert!(config::load_static(&path).is_ok());
    assert!(config::load(&path).is_err());
    value["credentials"] = json!([]);
    value["key_store"] = json!("/nonexistent/PRIVATE_STORE");
    fs::write(&path, value.to_string()).unwrap();
    assert!(config::load_static(&path).is_ok());
}

#[test]
fn increment_native_provider_and_unloaded_tokenizer_keep_runtime_deferred() {
    let mut value = configured();
    value["codex"] = json!({"enabled":true,"alias":"cloud","model":"synthetic"});
    value["aliases"]["small"]["tokenizer"] =
        json!({"file":"/synthetic/absent-tokenizer","sha256":"a".repeat(64)});
    let config: config::Config = serde_json::from_value(value).unwrap();
    let profile = &config.semantic_routing.profiles["auto"];
    let rules = semantic::compile_rules(profile).unwrap();
    let request = json!({"model":"auto","messages":[{"role":"user","content":"hello"}]});
    let tokenizers = HashMap::new();
    let context = semantic::Context {
        config: &config,
        credential: &config.credentials[0],
        profile,
        rules: &rules,
        request: &request,
        responses: false,
        tokenizers: &tokenizers,
    };
    assert_eq!(context.compatibility("small"), "context_deferred");
    assert_eq!(
        context.compatibility("cloud"),
        "static_compatible_runtime_deferred"
    );
}

fn cloud_configured(responses: bool, branch: &str, missing_oauth: &std::path::Path) -> Value {
    let mut value = configured();
    value["copilot"] = json!({"enabled":true,"auth_source":"oauth_token_file",
        "oauth_token_file":missing_oauth,
        "models":[{"alias":"cloud","model":"synthetic-cloud",
        "endpoint":if responses { "responses" } else { "chat_completions" }}]});
    value["credentials"][0]["cloud_allowed"] = json!(true);
    value["credentials"][0]["aliases"]
        .as_array_mut()
        .unwrap()
        .push(json!("cloud"));
    value["credentials"][0]["allowed_routes"]
        .as_array_mut()
        .unwrap()
        .push(json!("copilot"));
    let profile = &mut value["semantic_routing"]["profiles"]["auto"];
    profile["targets"] = json!(["cloud"]);
    profile["default_target"] = json!("cloud");
    profile["rules"] = if branch == "rule" {
        json!([{"id":"cloud-rule","priority":1,"target":"cloud","keywords":["hello"]}])
    } else {
        json!([])
    };
    if branch == "default" {
        profile.as_object_mut().unwrap().remove("decision");
    }
    value
}

#[test]
fn semantic_request_policy_overrides_are_rejected_after_normalization() {
    for field in ["cloud_allowed", "local_only"] {
        for nested in [false, true] {
            for value in [json!(false), json!(true), json!(null), json!("false")] {
                let mut request = json!({"model":"auto","input":"hello","store":false});
                if nested {
                    request["extra_body"] = json!({field:value});
                } else {
                    request[field] = value;
                }
                let original = request.clone();
                let result = semantic::inspect_request(&request);
                assert!(
                    result.is_err(),
                    "accepted unsupported policy field: {request}"
                );
                assert_eq!(result.err().unwrap().status(), StatusCode::BAD_REQUEST);
                assert_eq!(request, original);
            }
        }
    }
}

#[test]
fn semantic_request_inspection_preserves_supported_fields_and_nested_schema_names() {
    let request = json!({"model":"auto","input":"hello","store":false,
        "tools":[{"type":"function","name":"lookup","parameters":{"type":"object",
            "properties":{"cloud_allowed":{"type":"boolean"},"local_only":{"type":"boolean"}}}}],
        "extra_body":{"temperature":0.5}});
    let mut expected = request.clone();
    expected.as_object_mut().unwrap().remove("extra_body");
    expected["temperature"] = json!(0.5);
    assert_eq!(semantic::inspect_request(&request).ok().unwrap(), expected);
    assert!(request.get("extra_body").is_some());
}

#[tokio::test]
async fn semantic_original_preservation_fixture_metadata_is_rejected_without_generation() {
    for model in ["small", "auto"] {
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"]
            .as_object_mut()
            .unwrap()
            .remove("decision");
        let runtime = runtime_from(value);
        let request = json!({"model":model,"messages":[
            {"role":"system","content":"debug is not user text"},{"role":"user","content":"hello"}],
            "max_tokens":16,"top_k":5,"metadata":{"synthetic":"preserved"},
            "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}],
            "response_format":{"type":"json_schema","json_schema":{"name":"result","schema":{"type":"object","properties":{}}}}});
        let response = handlers::chat(
            State(runtime.clone()),
            headers(),
            Bytes::from(request.to_string()),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        println!(
            "original metadata fixture model={model} status={status} body={}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert_eq!(
            body["error"]["message"],
            "Unsupported model argument: metadata"
        );
        assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn semantic_default_fallback_cannot_bypass_quarantined_shared_route() {
    let runtime = runtime_from(configured());
    assert_eq!(
        runtime.observations.lock().unwrap()[0].status,
        RouteStatus::Serving
    );
    mark_failure(&runtime, 0);
    let held = runtime
        .semantic_admission
        .clone()
        .try_acquire_owned()
        .unwrap();
    let response = handlers::chat(
        State(runtime.clone()),
        headers(),
        Bytes::from(
            json!({"model":"auto","messages":[{"role":"user","content":"hello"}],"max_tokens":16})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-slotline-semantic-outcome"], "busy");
    assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    println!(
        "quarantined shared fallback: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "no_serving_capacity");
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].status, RouteStatus::CoolingDown);
    assert!(observations[0].qualified.is_none());
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    drop(held);
}

#[tokio::test]
async fn semantic_request_cloud_denial_precedes_even_busy_decision_admission() {
    for responses in [false, true] {
        for field in ["cloud_allowed", "local_only"] {
            for nested in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut value = cloud_configured(
                    responses,
                    "fallback",
                    &directory.path().join("absent-oauth"),
                );
                value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] =
                    json!("error");
                let runtime = runtime_from(value);
                let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
                assert!(semantic::allowed_alias(
                    &runtime.policy.config,
                    &credential.config,
                    "cloud"
                ));
                let held = runtime
                    .semantic_admission
                    .clone()
                    .try_acquire_owned()
                    .unwrap();
                let mut request = if responses {
                    json!({"model":"auto","input":"hello","store":false})
                } else {
                    json!({"model":"auto","messages":[{"role":"user","content":"hello"}]})
                };
                let denial = json!(field == "local_only");
                if nested {
                    request["extra_body"] = json!({field:denial});
                } else {
                    request[field] = denial;
                }
                let response = if responses {
                    handlers::responses(
                        State(runtime.clone()),
                        headers(),
                        Bytes::from(request.to_string()),
                    )
                    .await
                } else {
                    handlers::chat(
                        State(runtime.clone()),
                        headers(),
                        Bytes::from(request.to_string()),
                    )
                    .await
                };
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "{request}: {}",
                    String::from_utf8_lossy(&bytes)
                );
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["error"]["code"], "unsupported_parameter");
                assert!(
                    runtime
                        .route_attempts
                        .iter()
                        .all(|counter| counter.load(Ordering::Relaxed) == 0)
                );
                assert_eq!(credential.class_semaphore.available_permits(), 1);
                assert_eq!(runtime.semantic_admission.available_permits(), 0);
                drop(held);
            }
        }
    }
}

#[test]
fn semantic_config_accepts_opt_in_rules_and_decision_model() {
    let mut value = minimal_config();
    value["semantic_routing"] = json!({
        "enabled":true,
        "profiles":{"auto":{"targets":["small","large"],"default_target":"small",
            "rules":[{"id":"code","priority":10,"target":"large","regex":"(?i)debug"}],
            "decision":{"classifier":"judge","confidence_threshold":0.8,
                "timeout_ms":500,"max_output_tokens":64,"on_failure":"default"}}}
    });
    let parsed = serde_json::from_value::<config::Config>(value);
    assert!(parsed.is_ok(), "operator semantic configuration must parse");
}

#[test]
fn semantic_config_absent_keeps_legacy_configuration() {
    assert!(serde_json::from_value::<config::Config>(minimal_config()).is_ok());
}

#[test]
fn semantic_rules_priority_order_unicode_case_and_default() {
    let profile: semantic::Profile = serde_json::from_value(json!({
        "targets":["small","large"],"default_target":"small","rules":[
            {"id":"first","priority":2,"target":"large","keywords":["DEBUG"]},
            {"id":"tie","priority":2,"target":"small","regex":"debug"},
            {"id":"lower","priority":1,"target":"small","regex":".*"}
        ]
    }))
    .unwrap();
    let rules = semantic::compile_rules(&profile).unwrap();
    assert_eq!(
        semantic::match_rule(&rules, "debug a function"),
        Some(("first", "large"))
    );
    assert_eq!(
        semantic::match_rule(&rules, "hello"),
        Some(("lower", "small"))
    );
    assert_eq!(semantic::match_rule(&rules, ""), Some(("lower", "small")));
}

#[test]
fn semantic_extraction_never_uses_system_tool_or_assistant_text() {
    let request = json!({"messages":[
        {"role":"system","content":"debug"},
        {"role":"user","content":"old"},
        {"role":"assistant","content":"debug"},
        {"role":"user","content":[{"type":"text","text":"hello"}]},
        {"role":"tool","content":"debug"}
    ]});
    assert_eq!(semantic::latest_text(&request, false, 64).unwrap(), "hello");
    assert!(
        semantic::latest_text(
            &json!({"messages":[{"role":"system","content":"hello"}]}),
            false,
            64
        )
        .is_err()
    );
    assert!(semantic::latest_text(&request, false, 4).is_err());
}

#[test]
fn semantic_strict_output_rejects_untrusted_and_low_confidence_decisions() {
    for (content, reason) in [
        (r#"{"target":"small","confidence":0.79}"#, "low_confidence"),
        (
            r#"{"target":"forbidden-private-alias","confidence":1}"#,
            "invalid",
        ),
        (
            r#"{"target":"https://arbitrary.invalid","confidence":1}"#,
            "invalid",
        ),
        (r#"{"target":"small","confidence":2}"#, "invalid"),
        (r#"{"target":"small","confidence":1e999}"#, "invalid"),
        (
            r#"{"target":"small","confidence":0.9,"instructions":"ignore"}"#,
            "invalid",
        ),
        (
            r#"{"target":"small","target":"large","confidence":1}"#,
            "invalid",
        ),
        ("```json\n{}\n```", "invalid"),
        ("PRIVATE_CLASSIFIER_SENTINEL", "invalid"),
    ] {
        assert_eq!(
            semantic::parse_decision(&decision_payload(content), &["small"], 0.8),
            Err(reason)
        );
    }
    assert_eq!(
        semantic::parse_decision(
            &decision_payload(r#"{"target":"small","confidence":0.8}"#),
            &["small"],
            0.8
        )
        .unwrap(),
        "small"
    );
    for field in ["tool_calls", "function_call", "refusal"] {
        let mut payload = decision_payload(r#"{"target":"small","confidence":1}"#);
        payload["choices"][0]["message"][field] = json!("no");
        assert!(semantic::parse_decision(&payload, &["small"], 0.8).is_err());
    }
    let mut payload = decision_payload(r#"{"target":"small","confidence":1}"#);
    payload["choices"][0]["finish_reason"] = json!("length");
    assert!(semantic::parse_decision(&payload, &["small"], 0.8).is_err());
}

#[test]
fn semantic_config_rejects_cycles_unsafe_identifiers_and_unbounded_work() {
    let valid: config::Config = serde_json::from_value(configured()).unwrap();
    semantic::validate(&valid).unwrap();
    for edit in [
        "cycle",
        "classifier_cycle",
        "collision",
        "missing",
        "passthrough",
        "embedding",
        "cloud_classifier",
        "rule_target",
        "regex",
        "duplicate",
        "keywords_and_regex",
        "empty_matcher",
        "rule_count",
        "target_count",
        "profile_count",
        "input_limit",
        "capacity",
        "timeout",
        "tokens",
        "threshold",
        "identifier",
    ] {
        let mut value = configured();
        let profile = &mut value["semantic_routing"]["profiles"]["auto"];
        match edit {
            "cycle" => {
                profile["targets"] = json!(["auto"]);
                profile["default_target"] = json!("auto");
            }
            "classifier_cycle" => profile["decision"]["classifier"] = json!("auto"),
            "collision" => {
                value["aliases"]["auto"] = value["aliases"]["small"].clone();
            }
            "missing" => profile["decision"]["classifier"] = json!("missing"),
            "passthrough" => value["aliases"]["small"]["passthrough_route"] = json!("native"),
            "embedding" => value["aliases"]["judge"]["endpoint"] = json!("embedding"),
            "cloud_classifier" => {
                value["aliases"].as_object_mut().unwrap().remove("judge");
                value["codex"] = json!({"enabled":true,"alias":"judge","model":"synthetic"});
            }
            "rule_target" => profile["rules"][0]["target"] = json!("judge"),
            "regex" => profile["rules"][0]["regex"] = json!("["),
            "duplicate" => profile["rules"] = json!([profile["rules"][0], profile["rules"][0]]),
            "keywords_and_regex" => profile["rules"][0]["keywords"] = json!(["debug"]),
            "empty_matcher" => {
                profile["rules"][0].as_object_mut().unwrap().remove("regex");
            }
            "rule_count" => profile["rules"] = json!(vec![profile["rules"][0].clone(); 33]),
            "target_count" => profile["targets"] = json!(vec!["small"; 33]),
            "profile_count" => {
                let clone = profile.clone();
                value["semantic_routing"]["profiles"] = json!(
                    (0..17)
                        .map(|index| (format!("auto{index}"), clone.clone()))
                        .collect::<HashMap<_, _>>()
                );
            }
            "input_limit" => value["semantic_routing"]["max_input_bytes"] = json!(16385),
            "capacity" => value["semantic_routing"]["max_in_flight"] = json!(17),
            "timeout" => profile["decision"]["timeout_ms"] = json!(30001),
            "tokens" => profile["decision"]["max_output_tokens"] = json!(257),
            "threshold" => profile["decision"]["confidence_threshold"] = json!(1.1),
            "identifier" => profile["rules"][0]["id"] = json!("unsafe\r\nHeader"),
            _ => unreachable!(),
        }
        let config: config::Config = serde_json::from_value(value).unwrap();
        assert!(semantic::validate(&config).is_err(), "accepted {edit}");
    }
}

#[test]
fn semantic_case_defaults_and_rule_bounds() {
    let mut profile: semantic::Profile =
        serde_json::from_value(json!({"targets":["small"],"default_target":"small",
        "rules":[{"id":"unicode","priority":0,"target":"small","keywords":["ΔΕΛΤΑ","literal.*"]}]}))
        .unwrap();
    let rules = semantic::compile_rules(&profile).unwrap();
    assert_eq!(
        semantic::match_rule(&rules, "δελτα"),
        Some(("unicode", "small"))
    );
    assert!(semantic::match_rule(&rules, "literal-not-regex").is_none());
    profile.rules[0].case_sensitive = true;
    assert!(semantic::match_rule(&semantic::compile_rules(&profile).unwrap(), "δελτα").is_none());
    profile.rules.clear();
    assert!(
        semantic::match_rule(&semantic::compile_rules(&profile).unwrap(), "anything").is_none()
    );
}

#[test]
fn semantic_rules_observe_literal_newline_join_even_after_empty_first_text_part() {
    let request = json!({"messages":[{"role":"user","content":[{"type":"text","text":""},{"type":"text","text":"hello"}]}]});
    let text = semantic::latest_text(&request, false, 6).unwrap();
    let profile: semantic::Profile =
        serde_json::from_value(json!({"targets":["small","large"],"default_target":"small",
        "rules":[{"id":"newline","priority":1,"target":"large","regex":"^\\nhello$"}]}))
        .unwrap();
    let rules = semantic::compile_rules(&profile).unwrap();
    assert_eq!(
        semantic::match_rule(&rules, &text),
        Some(("newline", "large"))
    );
    assert!(semantic::latest_text(&request, false, 5).is_err());
    assert!(semantic::latest_text(&json!({"messages":[{"role":"user","content":[{"type":"text","text":""},{"type":"text","text":""}]}]}),false,6).is_err());
}

#[test]
fn semantic_responses_extraction_and_continuations_fail_closed() {
    assert_eq!(
        semantic::latest_text(&json!({"input":"hello","instructions":"debug"}), true, 5).unwrap(),
        "hello"
    );
    assert_eq!(semantic::latest_text(&json!({"input":[{"role":"system","content":"debug"},{"type":"message","role":"user","content":[{"type":"input_text","text":"one"},{"type":"input_text","text":"two"}]}]}), true, 7).unwrap(), "one\ntwo");
    for request in [
        json!({"input":[{"content":"not explicitly user"}]}),
        json!({"input":"hello","previous_response_id":null}),
        json!({"input":"hello","conversation":"private"}),
        json!({"input":"hello","store":true}),
        json!({"input":"hello","background":true}),
        json!({"input":[{"role":"user","content":"hello"},{"type":"function_call_output","output":"debug"}]}),
    ] {
        assert!(semantic::latest_text(&request, true, 64).is_err());
    }
}

fn unsupported_responses_items() -> Vec<Value> {
    vec![
        json!({"id":"msg_previous"}),
        json!({"id":"msg_previous","role":"user","content":"hello"}),
        json!({"id":"msg_previous","role":"assistant","content":"hello","status":"completed"}),
        json!({"type":"item_reference","id":"msg_previous"}),
        json!({"type":"function_call_output","call_id":"old","output":"hello"}),
        json!({"type":"message","id":"msg_previous"}),
        json!({"type":"message","role":"user"}),
        json!({"type":"message","content":"hello"}),
        json!({"content":"hello"}),
        json!({"role":"assistant","content":"hello","item_reference":"old"}),
        json!({"type":null,"role":"user","content":"hello"}),
        json!({"type":17,"role":"user","content":"hello"}),
        json!({"type":false,"role":"user","content":"hello"}),
        json!({"type":"","role":"user","content":"hello"}),
        json!({"type":{},"role":"user","content":"hello"}),
        json!({"role":null,"content":"hello"}),
        json!({"role":[],"content":"hello"}),
        json!({"role":"tool","content":"hello"}),
        json!({"role":"user","content":null}),
        json!({"role":"user","content":{ "text":"hello" }}),
        json!({"role":"user","content":42}),
        json!({"role":"user","content":[]}),
        json!({"role":"user","content":[null]}),
        json!({"role":"user","content":[{"type":"input_text","text":null}]}),
        json!({"role":"user","content":[{"type":null,"text":"hello"}]}),
        json!({"role":"user","content":[{"type":"input_text","text":"hello","id":"old"}]}),
        json!({"role":"user","content":[{"type":"item_reference","id":"old"}]}),
        json!({"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/image"}]}),
        json!({"type":"message","id":null,"role":"user","content":"hello"}),
        json!({"type":"message","id":"","role":"user","content":"hello"}),
        json!({"type":"message","id":"x".repeat(129),"role":"user","content":"hello"}),
        json!({"type":"message","status":null,"role":"user","content":"hello"}),
        json!({"type":"message","status":"unknown","role":"user","content":"hello"}),
        json!(null),
        json!("hello"),
        json!([]),
    ]
}

fn supported_responses_messages() -> Vec<Value> {
    vec![
        json!({"role":"user","content":"hello"}),
        json!({"type":"message","role":"user","content":"hello"}),
        json!({"type":"message","id":"msg_self_contained","status":"completed","role":"user","content":[{"type":"input_text","text":"hello"}]}),
    ]
}

#[test]
fn semantic_responses_validates_every_complete_message_after_normalization() {
    for item in unsupported_responses_items() {
        for last in [false, true] {
            for nested in [false, true] {
                let user = json!({"role":"user","content":"hello"});
                let input = if last {
                    json!([user, item])
                } else {
                    json!([item, user])
                };
                let request = if nested {
                    json!({"extra_body":{"input":input}})
                } else {
                    json!({"input":input})
                };
                let original = request.clone();
                let inspection = semantic::inspect_request(&request).unwrap();
                assert!(
                    semantic::latest_text(&inspection, true, 64).is_err(),
                    "accepted {request}"
                );
                assert_eq!(request, original);
            }
        }
    }
}

#[test]
fn semantic_responses_complete_messages_and_literal_ids_remain_supported() {
    for message in supported_responses_messages() {
        let request = json!({"input":[
            {"role":"system","content":"instructions"},
            {"type":"message","id":"msg_history","role":"assistant","content":[{"type":"output_text","text":"prior reply"}]},
            message
        ]});
        assert_eq!(semantic::latest_text(&request, true, 64).unwrap(), "hello");
    }
    for input in [
        json!(r#"{"id":"msg_previous"}"#),
        json!([{"role":"user","content":r#"{"id":"msg_previous"}"#}]),
    ] {
        let request = json!({"input":input,"text":{"format":{"type":"json_schema","schema":{"properties":{"id":{"type":"string"}}}}}});
        assert_eq!(
            semantic::latest_text(&request, true, 64).unwrap(),
            r#"{"id":"msg_previous"}"#
        );
    }
}

#[test]
fn semantic_status_separates_scoped_selector_grants_and_physical_routes() {
    for enabled in [false, true] {
        let mut value = configured();
        value["semantic_routing"]["enabled"] = json!(enabled);
        value["semantic_routing"]["profiles"]["other"] =
            value["semantic_routing"]["profiles"]["auto"].clone();
        let runtime = runtime_from(value);
        let status = status_payload(&runtime);
        assert_eq!(status["semantic_routing"]["enabled"], enabled);
        assert_eq!(
            status["semantic_routing"]["selectors"],
            json!(["auto", "other"])
        );
        assert_eq!(
            status["semantic_routing"]["decision_admission"],
            json!({"limit":1,"in_flight":0,"available":1,"closed":false})
        );
        assert_eq!(
            status["semantic_routing"]["profiles"]["auto"]["mode"],
            "active"
        );
        assert_eq!(
            status["semantic_routing"]["policy_sha256"],
            runtime.policy_digest
        );
        assert_eq!(
            status["credentials"]["fixture"]["semantic_selectors"],
            json!(["auto"])
        );
        assert!(
            status["credentials"]["fixture"]["aliases"]
                .get("auto")
                .is_none()
        );
        assert_eq!(
            status["credentials"]["fixture"]["aliases"]["small"][0]["route"],
            "native"
        );
        assert_eq!(status["routes"].as_object().unwrap().len(), 1);
        let output = status_cli::render(&status).unwrap();
        assert!(output.contains(if enabled {
            "Semantic routing: enabled"
        } else {
            "Semantic routing: disabled"
        }));
        assert!(output.contains("Configured selectors: auto, other"));
        assert!(output.contains("Selector grants: auto"));
        assert!(!output.contains("auto ->"));
        assert!(!status["semantic_routing"].to_string().contains("judge"));
        for secret in [
            "synthetic-semantic-test-key",
            "confidence_threshold",
            "code|debug",
            "oauth_token_file",
        ] {
            assert!(!status["semantic_routing"].to_string().contains(secret));
            assert!(!output.contains(secret));
        }
    }
    let status = status_payload(&runtime_from(minimal_config()));
    assert_eq!(status["semantic_routing"]["enabled"], false);
    assert_eq!(status["semantic_routing"]["selectors"], json!([]));
    assert!(
        status_cli::render(&status)
            .unwrap()
            .contains("Configured selectors: (none)")
    );
    let mut value = configured();
    value["credentials"][0]["aliases"] = json!(["small"]);
    let status = status_payload(&runtime_from(value));
    assert_eq!(
        status["credentials"]["fixture"]["semantic_selectors"],
        json!([])
    );
    assert!(
        status_cli::render(&status)
            .unwrap()
            .contains("Selector grants: (none)")
    );
}

#[test]
fn semantic_status_tracks_shared_live_gate_across_unrelated_reload() {
    let mut value = configured();
    value["routes"][0]["model"] = json!("qwen-synthetic");
    let runtime = runtime_from(value.clone());
    let held = runtime
        .semantic_admission
        .clone()
        .try_acquire_owned()
        .unwrap();
    value["refresh_seconds"] = json!(57);
    let candidate = runtime_from(value);
    validate_reload(&runtime.policy, &candidate.policy).unwrap();
    let candidate = Arc::try_unwrap(candidate).ok().unwrap();
    let reloaded = make_runtime(candidate.policy, Some(&runtime)).unwrap();
    assert!(Arc::ptr_eq(
        &runtime.semantic_admission,
        &reloaded.semantic_admission
    ));
    for current in [&runtime, &reloaded] {
        assert_eq!(
            status_payload(current)["semantic_routing"]["decision_admission"],
            json!({"limit":1,"in_flight":1,"available":0,"closed":false})
        );
        assert!(
            status_cli::render(&status_payload(current))
                .unwrap()
                .contains("Decision admission: 1/1 in flight, 0 available, open")
        );
    }
    drop(held);
    assert_eq!(
        status_payload(&reloaded)["semantic_routing"]["decision_admission"]["in_flight"],
        0
    );
    runtime.semantic_admission.close();
    assert_eq!(
        status_payload(&reloaded)["semantic_routing"]["decision_admission"]["closed"],
        true
    );
}

#[test]
fn semantic_status_cli_renders_feature_admission_and_selector_grants() {
    let runtime = runtime_from(configured());
    let mut status = status_payload(&runtime);
    status["semantic_routing"] = json!({"enabled":true,"selectors":["auto"],"decision_admission":{"limit":1,"in_flight":1,"available":0,"closed":false}});
    status["credentials"]["fixture"]["semantic_selectors"] = json!(["auto"]);
    let output = status_cli::render(&status).unwrap();
    assert!(output.contains("Semantic routing: enabled"));
    assert!(output.contains("Configured selectors: auto"));
    assert!(output.contains("Decision admission: 1/1 in flight, 0 available, open"));
    assert!(output.contains("Selector grants: auto"));
    assert!(output.contains("small -> native"));
}

#[tokio::test]
async fn semantic_status_remains_operator_only() {
    let denied = status(State(runtime_from(configured())), headers()).await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let mut value = configured();
    value["credentials"][0]["operator"] = json!(true);
    let runtime = runtime_from(value);
    assert_eq!(
        status(State(runtime.clone()), HeaderMap::new())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = status(State(runtime), headers()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["semantic_routing"]["enabled"], true);
    assert_eq!(
        body["credentials"]["fixture"]["semantic_selectors"],
        json!(["auto"])
    );
}

#[tokio::test]
async fn semantic_responses_rejects_unsupported_items_before_busy_gate_or_cloud_work() {
    for branch in ["rule", "default", "model", "fallback"] {
        let directory = tempfile::tempdir().unwrap();
        let mut value = cloud_configured(true, branch, &directory.path().join("absent-oauth"));
        value["credentials"][0]["burst"] = json!(2048);
        let runtime = runtime_from(value);
        let held = runtime
            .semantic_admission
            .clone()
            .try_acquire_owned()
            .unwrap();
        for item in unsupported_responses_items() {
            for nested in [false, true] {
                let input = json!([item, {"role":"user","content":"hello"}]);
                let mut request = json!({"model":"auto","input":"hello","store":false});
                if nested {
                    request.as_object_mut().unwrap().remove("input");
                    request["extra_body"] = json!({"input":input});
                } else {
                    request["input"] = input;
                }
                let response = responses(
                    State(runtime.clone()),
                    headers(),
                    Bytes::from(request.to_string()),
                )
                .await;
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "branch={branch} request={request}"
                );
                assert!(
                    response
                        .headers()
                        .get("x-slotline-semantic-outcome")
                        .is_none()
                );
                assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
                assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
            }
        }
        let duplicate = json!({"model":"auto","input":"hello","store":false,"extra_body":{"input":[{"id":"msg_previous"},{"role":"user","content":"hello"}]}});
        let response = responses(
            State(runtime.clone()),
            headers(),
            Bytes::from(duplicate.to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
        assert_eq!(runtime.semantic_admission.available_permits(), 0);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        drop(held);
    }
}

#[test]
fn semantic_alias_grants_routes_and_cloud_permission_are_independent() {
    let mut value = configured();
    value["codex"] = json!({"enabled":true,"alias":"cloud","model":"synthetic"});
    let config: config::Config = serde_json::from_value(value).unwrap();
    let mut credential = config.credentials[0].clone();
    assert!(semantic::allowed_alias(&config, &credential, "small"));
    assert!(!semantic::allowed_alias(&config, &credential, "cloud"));
    credential.aliases.push("cloud".into());
    assert!(!semantic::allowed_alias(&config, &credential, "cloud"));
    credential.cloud_allowed = true;
    assert!(!semantic::allowed_alias(&config, &credential, "cloud"));
    credential.allowed_routes.push("codex".into());
    assert!(semantic::allowed_alias(&config, &credential, "cloud"));
    credential.allowed_routes = vec!["forbidden".into()];
    assert!(!semantic::allowed_alias(&config, &credential, "small"));
}

#[tokio::test]
async fn semantic_unauthenticated_scope_and_default_denial_never_dispatch() {
    for missing in ["auth", "auto", "judge", "small", "routes"] {
        let mut value = configured();
        if missing == "routes" {
            value["credentials"][0]["allowed_routes"] = json!(["ungranted"]);
        } else if missing != "auth" {
            value["credentials"][0]["aliases"]
                .as_array_mut()
                .unwrap()
                .retain(|name| name != missing);
        }
        let runtime = runtime_from(value);
        let response = chat(
            State(runtime.clone()),
            if missing == "auth" {
                HeaderMap::new()
            } else {
                headers()
            },
            Bytes::from(
                json!({"model":"auto","messages":[{"role":"user","content":"hello"}]}).to_string(),
            ),
        )
        .await;
        assert_eq!(
            response.status(),
            if missing == "auth" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            },
            "{missing}"
        );
        assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn semantic_busy_gate_fails_without_class_permit_or_classifier_rate_charge() {
    let mut value = configured();
    value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!("error");
    let runtime = runtime_from(value);
    let held = runtime
        .semantic_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let response = chat(
        State(runtime.clone()),
        headers(),
        Bytes::from(
            json!({"model":"auto","messages":[{"role":"user","content":"hello"}]}).to_string(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-slotline-semantic-outcome"], "busy");
    let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
    assert_eq!(credential.class_semaphore.available_permits(), 1);
    assert!(credential.rate.lock().unwrap().tokens >= 99.0);
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    drop(held);
}

#[test]
fn semantic_reload_is_atomic_immutable_and_digest_tracks_rules() {
    let runtime = runtime_from(configured());
    let mut value = configured();
    value["semantic_routing"]["profiles"]["auto"]["rules"][0]["priority"] = json!(99);
    let candidate = runtime_from(value);
    assert!(
        validate_reload(&runtime.policy, &candidate.policy)
            .unwrap_err()
            .contains("restart")
    );
    assert_ne!(runtime.policy_digest, candidate.policy_digest);
    assert_eq!(runtime.semantic_admission.available_permits(), 1);
}

#[tokio::test]
async fn semantic_extra_body_continuation_is_rejected_before_classifier() {
    let runtime = runtime_from(configured());
    let request = json!({"model":"auto","input":"hello","extra_body":{"previous_response_id":"private-state"}});
    let response = responses(
        State(runtime.clone()),
        headers(),
        Bytes::from(request.to_string()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn semantic_rule_and_classifier_route_grants_are_checked_before_any_work() {
    for classifier in [true, false] {
        let mut value = configured();
        let mut other = value["routes"][0].clone();
        other["name"] = json!("ungranted");
        value["routes"].as_array_mut().unwrap().push(other);
        value["aliases"][if classifier { "judge" } else { "large" }]["routes"] =
            json!([{"route":"ungranted","priority":1}]);
        let runtime = runtime_from(value);
        let request = json!({"model":"auto","messages":[{"role":"user","content":if classifier { "hello" } else { "DEBUG code" }}]});
        let response = chat(
            State(runtime.clone()),
            headers(),
            Bytes::from(request.to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            runtime
                .route_attempts
                .iter()
                .all(|counter| counter.load(Ordering::Relaxed) == 0)
        );
    }
}

#[tokio::test]
async fn semantic_base_dispatch_retains_copilot_chat_endpoint_guard() {
    let mut value = configured();
    value["copilot"] = json!({"enabled":true,"models":[{"alias":"cloud-responses","model":"synthetic","endpoint":"responses"}]});
    value["credentials"][0]["aliases"]
        .as_array_mut()
        .unwrap()
        .push(json!("cloud-responses"));
    let runtime = runtime_from(value);
    assert!(!runtime.policy.config.credentials[0].cloud_allowed);
    let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
    let response = handlers::chat_base(
        runtime.clone(),
        credential,
        headers(),
        Bytes::from(
            json!({"model":"cloud-responses","messages":[{"role":"user","content":"hello"}]})
                .to_string(),
        ),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn semantic_classifier_rate_is_separate_from_single_generation_admission() {
    let mut value = configured();
    value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!("error");
    let runtime = runtime_from(value);
    let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
    credential.rate.lock().unwrap().tokens = 1.0;
    let response = chat(
        State(runtime.clone()),
        headers(),
        Bytes::from(
            json!({"model":"auto","messages":[{"role":"user","content":"hello"}]}).to_string(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()["x-slotline-semantic-outcome"],
        "rate_limited"
    );
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semantic_admission.available_permits(), 1);
}

#[test]
fn semantic_selector_grants_work_in_file_and_key_store_policies() {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("fixture.key");
    fs::write(&key, b"synthetic-semantic-test-key-1234567890").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let mut value = configured();
    value["credentials"][0]["key_file"] = json!(key);
    let policy_file = directory.path().join("policy.json");
    for enabled in [true, false] {
        value["semantic_routing"]["enabled"] = json!(enabled);
        fs::write(&policy_file, value.to_string()).unwrap();
        let policy = config::load(&policy_file).unwrap();
        let credential = &policy.config.credentials[0];
        let key_policy: key_manager::KeyPolicy = serde_json::from_value(json!({
            "id":"synthetic","class":credential.class,"aliases":credential.aliases,
            "allowed_routes":credential.allowed_routes,"requests_per_minute":60,"burst":10,"max_in_flight":1
        })).unwrap();
        let snapshot = HashMap::from([(
            "synthetic".to_owned(),
            key_manager::SnapshotKey {
                verifier: String::new(),
                policy: key_policy,
                expires_at: None,
            },
        )]);
        assert!(config::key_credentials_from_snapshot(&policy.config, &snapshot).is_ok());
    }
}
