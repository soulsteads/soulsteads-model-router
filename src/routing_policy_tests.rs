use super::*;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderValue, Request, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};
use tower::ServiceExt;

#[path = "routing_policy_audit_tests.rs"]
mod audit_tests;

const WORKLOAD_KEY: &str = "routing-policy-fixture-workload-key-0123456789";

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DecisionReply {
    #[default]
    Valid,
    Malformed,
    UnknownTarget,
    Truncated,
    Duplicate,
    ExtraField,
    ToolCall,
    Timeout,
}

#[derive(Default)]
struct UpstreamState {
    requests: Mutex<Vec<(String, Value)>>,
    decision_calls: AtomicUsize,
    target_calls: AtomicUsize,
    decision_reply: Mutex<DecisionReply>,
    decision_target: Mutex<String>,
    decision_usage: Mutex<Option<Value>>,
    target_usage: Mutex<Option<Value>>,
    decision_status: Mutex<Option<StatusCode>>,
    failing_target: Mutex<Option<String>>,
}

struct Upstream {
    base_url: String,
    state: Arc<UpstreamState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn decision_upstream() -> Upstream {
    let state = Arc::new(UpstreamState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback fixture bind");
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server_state = state.clone();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(post(decision_handler))
                .with_state(server_state),
        )
        .await
        .unwrap();
    });
    Upstream {
        base_url,
        state,
        task,
    }
}

async fn decision_handler(
    State(state): State<Arc<UpstreamState>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let request: Value = serde_json::from_slice(&body).expect("router must send JSON");
    state
        .requests
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), request.clone()));
    if request["max_tokens"] == 128 {
        state.decision_calls.fetch_add(1, AtomicOrdering::Relaxed);
        if let Some(status) = *state.decision_status.lock().unwrap() {
            return status.into_response();
        }
        let reply = *state.decision_reply.lock().unwrap();
        let target = state.decision_target.lock().unwrap().clone();
        if reply == DecisionReply::Timeout {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        if reply == DecisionReply::ToolCall {
            return axum::Json(json!({"model":request["model"],"choices":[{"finish_reason":"stop","message":{"content":"{\"target\":\"target\"}","tool_calls":[{"id":"call-fixture","type":"function","function":{"name":"route","arguments":"{}"}}]}}]})).into_response();
        }
        let target = if target.is_empty() { "target" } else { &target };
        let (content, finish_reason) = match reply {
            DecisionReply::Valid | DecisionReply::Timeout | DecisionReply::ToolCall => {
                (format!("{{\"target\":\"{target}\"}}"), "stop")
            }
            DecisionReply::Duplicate => (
                format!("{{\"target\":\"{target}\",\"target\":\"{target}\"}}"),
                "stop",
            ),
            DecisionReply::ExtraField => (
                format!("{{\"target\":\"{target}\",\"reason\":\"ignored\"}}"),
                "stop",
            ),
            DecisionReply::Malformed => ("not json".into(), "stop"),
            DecisionReply::UnknownTarget => ("{\"target\":\"unknown\"}".into(), "stop"),
            DecisionReply::Truncated => (format!("{{\"target\":\"{target}\"}}"), "length"),
        };
        let mut payload = json!({
            "model": request["model"],
            "choices": [{"message": {"content": content}, "finish_reason": finish_reason}]
        });
        if let Some(usage) = state.decision_usage.lock().unwrap().clone() {
            payload["usage"] = usage;
        }
        return axum::Json(payload).into_response();
    }
    state.target_calls.fetch_add(1, AtomicOrdering::Relaxed);
    if state
        .failing_target
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|model| request["model"] == *model)
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    if request["stream"] == true {
        let event = if uri.path().ends_with("/responses") {
            format!(
                "event:response.completed\ndata:{}\n\n",
                json!({
                    "type":"response.completed",
                    "response":{
                        "object":"response",
                        "model":request["model"],
                        "status":"completed",
                        "output":[]
                    }
                })
            )
        } else {
            format!(
                "data:{}\n\ndata:[DONE]\n\n",
                json!({
                    "model":request["model"],
                    "choices":[{"delta":{"content":"target served"}}]
                })
            )
        };
        let mut response = Response::new(Body::from(event));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        return response;
    }
    if uri.path().ends_with("/responses") {
        return axum::Json(json!({
            "object": "response",
            "model": request["model"],
            "status": "completed",
            "output": []
        }))
        .into_response();
    }
    let mut payload = json!({
        "model": request["model"],
        "choices": [{"message": {"content": "target served"}}]
    });
    if let Some(usage) = state.target_usage.lock().unwrap().clone() {
        payload["usage"] = usage;
    }
    axum::Json(payload).into_response()
}

struct Fixture {
    _dir: tempfile::TempDir,
    config_path: PathBuf,
    app: Arc<AppState>,
    router: Router,
}

fn fixture(upstream: &Upstream) -> Fixture {
    fixture_with_policy(
        upstream,
        json!({
            "decision_model": "judge",
            "decision_targets": ["target"],
            "decision_timeout_seconds": 2,
            "decision_prompt": "Route based on request complexity."
        }),
    )
}

fn fixture_with_policy(upstream: &Upstream, routing_policy: Value) -> Fixture {
    fixture_with_policy_and_cloud(upstream, routing_policy, None)
}

fn fixture_with_target_response_support(
    upstream: &Upstream,
    routing_policy: Value,
    target_supports_responses: bool,
) -> Fixture {
    fixture_with_policy_and_cloud_and_target_response_support(
        upstream,
        routing_policy,
        None,
        target_supports_responses,
    )
}

fn fixture_with_cloud_policy(
    upstream: &Upstream,
    routing_policy: Value,
    cloud_allowed: bool,
) -> Fixture {
    fixture_with_policy_and_cloud(upstream, routing_policy, Some(cloud_allowed))
}

fn fixture_with_policy_and_cloud(
    upstream: &Upstream,
    routing_policy: Value,
    cloud_allowed: Option<bool>,
) -> Fixture {
    fixture_with_policy_and_cloud_and_target_response_support(
        upstream,
        routing_policy,
        cloud_allowed,
        true,
    )
}

fn fixture_with_policy_and_cloud_and_target_response_support(
    upstream: &Upstream,
    routing_policy: Value,
    cloud_allowed: Option<bool>,
    target_supports_responses: bool,
) -> Fixture {
    fixture_with_custom_config(
        upstream,
        routing_policy,
        cloud_allowed,
        target_supports_responses,
        |_| {},
        |_| {},
    )
}

fn fixture_with_custom_config(
    upstream: &Upstream,
    routing_policy: Value,
    cloud_allowed: Option<bool>,
    target_supports_responses: bool,
    configure: impl FnOnce(&mut Value),
    adjust_loaded_policy: impl FnOnce(&mut config::Policy),
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let key = dir.path().join("workload.key");
    fs::write(&key, WORKLOAD_KEY).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let config_path = dir.path().join("policy.json");
    let target_capabilities = if target_supports_responses {
        json!(["text", "responses"])
    } else {
        json!(["text"])
    };
    let mut config = json!({
        "bind": "100.127.255.254",
        "port": 14022,
        "credentials": [{
            "name": "fixture",
            "key_file": key,
            "aliases": ["smart", "judge", "target"],
            "allowed_routes": ["shared"],
            "class": "fixture",
            "requests_per_minute": 1,
            "burst": 1,
            "max_in_flight": 1
        }],
        "retry_policy": {"max_attempts": 1, "total_seconds": 10},
        "routes": [{
            "name": "shared",
            "kind": "local",
            "base_url": upstream.base_url,
            "model": "shared-fixture",
            "safe_concurrency": 1,
            "context_limit": 4096,
            "adapter_version": "fixture-v1",
            "capabilities": ["text", "responses"],
            "streaming": true,
            "timeout_seconds": 2,
            "enabled": true
        }],
        "aliases": {
            "judge": {
                "context_limit": 4096,
                "capabilities": ["text", "responses"],
                "streaming": true,
                "routes": [{"route": "shared", "priority": 0}]
            },
            "target": {
                "context_limit": 4096,
                "capabilities": target_capabilities,
                "streaming": true,
                "routes": [{"route": "shared", "priority": 0}]
            }
        },
        "routing_policies": {"smart": routing_policy}
    });
    if let Some(cloud_allowed) = cloud_allowed {
        config["credentials"][0]["aliases"] = json!(["smart", "judge", "target", "cloud"]);
        config["credentials"][0]["allowed_routes"] = json!(["shared", "codex"]);
        config["credentials"][0]["cloud_allowed"] = json!(cloud_allowed);
        config["codex"] = json!({
            "enabled": true,
            "alias": "cloud",
            "model": "gpt-fixture",
            "account_file": dir.path().join("missing-account.json")
        });
    }
    configure(&mut config);
    fs::write(&config_path, config.to_string()).unwrap();
    let mut policy = config::load(&config_path).unwrap();
    adjust_loaded_policy(&mut policy);
    let app = Arc::new(AppState::new(config_path.clone(), policy).unwrap());
    let runtime = app.current();
    for observation in runtime.observations.lock().unwrap().iter_mut() {
        observation.status = RouteStatus::Serving;
        observation.checked = Some(Instant::now());
        observation.qualified = Some(Instant::now());
    }
    let router = Router::new()
        .route("/v1/chat/completions", post(current_chat))
        .route("/v1/responses", post(current_responses))
        .route("/v1/models", get(current_models))
        .with_state(app.clone());
    Fixture {
        _dir: dir,
        config_path,
        app,
        router,
    }
}

fn fixture_with_locally_incompatible_candidate(
    upstream: &Upstream,
    routing_policy: Value,
    weak_capabilities: Value,
    target_alias_context_limit: u32,
    target_route_context_limit: u32,
    target_streaming: bool,
) -> Fixture {
    fixture_with_custom_config(
        upstream,
        routing_policy,
        None,
        true,
        move |config| {
            let full_context_limit = 32768;
            let full_capabilities = json!([
                "text",
                "image",
                "tools",
                "json_schema",
                "json_object",
                "responses",
                "responses_json_schema",
                "responses_json_object"
            ]);
            config["credentials"][0]["aliases"] = json!(["smart", "judge", "target", "valid"]);
            config["credentials"][0]["allowed_routes"] =
                json!(["judge-route", "target-weak", "valid-route"]);
            config["routes"] = json!([
                {
                    "name":"judge-route", "kind":"local", "base_url":upstream.base_url,
                    "model":"judge-fixture", "safe_concurrency":1, "context_limit":full_context_limit,
                    "adapter_version":"fixture-v1", "capabilities":full_capabilities,
                    "streaming":true, "timeout_seconds":2, "enabled":true
                },
                {
                    "name":"target-weak", "kind":"local", "base_url":upstream.base_url,
                    "model":"target-weak-fixture", "safe_concurrency":1,
                    "context_limit":target_alias_context_limit, "adapter_version":"fixture-v1",
                    "capabilities":weak_capabilities, "streaming":target_streaming,
                    "timeout_seconds":2, "enabled":true
                },
                {
                    "name":"target-full", "kind":"local", "base_url":upstream.base_url,
                    "model":"target-full-fixture", "safe_concurrency":1, "context_limit":full_context_limit,
                    "adapter_version":"fixture-v1", "capabilities":full_capabilities,
                    "streaming":true, "timeout_seconds":2, "enabled":true
                },
                {
                    "name":"valid-route", "kind":"local", "base_url":upstream.base_url,
                    "model":"valid-fixture", "safe_concurrency":1, "context_limit":full_context_limit,
                    "adapter_version":"fixture-v1", "capabilities":full_capabilities,
                    "streaming":true, "timeout_seconds":2, "enabled":true
                }
            ]);
            config["aliases"] = json!({
                "judge": {
                    "context_limit":full_context_limit, "capabilities":full_capabilities,
                    "streaming":true,
                    "routes":[{"route":"judge-route", "priority":0}]
                },
                "target": {
                    "context_limit":target_alias_context_limit, "capabilities":full_capabilities,
                    "streaming":target_streaming,
                    "routes":[
                        {"route":"target-weak", "priority":0},
                        {"route":"target-full", "priority":1}
                    ]
                },
                "valid": {
                    "context_limit":full_context_limit, "capabilities":full_capabilities,
                    "streaming":true,
                    "routes":[{"route":"valid-route", "priority":0}]
                }
            });
        },
        move |policy| {
            if target_route_context_limit < target_alias_context_limit {
                policy
                    .routes
                    .iter_mut()
                    .find(|route| route.config.name == "target-weak")
                    .expect("target weak route")
                    .config
                    .context_limit = target_route_context_limit;
            }
        },
    )
}

async fn invoke(fixture: &Fixture, endpoint: &str, body: Value) -> Response {
    fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/{endpoint}"))
                .header(header::AUTHORIZATION, format!("Bearer {WORKLOAD_KEY}"))
                .header("ChatGPT-Account-Id", "caller-controlled-account")
                .header("x-api-key", "caller-controlled-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn virtual_policy_decision_selects_local_target_once_without_double_admission() {
    let upstream = decision_upstream().await;
    let fixture = fixture(&upstream);

    let response = invoke(
        &fixture,
        "chat/completions",
        json!({
            "model": "smart",
            "messages": [{"role": "user", "content": "route this request"}],
            "max_tokens": 16
        }),
    )
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["model"],
        "target"
    );

    let requests = upstream.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one decision and one target request");
    assert_eq!(requests[0].0, "/v1/chat/completions");
    assert_eq!(requests[0].1["stream"], false);
    assert_eq!(requests[0].1["max_tokens"], 128);
    assert!(requests[0].1.get("tools").is_none());
    let decision_text = requests[0].1.to_string();
    assert!(decision_text.contains("Route based on request complexity."));
    assert!(decision_text.contains("\\\"model\\\":\\\"smart\\\""));
    assert!(decision_text.contains("target"));
    drop(requests);

    let runtime = fixture.app.current();
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 2);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
    assert_eq!(credential.class_semaphore.available_permits(), 1);
    assert!(
        credential.rate.lock().unwrap().tokens < 0.1,
        "one caller admission only"
    );
}

#[tokio::test]
async fn decision_candidates_exclude_incompatible_local_aliases_for_chat_and_responses() {
    for (endpoint, case, mut request) in [
        (
            "chat/completions",
            "tools",
            json!({"model":"smart","messages":[{"role":"user","content":"tools"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]}),
        ),
        (
            "chat/completions",
            "image",
            json!({"model":"smart","messages":[{"role":"user","content":[{"type":"text","text":"image"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]}]}),
        ),
        (
            "chat/completions",
            "structured",
            json!({"model":"smart","messages":[{"role":"user","content":"schema"}],"extra_body":{"response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"}}}}}),
        ),
        (
            "chat/completions",
            "stream",
            json!({"model":"smart","messages":[{"role":"user","content":"stream"}],"stream":true}),
        ),
        (
            "chat/completions",
            "alias-context",
            json!({"model":"smart","messages":[{"role":"user","content":"context"}],"max_tokens":2049}),
        ),
        (
            "chat/completions",
            "physical-context",
            json!({"model":"smart","messages":[{"role":"user","content":"context"}],"max_tokens":2049}),
        ),
        (
            "responses",
            "tools",
            json!({"model":"smart","input":"tools","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}),
        ),
        (
            "responses",
            "structured",
            json!({"model":"smart","input":"schema","text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object"}}}}),
        ),
        (
            "responses",
            "stream",
            json!({"model":"smart","input":"stream","stream":true}),
        ),
        (
            "responses",
            "alias-context",
            json!({"model":"smart","input":"context","max_output_tokens":2049}),
        ),
    ] {
        let upstream = decision_upstream().await;
        *upstream.state.decision_target.lock().unwrap() = "valid".into();
        let mut weak_capabilities = vec![
            "text",
            "image",
            "tools",
            "json_schema",
            "json_object",
            "responses",
            "responses_json_schema",
            "responses_json_object",
        ];
        match (endpoint, case) {
            (_, "image") => weak_capabilities.retain(|capability| *capability != "image"),
            (_, "tools") => weak_capabilities.retain(|capability| *capability != "tools"),
            ("chat/completions", "structured") => {
                weak_capabilities.retain(|capability| *capability != "json_schema")
            }
            ("responses", "structured") => {
                weak_capabilities.retain(|capability| *capability != "responses_json_schema")
            }
            _ => {}
        }
        if endpoint == "chat/completions" {
            request["max_tokens"] = json!(if case.ends_with("context") { 2000 } else { 16 });
        } else {
            request["max_output_tokens"] = json!(if case.ends_with("context") { 2000 } else { 16 });
        }
        let fixture = fixture_with_locally_incompatible_candidate(
            &upstream,
            json!({
                "decision_model":"judge", "decision_targets":["target","valid"],
                "decision_timeout_seconds":2, "decision_prompt":"filter incompatible locals"
            }),
            json!(weak_capabilities),
            if case == "alias-context" { 2048 } else { 32768 },
            if case.ends_with("context") {
                2048
            } else {
                32768
            },
            case != "stream",
        );
        if case == "physical-context" {
            let runtime = fixture.app.current();
            let mut target_request = request.clone();
            target_request["model"] = json!("target");
            assert!(
                validate_request(
                    &target_request,
                    &runtime.policy.config.aliases["target"],
                    None,
                    true,
                )
                .is_ok(),
                "target alias must admit the physical-context request"
            );
            assert_eq!(
                runtime
                    .policy
                    .routes
                    .iter()
                    .find(|route| route.config.name == "target-weak")
                    .unwrap()
                    .config
                    .context_limit,
                2048
            );
            let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
            assert!(
                credential
                    .allowed_routes
                    .iter()
                    .any(|route| route == "target-weak")
            );
            assert!(
                !credential
                    .allowed_routes
                    .iter()
                    .any(|route| route == "target-full")
            );
        }

        let response = invoke(&fixture, endpoint, request).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{endpoint}/{case}: {}",
            String::from_utf8_lossy(&body)
        );
        if case == "stream" {
            let stream = String::from_utf8_lossy(&body);
            if endpoint == "chat/completions" {
                assert!(
                    stream.contains("target served"),
                    "{endpoint}/{case}: {stream}"
                );
            }
            assert!(
                stream.contains(if endpoint == "chat/completions" {
                    "[DONE]"
                } else {
                    "response.completed"
                }),
                "{endpoint}/{case}: {stream}"
            );
        }
        assert_eq!(
            upstream.state.decision_calls.load(AtomicOrdering::Relaxed),
            1,
            "{endpoint}/{case} must invoke the judge once"
        );
        assert_eq!(
            upstream.state.target_calls.load(AtomicOrdering::Relaxed),
            1,
            "{endpoint}/{case} must dispatch only the valid target"
        );
        let requests = upstream.state.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{endpoint}/{case}");
        assert!(
            requests[0].1["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("one of: [\"valid\"]"),
            "{endpoint}/{case}: {}",
            requests[0].1
        );
        assert_eq!(requests[1].1["model"], "valid-fixture", "{endpoint}/{case}");
    }
}

#[tokio::test]
async fn all_incompatible_local_decision_candidates_fail_before_judge_or_target() {
    let upstream = decision_upstream().await;
    let fixture = fixture_with_locally_incompatible_candidate(
        &upstream,
        json!({
            "decision_model":"judge", "decision_targets":["target"],
            "decision_timeout_seconds":2, "decision_prompt":"do not call the judge"
        }),
        json!([
            "text",
            "json_schema",
            "json_object",
            "responses",
            "responses_json_schema",
            "responses_json_object"
        ]),
        4096,
        4096,
        true,
    );

    let response = invoke(
        &fixture,
        "chat/completions",
        json!({"model":"smart","messages":[{"role":"user","content":"tools"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],"max_tokens":16}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        upstream.state.decision_calls.load(AtomicOrdering::Relaxed),
        0
    );
    assert_eq!(upstream.state.target_calls.load(AtomicOrdering::Relaxed), 0);
}

#[tokio::test]
async fn forged_decision_for_a_filtered_local_target_fails_before_target_inference() {
    let upstream = decision_upstream().await;
    *upstream.state.decision_target.lock().unwrap() = "target".into();
    let fixture = fixture_with_locally_incompatible_candidate(
        &upstream,
        json!({
            "decision_model":"judge", "decision_targets":["target","valid"],
            "decision_timeout_seconds":2, "decision_prompt":"reject forged target"
        }),
        json!([
            "text",
            "json_schema",
            "json_object",
            "responses",
            "responses_json_schema",
            "responses_json_object"
        ]),
        4096,
        4096,
        true,
    );

    let response = invoke(
        &fixture,
        "chat/completions",
        json!({"model":"smart","messages":[{"role":"user","content":"tools"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],"max_tokens":16}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        upstream.state.decision_calls.load(AtomicOrdering::Relaxed),
        1
    );
    assert_eq!(upstream.state.target_calls.load(AtomicOrdering::Relaxed), 0);
    let requests = upstream.state.requests.lock().unwrap();
    assert!(
        requests[0].1["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("one of: [\"valid\"]")
    );
}

#[tokio::test]
async fn catalog_exposes_a_decision_only_virtual_alias_when_every_required_grant_exists() {
    let upstream = decision_upstream().await;
    let fixture = fixture(&upstream);
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header(header::AUTHORIZATION, format!("Bearer {WORKLOAD_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let catalog: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "smart"),
        "{catalog}"
    );
}

#[tokio::test]
async fn catalog_rule_aliases_require_a_matching_class_and_compatible_target() {
    for (name, routing_policy, target_supports_responses, advertised) in [
        (
            "foreign-class",
            json!({"rules":[{"id":"foreign","target":"target","when":{"credential_classes":["other"]}}]}),
            true,
            false,
        ),
        (
            "unconstrained",
            json!({"rules":[{"id":"unconstrained","target":"target","when":{"stream":false}}]}),
            true,
            true,
        ),
        (
            "matching-class",
            json!({"rules":[{"id":"matching","target":"target","when":{"credential_classes":["fixture"]}}]}),
            true,
            true,
        ),
        (
            "responses-chat-only",
            json!({"rules":[{"id":"responses","target":"target","when":{"endpoints":["responses"]}}]}),
            false,
            false,
        ),
    ] {
        let upstream = decision_upstream().await;
        let fixture = fixture_with_target_response_support(
            &upstream,
            routing_policy.clone(),
            target_supports_responses,
        );
        let response = fixture
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, format!("Bearer {WORKLOAD_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let catalog: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "smart"),
            advertised,
            "{name}: {catalog}"
        );

        if name == "foreign-class" {
            let invocation_fixture = fixture_with_target_response_support(
                &upstream,
                routing_policy,
                target_supports_responses,
            );
            let response = invoke(
                &invocation_fixture,
                "chat/completions",
                json!({"model":"smart","messages":[{"role":"user","content":"not eligible"}]}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"],
                "no_routing_target"
            );
            assert!(
                upstream.state.requests.lock().unwrap().is_empty(),
                "unmatched rules must not invoke an upstream"
            );
        }
    }
}

#[tokio::test]
async fn first_matching_rule_skips_decision_and_routes_both_apis() {
    let upstream = decision_upstream().await;
    for (endpoint, body) in [
        (
            "chat/completions",
            json!({"model":"smart","messages":[{"role":"user","content":"chat"}],"max_tokens":16}),
        ),
        (
            "responses",
            json!({"model":"smart","input":"responses","max_output_tokens":16}),
        ),
    ] {
        let fixture = fixture_with_policy(
            &upstream,
            json!({
                "rules": [
                    {"id": "first", "target": "target", "when": {"credential_classes": ["fixture"], "endpoints": ["chat", "responses"]}},
                    {"id": "second", "target": "judge", "when": {"credential_classes": ["fixture"]}}
                ],
                "decision_model": "judge",
                "decision_targets": ["judge"],
                "decision_timeout_seconds": 2,
                "decision_prompt": "rules must win",
                "default_target": "judge"
            }),
        );
        let response = invoke(&fixture, endpoint, body).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{endpoint}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["model"],
            "target"
        );
    }
    let requests = upstream.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|(_, request)| request["max_tokens"] != 128)
    );
}

#[tokio::test]
async fn decision_selects_a_local_target_for_responses() {
    let upstream = decision_upstream().await;
    let fixture = fixture(&upstream);
    let response = invoke(
        &fixture,
        "responses",
        json!({"model":"smart","input":"choose a target","max_output_tokens":16}),
    )
    .await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["model"],
        "target"
    );
    let requests = upstream.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, "/v1/chat/completions");
    assert_eq!(requests[1].0, "/v1/responses");
}

#[tokio::test]
async fn malformed_or_truncated_decision_fails_closed_without_target_replay() {
    for reply in [
        DecisionReply::Malformed,
        DecisionReply::UnknownTarget,
        DecisionReply::Truncated,
        DecisionReply::Duplicate,
        DecisionReply::ExtraField,
        DecisionReply::ToolCall,
    ] {
        let upstream = decision_upstream().await;
        *upstream.state.decision_reply.lock().unwrap() = reply;
        let fixture = fixture_with_policy(
            &upstream,
            json!({
                "decision_model": "judge",
                "decision_targets": ["target"],
                "decision_timeout_seconds": 2,
                "decision_prompt": "closed failure",
                "default_target": "target"
            }),
        );
        let response = invoke(
            &fixture,
            "chat/completions",
            json!({"model":"smart","messages":[{"role":"user","content":"must not retry target"}],"max_tokens":16}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let requests = upstream.state.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "decision failure must not invoke a fallback target"
        );
        assert_eq!(requests[0].0, "/v1/chat/completions");
        assert!(requests[0].1["max_tokens"] == 128);
    }
}

#[tokio::test]
async fn cloud_rule_and_decision_require_configured_cloud_grants_not_caller_headers() {
    for routing_policy in [
        json!({
            "rules": [{"id":"cloud", "target":"cloud", "when":{"credential_classes":["fixture"]}}]
        }),
        json!({
            "decision_model":"judge",
            "decision_targets":["cloud"],
            "decision_timeout_seconds":2,
            "decision_prompt":"cloud preflight"
        }),
    ] {
        let upstream = decision_upstream().await;
        let fixture = fixture_with_cloud_policy(&upstream, routing_policy, false);
        let response = invoke(
            &fixture,
            "chat/completions",
            json!({"model":"smart","messages":[{"role":"user","content":"cloud denied"}],"max_tokens":16}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            upstream.state.requests.lock().unwrap().is_empty(),
            "denied cloud must not infer locally or dispatch a target"
        );
    }
}

#[tokio::test]
async fn cloud_rule_and_decision_reach_native_auth_preflight_without_live_provider_access() {
    for (routing_policy, decisions) in [
        (
            json!({
                "rules": [{"id":"cloud", "target":"cloud", "when":{"credential_classes":["fixture"]}}]
            }),
            0,
        ),
        (
            json!({
                "decision_model":"judge",
                "decision_targets":["cloud"],
                "decision_timeout_seconds":2,
                "decision_prompt":"cloud preflight"
            }),
            1,
        ),
    ] {
        let upstream = decision_upstream().await;
        *upstream.state.decision_target.lock().unwrap() = "cloud".into();
        let fixture = fixture_with_cloud_policy(&upstream, routing_policy, true);
        let response = invoke(
            &fixture,
            "chat/completions",
            json!({"model":"smart","messages":[{"role":"user","content":"cloud preflight only"}]}),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            matches!(
                status,
                StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
            ),
            "{error}"
        );
        assert!(
            matches!(
                error["error"]["code"].as_str(),
                Some("upstream_auth_error" | "codex_auth_unavailable")
            ),
            "{error}"
        );
        let requests = upstream.state.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            decisions,
            "only decision inference may use the loopback route"
        );
        assert!(
            requests
                .iter()
                .all(|(_, request)| request["max_tokens"] == 128)
        );
    }
}

#[tokio::test]
async fn decision_timeout_and_input_bound_never_dispatch_a_target() {
    for oversized in [false, true] {
        let upstream = decision_upstream().await;
        *upstream.state.decision_reply.lock().unwrap() = DecisionReply::Timeout;
        let fixture = fixture_with_policy(
            &upstream,
            json!({
                "decision_model":"judge", "decision_targets":["target"],
                "decision_timeout_seconds":1, "default_target":"target"
            }),
        );
        let response = invoke(&fixture, "chat/completions", json!({
            "model":"smart", "messages":[{"role":"user","content":if oversized {"x".repeat(32769)} else {"hello".into()}}], "max_tokens":16
        })).await;
        assert_eq!(
            response.status(),
            if oversized {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::GATEWAY_TIMEOUT
            }
        );
        assert_eq!(
            upstream.state.requests.lock().unwrap().len(),
            usize::from(!oversized)
        );
        let runtime = fixture.app.current();
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            1
        );
    }
}

#[tokio::test]
async fn decision_and_concrete_target_grants_are_independent_of_virtual_alias() {
    for denied in ["judge", "target"] {
        let upstream = decision_upstream().await;
        let fixture = fixture_with_policy(
            &upstream,
            json!({
                "decision_model":"judge", "decision_targets":["target","judge"], "decision_timeout_seconds":2
            }),
        );
        let runtime = fixture.app.current();
        {
            let mut auth = runtime.auth.write().unwrap();
            let auth = Arc::get_mut(&mut auth).unwrap();
            let credential = Arc::get_mut(auth.credentials.get_mut("fixture").unwrap()).unwrap();
            credential.config.aliases.retain(|alias| alias != denied);
        }
        let response = invoke(
            &fixture,
            "chat/completions",
            json!({"model":"smart","messages":[{"role":"user","content":"hello"}],"max_tokens":16}),
        )
        .await;
        assert_eq!(
            response.status(),
            if denied == "judge" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_GATEWAY
            }
        );
        assert_eq!(
            upstream.state.requests.lock().unwrap().len(),
            usize::from(denied == "target")
        );
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
    }
}

#[test]
fn decision_authority_changes_policy_digest_and_requires_restart() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let upstream = decision_upstream().await;
        let fixture = fixture(&upstream);
        let original = config::load(&fixture.config_path).unwrap();
        let mut candidate = config::load(&fixture.config_path).unwrap();
        candidate
            .config
            .routing_policies
            .get_mut("smart")
            .unwrap()
            .decision_prompt = "different authority guidance".into();
        assert_ne!(
            audit_identity::digest(&original.config),
            audit_identity::digest(&candidate.config),
            "decision controls must be included in policy identity"
        );
        assert!(validate_reload(&original, &candidate).is_err());
    });
}

#[tokio::test]
async fn routing_shadow_serves_baseline_once_without_judge_or_extra_rate_debit() {
    let upstream = decision_upstream().await;
    let fixture = fixture_with_policy(
        &upstream,
        json!({"mode":"shadow","version":"synthetic-v1",
        "default_target":"target","decision_model":"judge","decision_targets":["target"],
        "rules":[{"id":"λ advisory","target":"target","when":{"stream":false}}]}),
    );
    // Leave prompt headroom within the fixture's 4096-token context.
    let request =
        json!({"model":"smart","messages":[{"role":"user","content":"hello"}],"max_tokens":16});
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", format!("Bearer {WORKLOAD_KEY}"))
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-slotline-routing-mode"], "shadow");
    assert_eq!(
        response.headers()["x-slotline-routing-version"],
        "synthetic-v1"
    );
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        upstream.state.decision_calls.load(AtomicOrdering::Relaxed),
        0
    );
    assert_eq!(upstream.state.target_calls.load(AtomicOrdering::Relaxed), 1);
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", format!("Bearer {WORKLOAD_KEY}"))
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(upstream.state.target_calls.load(AtomicOrdering::Relaxed), 1);
}

#[tokio::test]
async fn routing_shadow_denied_advisory_is_absent_and_denied_baseline_never_judges() {
    for denied_baseline in [false, true] {
        let upstream = decision_upstream().await;
        let fixture = fixture_with_cloud_policy(
            &upstream,
            json!({"mode":"shadow",
            "default_target":if denied_baseline {"cloud"}else{"target"},"decision_model":"judge","decision_targets":["target"],
            "rules":[{"id":"cloud","target":"cloud","when":{"stream":false}}]}),
            false,
        );
        let response = fixture
            .router
            .clone()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("authorization", format!("Bearer {WORKLOAD_KEY}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        // Leave prompt headroom within the fixture's 4096-token context.
                        json!({"model":"smart","messages":[{"role":"user","content":"hello"}],"max_tokens":16})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if denied_baseline {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            }
        );
        assert!(
            response
                .headers()
                .get("x-slotline-routing-advisory-sha256")
                .is_none()
        );
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("cloud"));
        assert_eq!(
            upstream.state.decision_calls.load(AtomicOrdering::Relaxed),
            0
        );
        assert_eq!(
            upstream.state.target_calls.load(AtomicOrdering::Relaxed),
            usize::from(!denied_baseline)
        );
    }
}
