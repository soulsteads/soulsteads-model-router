use super::*;
use axum::{extract::Request, http::Method};
use tower::ServiceExt;

struct Mock {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn start(reply: Value, delay: Duration, status: StatusCode) -> Self {
        Self::start_with_generation_status(reply, delay, status, None).await
    }

    async fn start_with_generation_status(
        reply: Value,
        delay: Duration,
        status: StatusCode,
        generation_status: Option<StatusCode>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let router = Router::new().fallback(post(move |request: Request| {
            let captured = captured.clone();
            let reply = reply.clone();
            async move {
                let path = request.uri().path().to_string();
                let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                captured.lock().unwrap().push((path.clone(),value.clone()));
                assert_eq!(value["model"], "synthetic", "mock enforces actual model identity");
                if is_classifier(&value) {
                    assert_eq!(path,"/v1/chat/completions");
                    assert_eq!(value["stream"],false);
                    assert_eq!(value["max_tokens"],64);
                    assert!(value.get("tools").is_none());
                    tokio::time::sleep(delay).await;
                    let mut response = axum::Json(reply).into_response();
                    *response.status_mut() = status;
                    return response;
                }
                if let Some(generation_status) = generation_status {
                    return (generation_status, axum::Json(json!({"error":{"message":"synthetic generation failure","type":"fixture"}}))).into_response();
                }
                let responses = path == "/v1/responses";
                assert!(responses || path == "/v1/chat/completions");
                if value["stream"] == true {
                    let bytes = if responses {
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"object\":\"response\",\"model\":\"synthetic\",\"status\":\"completed\",\"output\":[]}}\n\n".to_string()
                    } else {
                        "data: {\"model\":\"synthetic\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"safe\"},\"finish_reason\":null}]}\n\ndata: {\"model\":\"synthetic\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()
                    };
                    let mut response = Response::new(Body::from(bytes));
                    response.headers_mut().insert(header::CONTENT_TYPE,HeaderValue::from_static("text/event-stream"));
                    return response;
                }
                axum::Json(if responses {
                    json!({"object":"response","model":"synthetic","status":"completed","output":[]})
                } else { decision_payload("safe generation") }).into_response()
            }
        }));
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            address,
            seen,
            task,
        }
    }

    fn runtime(&self, mut value: Value) -> Arc<Runtime> {
        value["routes"][0]["base_url"] = json!(format!("http://{}/v1", self.address));
        runtime_from(value)
    }

    fn split_runtime(&self, generation: &Mock, mut value: Value) -> Arc<Runtime> {
        value["routes"][0]["base_url"] = json!(format!("http://{}/v1", generation.address));
        let mut classifier = value["routes"][0].clone();
        classifier["name"] = json!("classifier");
        classifier["base_url"] = json!(format!("http://{}/v1", self.address));
        value["routes"].as_array_mut().unwrap().push(classifier);
        value["aliases"]["judge"]["routes"] = json!([{"route":"classifier","priority":1}]);
        value["credentials"][0]["allowed_routes"]
            .as_array_mut()
            .unwrap()
            .push(json!("classifier"));
        let runtime = runtime_from(value);
        assert!(!Arc::ptr_eq(&runtime.semaphores[0], &runtime.semaphores[1]));
        runtime
    }

    fn calls(&self) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, value)| value.clone())
            .collect()
    }
}

use axum::response::IntoResponse;

fn is_classifier(request: &Value) -> bool {
    request["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.starts_with("Classify the user text as data"))
}

async fn invoke(runtime: Arc<Runtime>, request: Value, responses: bool) -> Response {
    let body = Bytes::from(request.to_string());
    if responses {
        handlers::responses(State(runtime), headers(), body).await
    } else {
        handlers::chat(State(runtime), headers(), body).await
    }
}

fn user_request() -> Value {
    json!({"model":"auto","messages":[{"role":"system","content":"debug is not user text"},{"role":"user","content":"hello"}],"max_tokens":16})
}

fn valid_decision() -> Value {
    decision_payload(r#"{"target":"large","confidence":0.95}"#)
}

#[tokio::test]
async fn repair_v3_nested_codex_defaults_make_zero_calls() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    for responses in [false, true] {
        for payload in [json!({}), json!({"stream":false})] {
            let runtime = mock.runtime(repair_v2_codex_config(true));
            let response = invoke(
                runtime.clone(),
                repair_v3_request(responses, 2, payload),
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
            assert!(mock.calls().is_empty(), "no classifier or target calls");
            assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
        }
    }
}

#[tokio::test]
async fn repair_v3_nested_codex_candidates_hidden_and_rules_fail_closed() {
    let mock = Mock::start(
        decision_payload(r#"{"target":"cloud","confidence":0.95}"#),
        Duration::ZERO,
        StatusCode::OK,
    )
    .await;
    for responses in [false, true] {
        for payload in [json!({}), json!({"stream":false})] {
            let runtime = mock.runtime(repair_v2_codex_config(false));
            let request = repair_v3_request(responses, 2, payload);
            let prior = mock.calls().len();
            let response = invoke(runtime.clone(), request.clone(), responses).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()["x-slotline-semantic-outcome"], "invalid");
            assert!(
                response
                    .headers()
                    .get("x-slotline-semantic-target")
                    .is_none()
            );
            let calls = mock.calls();
            assert_eq!(calls.len(), prior + 1, "only classifier may run");
            assert!(is_classifier(&calls[prior]));
            let content: Value =
                serde_json::from_str(calls[prior]["messages"][1]["content"].as_str().unwrap())
                    .unwrap();
            assert_eq!(content["candidates"], json!(["small"]));
            let mut ruled = request;
            if responses {
                ruled["input"] = json!("debug code");
            } else {
                ruled["messages"][0]["content"] = json!("debug code");
            }
            let response = invoke(runtime, ruled, responses).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.headers()["x-slotline-semantic-outcome"],
                "incompatible"
            );
            assert_eq!(
                mock.calls().len(),
                prior + 1,
                "rule denial must not call classifier or target"
            );
        }
    }
}

#[tokio::test]
async fn repair_v3_managed_forwarding_preserves_once_projection_and_classifier_body() {
    let mock = Mock::start(
        decision_payload(r#"{"target":"small","confidence":0.95}"#),
        Duration::ZERO,
        StatusCode::OK,
    )
    .await;
    for responses in [false, true] {
        for depth in 0..=2 {
            for payload in [
                json!({}),
                json!({"stream":true}),
                json!({"enable_thinking":false}),
            ] {
                if depth < 2 && payload.get("stream").is_some() {
                    continue;
                }
                let mut value = configured();
                value["semantic_routing"]["profiles"]["auto"]["compatibility_preflight"] =
                    json!(true);
                value["aliases"]["small"]["streaming"] = json!(false);
                let mut request = repair_v3_request(responses, depth, payload.clone());
                if depth == 2 && payload.get("enable_thinking").is_some() {
                    request["enable_thinking"] = json!(true);
                }
                let original = request.clone();
                let mut expected = request.clone();
                normalize_extra_body(&mut expected, true).unwrap();
                expected["model"] = json!("synthetic");
                let prior = mock.calls().len();
                let response = invoke(mock.runtime(value), request.clone(), responses).await;
                assert_eq!(response.status(), StatusCode::OK, "{request}");
                assert_eq!(response.headers()["x-slotline-semantic-outcome"], "model");
                let calls = mock.calls();
                assert_eq!(calls.len(), prior + 2);
                assert!(is_classifier(&calls[prior]));
                assert!(calls[prior].get("extra_body").is_none());
                assert!(calls[prior].get("enable_thinking").is_none());
                let content: Value =
                    serde_json::from_str(calls[prior]["messages"][1]["content"].as_str().unwrap())
                        .unwrap();
                assert_eq!(content["candidates"], json!(["small", "large"]));
                assert_eq!(content["user_text"], "hello");
                assert_eq!(calls[prior + 1], expected);
                assert_eq!(request, original);
            }
        }
    }
}

#[tokio::test]
async fn repair_v2_codex_invalid_default_makes_zero_classifier_or_target_calls() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    for responses in [false, true] {
        for extra_body in [false, true] {
            let runtime = mock.runtime(repair_v2_codex_config(true));
            let response = invoke(
                runtime.clone(),
                repair_v2_codex_request(responses, extra_body),
                responses,
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.headers()["x-slotline-semantic-outcome"],
                "incompatible"
            );
            assert!(
                mock.calls().is_empty(),
                "no classifier or target may be called"
            );
            assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
        }
    }
}

#[tokio::test]
async fn repair_v2_codex_invalid_candidate_is_hidden_and_cannot_dispatch() {
    let mock = Mock::start(
        decision_payload(r#"{"target":"cloud","confidence":0.95}"#),
        Duration::ZERO,
        StatusCode::OK,
    )
    .await;
    for responses in [false, true] {
        for extra_body in [false, true] {
            let prior = mock.calls().len();
            let response = invoke(
                mock.runtime(repair_v2_codex_config(false)),
                repair_v2_codex_request(responses, extra_body),
                responses,
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()["x-slotline-semantic-outcome"], "invalid");
            let calls = mock.calls();
            assert_eq!(calls.len(), prior + 1, "only classifier may run");
            assert!(is_classifier(&calls[prior]));
            let content: Value =
                serde_json::from_str(calls[prior]["messages"][1]["content"].as_str().unwrap())
                    .unwrap();
            assert_eq!(content["candidates"], json!(["small"]));
        }
    }
}

#[tokio::test]
async fn increment_shadow_serves_only_baseline_preserving_body_and_sse() {
    for responses in [false, true] {
        for stream in [false, true] {
            let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
            let mut value = configured();
            value["semantic_routing"]["profiles"]["auto"]["mode"] = json!("shadow");
            value["semantic_routing"]["profiles"]["auto"]["policy_version"] = json!("synthetic-v1");
            for (text, advisory) in [("debug code", "rule"), ("hello", "classifier_not_run")] {
                let request = if responses {
                    json!({"model":"auto","input":text,"max_output_tokens":16,"stream":stream})
                } else {
                    json!({"model":"auto","messages":[{"role":"user","content":text}],"max_tokens":16,"stream":stream})
                };
                let before = mock.calls().len();
                let response =
                    invoke(mock.runtime(value.clone()), request.clone(), responses).await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
                assert_eq!(
                    response.headers()["x-slotline-semantic-advisory-outcome"],
                    advisory
                );
                assert_eq!(
                    response.headers()["x-slotline-semantic-classifier"],
                    "not_run"
                );
                if advisory == "rule" {
                    assert_eq!(
                        response.headers()["x-slotline-semantic-advisory-target"],
                        "large"
                    );
                } else {
                    assert!(
                        !response
                            .headers()
                            .contains_key("x-slotline-semantic-advisory-target")
                    );
                }
                let bytes = axum::body::to_bytes(response.into_body(), 16384)
                    .await
                    .unwrap();
                let mut explicit = request;
                explicit["model"] = json!("small");
                let baseline = invoke(mock.runtime(value.clone()), explicit, responses).await;
                assert_eq!(
                    bytes,
                    axum::body::to_bytes(baseline.into_body(), 16384)
                        .await
                        .unwrap()
                );
                assert_eq!(mock.calls().len(), before + 2);
                assert!(
                    mock.calls()[before..]
                        .iter()
                        .all(|call| !is_classifier(call))
                );
            }
        }
    }
}

#[tokio::test]
async fn increment_shadow_denied_and_incompatible_advisories_do_not_leak_or_generate() {
    for incompatible in [false, true] {
        let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"]["mode"] = json!("shadow");
        value["semantic_routing"]["profiles"]["auto"]["compatibility_preflight"] = json!(true);
        if incompatible {
            value["aliases"]["large"]["streaming"] = json!(false);
        } else {
            value["credentials"][0]["aliases"] = json!(["auto", "small"]);
        }
        let mut request = user_request();
        request["messages"][1]["content"] = json!("debug code");
        request["stream"] = json!(true);
        let response = invoke(mock.runtime(value), request, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
        assert_eq!(
            response.headers()["x-slotline-semantic-advisory-outcome"],
            if incompatible {
                "incompatible"
            } else {
                "denied"
            }
        );
        assert!(
            !response
                .headers()
                .contains_key("x-slotline-semantic-advisory-target")
        );
        assert!(!response.headers().contains_key("x-slotline-semantic-rule"));
        let _ = axum::body::to_bytes(response.into_body(), 16384)
            .await
            .unwrap();
        assert_eq!(mock.calls().len(), 1);
        assert!(!is_classifier(&mock.calls()[0]));
    }
}

#[tokio::test]
async fn increment_active_preflight_filters_candidates_and_rules_fail_closed() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    let mut value = configured();
    value["semantic_routing"]["profiles"]["auto"]["compatibility_preflight"] = json!(true);
    value["aliases"]["large"]["streaming"] = json!(false);
    let mut request = user_request();
    request["extra_body"] = json!({"stream":true});
    let response = invoke(mock.runtime(value.clone()), request.clone(), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-slotline-semantic-outcome"], "invalid");
    let _ = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    let content: Value =
        serde_json::from_str(mock.calls()[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["candidates"], json!(["small"]));
    assert_eq!(mock.calls().len(), 2);
    request["messages"][1]["content"] = json!("debug code");
    let response = invoke(mock.runtime(value), request, false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn increment_shadow_preserves_baseline_error_and_explicit_alias_mode() {
    for responses in [false, true] {
        let mock = Mock::start_with_generation_status(
            valid_decision(),
            Duration::ZERO,
            StatusCode::OK,
            Some(StatusCode::UNPROCESSABLE_ENTITY),
        )
        .await;
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"]["mode"] = json!("shadow");
        let mut request = if responses {
            json!({"model":"auto","input":"debug code","max_output_tokens":16})
        } else {
            json!({"model":"auto","messages":[{"role":"user","content":"debug code"}],"max_tokens":16})
        };
        let shadow = invoke(mock.runtime(value.clone()), request.clone(), responses).await;
        let status = shadow.status();
        assert!(!status.is_success());
        let body = axum::body::to_bytes(shadow.into_body(), 16384)
            .await
            .unwrap();
        request["model"] = json!("small");
        let explicit = invoke(mock.runtime(value), request, responses).await;
        assert_eq!(status, explicit.status());
        assert!(!explicit.headers().contains_key("x-slotline-semantic-mode"));
        assert_eq!(
            body,
            axum::body::to_bytes(explicit.into_body(), 16384)
                .await
                .unwrap()
        );
        assert_eq!(mock.calls().len(), 2);
        assert!(mock.calls().iter().all(|call| !is_classifier(call)));
    }
}

async fn body_json(response: Response) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn model_selection_preserves_chat_tools_schema_and_has_no_class_deadlock() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    let runtime = mock.runtime(configured());
    let mut request = user_request();
    request["tools"] = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}]);
    request["response_format"] = json!({"type":"json_schema","json_schema":{"name":"result","schema":{"type":"object","properties":{}}}});
    request["user"] = json!("synthetic-preserved");
    request["top_k"] = json!(5);
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        invoke(runtime.clone(), request.clone(), false),
    )
    .await
    .unwrap();
    let status = response.status();
    if status != StatusCode::OK {
        panic!(
            "generation returned {status}: {}",
            body_json(response).await
        );
    }
    assert_eq!(response.headers()["x-slotline-semantic-target"], "large");
    assert_eq!(response.headers()["x-slotline-semantic-outcome"], "model");
    assert_eq!(body_json(response).await["model"], "large");
    let calls = mock.calls();
    assert_eq!(calls.len(), 2);
    assert!(is_classifier(&calls[0]));
    request["model"] = json!("synthetic");
    assert_eq!(calls[1], request);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(runtime.semantic_admission.available_permits(), 1);
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn rules_precede_model_and_default_is_explicit() {
    for mode in ["rule", "default"] {
        let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
        let mut value = configured();
        let mut request = user_request();
        if mode == "rule" {
            request["messages"][1]["content"] = json!("DEBUG my code");
        } else {
            value["semantic_routing"]["profiles"]["auto"]
                .as_object_mut()
                .unwrap()
                .remove("decision");
        }
        let response = invoke(mock.runtime(value), request, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-slotline-semantic-outcome"], mode);
        if mode == "rule" {
            assert_eq!(response.headers()["x-slotline-semantic-rule"], "code");
        }
        assert_eq!(mock.calls().len(), 1);
        assert!(!is_classifier(&mock.calls()[0]));
    }
}

#[tokio::test]
async fn failures_are_sanitized_and_only_use_authorized_configured_fallback() {
    for (reply, reason) in [
        (decision_payload("PRIVATE_CLASSIFIER_SENTINEL"), "invalid"),
        (
            decision_payload(r#"{"target":"arbitrary","confidence":1}"#),
            "invalid",
        ),
        (
            decision_payload(r#"{"target":"large","confidence":0.1}"#),
            "low_confidence",
        ),
        (json!({"model":"wrong","choices":[]}), "unavailable"),
        (decision_payload(&"X".repeat(5000)), "unavailable"),
    ] {
        for policy in ["default", "error"] {
            let mock = Mock::start(reply.clone(), Duration::ZERO, StatusCode::OK).await;
            let generation = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
            let mut value = configured();
            value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!(policy);
            let runtime = mock.split_runtime(&generation, value);
            let response = invoke(runtime.clone(), user_request(), false).await;
            assert_eq!(response.headers()["x-slotline-semantic-outcome"], reason);
            assert_eq!(
                response.status(),
                if policy == "default" {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            );
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_CLASSIFIER_SENTINEL"));
            assert_eq!(mock.calls().len(), 1);
            assert_eq!(generation.calls().len(), usize::from(policy == "default"));
            assert_eq!(
                runtime.observations.lock().unwrap()[0].status,
                RouteStatus::Serving
            );
            if reason == "unavailable" {
                assert_eq!(
                    runtime.observations.lock().unwrap()[1].status,
                    RouteStatus::CoolingDown
                );
            }
            assert_eq!(runtime.semaphores[0].available_permits(), 1);
            assert_eq!(runtime.semaphores[1].available_permits(), 1);
        }
    }
}

#[tokio::test]
async fn shared_classifier_failure_keeps_default_generation_unavailable() {
    for (reply, status) in [
        (json!({"model":"wrong","choices":[]}), StatusCode::OK),
        (decision_payload(&"X".repeat(5000)), StatusCode::OK),
        (
            json!({"error":{"message":"PRIVATE_CLASSIFIER_SENTINEL"}}),
            StatusCode::BAD_GATEWAY,
        ),
    ] {
        for policy in ["default", "error"] {
            let mock = Mock::start(reply.clone(), Duration::ZERO, status).await;
            let mut value = configured();
            value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!(policy);
            let runtime = mock.runtime(value);
            let response = invoke(runtime.clone(), user_request(), false).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                response.headers()["x-slotline-semantic-outcome"],
                "unavailable"
            );
            if policy == "default" {
                assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
            } else {
                assert!(
                    response
                        .headers()
                        .get("x-slotline-semantic-target")
                        .is_none()
                );
            }
            let body = body_json(response).await;
            assert_eq!(
                body["error"]["code"],
                if policy == "default" {
                    "no_serving_capacity"
                } else {
                    "semantic_decision_failed"
                }
            );
            assert!(!body.to_string().contains("PRIVATE_CLASSIFIER_SENTINEL"));
            assert_eq!(mock.calls().len(), 1);
            assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 1);
            assert_eq!(runtime.semaphores[0].available_permits(), 1);
            let observations = runtime.observations.lock().unwrap();
            assert_eq!(observations[0].status, RouteStatus::CoolingDown);
            assert!(observations[0].qualified.is_none());
        }
    }
}

#[tokio::test]
async fn metadata_rejection_preserves_explicit_and_semantic_managed_validation() {
    for model in ["small", "auto"] {
        let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
        let mut request = user_request();
        request["model"] = json!(model);
        request["metadata"] = json!({"synthetic":"unsupported"});
        let response = invoke(mock.runtime(configured()), request, false).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "unsupported_parameter");
        assert_eq!(
            body["error"]["message"],
            "Unsupported model argument: metadata"
        );
        assert_eq!(mock.calls().len(), usize::from(model == "auto"));
        assert!(mock.calls().iter().all(is_classifier));
    }
}

#[tokio::test]
async fn explicit_nonsemantic_requests_keep_existing_policy_field_behavior() {
    for responses in [false, true] {
        let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
        let mut request = if responses {
            json!({"model":"small","input":"hello","store":false,"max_output_tokens":16})
        } else {
            user_request()
        };
        request["model"] = json!("small");
        request["cloud_allowed"] = json!(false);
        request["local_only"] = json!(true);
        let response = invoke(mock.runtime(configured()), request.clone(), responses).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("x-slotline-semantic-profile")
                .is_none()
        );
        let _ = body_json(response).await;
        request["model"] = json!("synthetic");
        assert_eq!(mock.calls(), vec![request]);
    }
}

#[tokio::test]
async fn request_cloud_denial_is_rejected_before_work_on_real_endpoints() {
    for responses in [false, true] {
        for branch in ["rule", "default", "model", "fallback"] {
            let reply = if branch == "fallback" {
                decision_payload("PRIVATE_CLASSIFIER_SENTINEL")
            } else {
                decision_payload(r#"{"target":"cloud","confidence":1}"#)
            };
            let mock = Mock::start(reply, Duration::ZERO, StatusCode::OK).await;
            let directory = tempfile::tempdir().unwrap();
            let value = cloud_configured(responses, branch, &directory.path().join("absent-oauth"));
            let runtime = mock.runtime(value);
            let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
            assert!(credential.cloud_allowed);
            assert!(semantic::allowed_alias(
                &runtime.policy.config,
                &credential.config,
                "cloud"
            ));
            let app_state = Arc::new(AppState {
                current: RwLock::new(runtime.clone()),
                reload_lock: tokio::sync::Mutex::new(()),
                path: PathBuf::from("unused-fixture"),
                hindsight_admission: hindsight::Admission::default(),
            });
            let router = Router::new()
                .route("/v1/chat/completions", post(current_chat))
                .route("/v1/responses", post(current_responses))
                .with_state(app_state);
            let original = if responses {
                json!({"model":"auto","input":"hello","store":false,"max_output_tokens":16})
            } else {
                user_request()
            };
            for field in ["cloud_allowed", "local_only"] {
                for nested in [false, true] {
                    let mut request = original.clone();
                    let denial = json!(field == "local_only");
                    if nested {
                        request["extra_body"] = json!({field:denial});
                    } else {
                        request[field] = denial;
                    }
                    let response = router
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method(Method::POST)
                                .uri(if responses {
                                    "/v1/responses"
                                } else {
                                    "/v1/chat/completions"
                                })
                                .header(
                                    header::AUTHORIZATION,
                                    headers()[header::AUTHORIZATION].clone(),
                                )
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(request.to_string()))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = body_json(response).await;
                    assert_eq!(
                        status,
                        StatusCode::BAD_REQUEST,
                        "branch={branch} request={request} body={body}"
                    );
                    assert_eq!(body["error"]["code"], "unsupported_parameter");
                    assert_eq!(
                        body["error"]["message"],
                        format!("Unsupported semantic request policy argument: {field}")
                    );
                    assert!(mock.calls().is_empty());
                    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
                    assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
                    assert_eq!(runtime.semantic_admission.available_permits(), 1);
                    assert_eq!(credential.class_semaphore.available_permits(), 1);
                    assert_eq!(runtime.semaphores[0].available_permits(), 1);
                }
            }
            let control = invoke(runtime.clone(), original, responses).await;
            assert_eq!(control.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(control.headers()["x-slotline-semantic-target"], "cloud");
            assert_eq!(
                control.headers()["x-slotline-semantic-outcome"],
                if branch == "fallback" {
                    "invalid"
                } else {
                    branch
                }
            );
            assert_eq!(
                body_json(control).await["error"]["code"],
                "upstream_auth_error"
            );
            assert_eq!(
                mock.calls().len(),
                usize::from(matches!(branch, "model" | "fallback"))
            );
            assert!(mock.calls().iter().all(is_classifier));
        }
    }
}

#[tokio::test]
async fn responses_complete_message_boundary_precedes_all_cloud_dispatch_branches() {
    for branch in ["rule", "default", "model", "fallback"] {
        let reply = if branch == "fallback" {
            decision_payload("PRIVATE_CLASSIFIER_SENTINEL")
        } else {
            decision_payload(r#"{"target":"cloud","confidence":1}"#)
        };
        let mock = Mock::start(reply, Duration::ZERO, StatusCode::OK).await;
        let directory = tempfile::tempdir().unwrap();
        let mut value = cloud_configured(true, branch, &directory.path().join("absent-oauth"));
        value["credentials"][0]["burst"] = json!(2048);
        let runtime = mock.runtime(value);
        let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
        assert!(credential.cloud_allowed);
        assert!(semantic::allowed_alias(
            &runtime.policy.config,
            &credential.config,
            "cloud"
        ));
        let app = Arc::new(AppState {
            current: RwLock::new(runtime.clone()),
            reload_lock: tokio::sync::Mutex::new(()),
            path: PathBuf::from("unused-fixture"),
            hindsight_admission: hindsight::Admission::default(),
        });
        let router = Router::new()
            .route("/v1/responses", post(current_responses))
            .with_state(app);
        for item in unsupported_responses_items() {
            for nested in [false, true] {
                for last in [false, true] {
                    let user = json!({"role":"user","content":"hello"});
                    let input = if last {
                        json!([user, item])
                    } else {
                        json!([item, user])
                    };
                    let mut request = json!({"model":"auto","input":"hello","store":false});
                    if nested {
                        request.as_object_mut().unwrap().remove("input");
                        request["extra_body"] = json!({"input":input});
                    } else {
                        request["input"] = input;
                    }
                    let response = router
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method(Method::POST)
                                .uri("/v1/responses")
                                .header(
                                    header::AUTHORIZATION,
                                    headers()[header::AUTHORIZATION].clone(),
                                )
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(request.to_string()))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = body_json(response).await;
                    assert_eq!(
                        status,
                        StatusCode::BAD_REQUEST,
                        "branch={branch} request={request} body={body}"
                    );
                    assert_eq!(body["error"]["code"], "unsupported_continuation");
                    assert!(mock.calls().is_empty());
                    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
                    assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
                    assert_eq!(runtime.semantic_admission.available_permits(), 1);
                    assert_eq!(credential.class_semaphore.available_permits(), 1);
                    assert_eq!(runtime.semaphores[0].available_permits(), 1);
                }
            }
        }
        for (index, message) in supported_responses_messages().into_iter().enumerate() {
            let request = json!({"model":"auto","input":[message],"store":false});
            let response = invoke(runtime.clone(), request, true).await;
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(response.headers()["x-slotline-semantic-target"], "cloud");
            assert_eq!(
                response.headers()["x-slotline-semantic-outcome"],
                if branch == "fallback" {
                    "invalid"
                } else {
                    branch
                }
            );
            assert_eq!(
                body_json(response).await["error"]["code"],
                "upstream_auth_error"
            );
            assert_eq!(
                mock.calls().len(),
                (index + 1) * usize::from(matches!(branch, "model" | "fallback"))
            );
            assert!(mock.calls().iter().all(is_classifier));
        }
    }
}

#[tokio::test]
async fn responses_complete_messages_preserve_normalized_outbound_body() {
    for message in supported_responses_messages() {
        for nested in [false, true] {
            let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
            let input = json!([message]);
            let mut request = json!({"model":"auto","input":input,"store":false});
            if nested {
                request.as_object_mut().unwrap().remove("input");
                request["extra_body"] = json!({"input":input});
            }
            let response = invoke(mock.runtime(configured()), request.clone(), true).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-slotline-semantic-outcome"], "model");
            let mut expected = semantic::inspect_request(&request).unwrap();
            expected["model"] = json!("synthetic");
            assert_eq!(mock.calls().len(), 2);
            assert_eq!(mock.calls()[1], expected);
        }
    }
}

#[tokio::test]
async fn unauthorized_candidates_are_not_model_visible_and_cannot_be_selected() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    let mut value = configured();
    value["credentials"][0]["aliases"] = json!(["auto", "small", "judge"]);
    let response = invoke(mock.runtime(value), user_request(), false).await;
    assert_eq!(response.headers()["x-slotline-semantic-outcome"], "invalid");
    assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
    let prompt: Value =
        serde_json::from_str(mock.calls()[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(prompt["candidates"], json!(["small"]));
    assert_eq!(prompt["user_text"], "hello");
    assert!(!mock.calls()[0].to_string().contains("large"));
}

#[tokio::test]
async fn classifier_uses_same_native_alias_without_callback_recursion() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    let mut value = configured();
    value["semantic_routing"]["profiles"]["auto"]["decision"]["classifier"] = json!("small");
    let response = invoke(mock.runtime(value), user_request(), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls().len(), 2);
}

#[tokio::test]
async fn timeout_and_cancellation_release_decision_class_and_physical_capacity() {
    for cancel in [false, true] {
        let mock = Mock::start(valid_decision(), Duration::from_secs(5), StatusCode::OK).await;
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"]["decision"]["timeout_ms"] =
            json!(if cancel { 10000 } else { 50 });
        value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!("error");
        let runtime = mock.runtime(value);
        let task = tokio::spawn(invoke(runtime.clone(), user_request(), false));
        tokio::time::timeout(Duration::from_secs(2), async {
            while mock.calls().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        if cancel {
            assert_eq!(runtime.semantic_admission.available_permits(), 0);
            let overloaded = invoke(runtime.clone(), user_request(), false).await;
            assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(overloaded.headers()["x-slotline-semantic-outcome"], "busy");
            assert_eq!(mock.calls().len(), 1);
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            let response = task.await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()["x-slotline-semantic-outcome"], "timeout");
        }
        assert_eq!(runtime.semantic_admission.available_permits(), 1);
        assert_eq!(runtime.semaphores[0].available_permits(), 1);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .available_permits(),
            1
        );
        assert_eq!(mock.calls().len(), 1);
    }
}

#[tokio::test]
async fn status_errors_and_deadlines_can_fall_back_but_never_replay_classifier_text() {
    for timeout in [false, true] {
        let mock = Mock::start(
            json!({"error":{"message":"PRIVATE_CLASSIFIER_SENTINEL"}}),
            if timeout {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            },
            StatusCode::BAD_GATEWAY,
        )
        .await;
        let generation = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"]["decision"]["timeout_ms"] = json!(50);
        let runtime = mock.split_runtime(&generation, value);
        let response = invoke(runtime.clone(), user_request(), false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
        assert_eq!(
            response.headers()["x-slotline-semantic-outcome"],
            if timeout { "timeout" } else { "unavailable" }
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_CLASSIFIER_SENTINEL"));
        assert_eq!(mock.calls().len(), 1);
        assert_eq!(generation.calls().len(), 1);
        assert_eq!(
            runtime.observations.lock().unwrap()[0].status,
            RouteStatus::Serving
        );
        if !timeout {
            assert_eq!(
                runtime.observations.lock().unwrap()[1].status,
                RouteStatus::CoolingDown
            );
        }
    }
}

#[tokio::test]
async fn denied_cloud_target_is_filtered_before_native_classifier_and_cannot_be_selected() {
    for denied_permission in ["cloud", "route"] {
        let mock = Mock::start(
            decision_payload(r#"{"target":"cloud","confidence":1}"#),
            Duration::ZERO,
            StatusCode::OK,
        )
        .await;
        let mut value = configured();
        value["codex"] = json!({"enabled":true,"alias":"cloud","model":"synthetic"});
        value["semantic_routing"]["profiles"]["auto"]["targets"] =
            json!(["small", "large", "cloud"]);
        value["credentials"][0]["aliases"]
            .as_array_mut()
            .unwrap()
            .push(json!("cloud"));
        value["credentials"][0]["cloud_allowed"] = json!(denied_permission != "cloud");
        if denied_permission == "cloud" {
            value["credentials"][0]["allowed_routes"]
                .as_array_mut()
                .unwrap()
                .push(json!("codex"));
        }
        let response = invoke(mock.runtime(value), user_request(), false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-slotline-semantic-target"], "small");
        let prompt: Value =
            serde_json::from_str(mock.calls()[0]["messages"][1]["content"].as_str().unwrap())
                .unwrap();
        assert_eq!(prompt["candidates"], json!(["small", "large"]));
        assert_eq!(mock.calls().len(), 2);
    }
}

#[tokio::test]
async fn actual_classifier_class_and_physical_admission_are_not_bypassed() {
    for class in [true, false] {
        let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
        let mut value = configured();
        value["semantic_routing"]["profiles"]["auto"]["decision"]["on_failure"] = json!("error");
        let runtime = mock.runtime(value);
        let semaphore = if class {
            runtime.auth.read().unwrap().credentials["fixture"]
                .class_semaphore
                .clone()
        } else {
            runtime.semaphores[0].clone()
        };
        let held = semaphore.acquire_owned().await.unwrap();
        let response = invoke(runtime.clone(), user_request(), false).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers()["x-slotline-semantic-outcome"],
            "unavailable"
        );
        assert!(mock.calls().is_empty());
        assert_eq!(runtime.semantic_admission.available_permits(), 1);
        drop(held);
    }
}

#[tokio::test]
async fn disabled_and_explicit_models_never_depend_on_classifier() {
    for enabled in [false, true] {
        let mock = Mock::start(
            json!({"error":"unavailable"}),
            Duration::ZERO,
            StatusCode::BAD_GATEWAY,
        )
        .await;
        let mut value = configured();
        value["semantic_routing"]["enabled"] = json!(enabled);
        let runtime = mock.runtime(value);
        let mut request = user_request();
        request["model"] = json!("small");
        let response = invoke(runtime.clone(), request, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("x-slotline-semantic-profile")
                .is_none()
        );
        assert_eq!(mock.calls().len(), 1);
        if !enabled {
            assert_eq!(
                invoke(runtime, user_request(), false).await.status(),
                StatusCode::NOT_FOUND
            );
            assert_eq!(mock.calls().len(), 1);
        }
    }
}

#[tokio::test]
async fn responses_tools_structured_output_and_both_sse_contracts_are_preserved() {
    for responses in [false, true] {
        for stream in [false, true] {
            let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
            let mut request = if responses {
                json!({"model":"auto","input":"hello","instructions":"debug system text","max_output_tokens":16,
                "tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}],
                "text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object","properties":{}}}}})
            } else {
                user_request()
            };
            request["stream"] = json!(stream);
            let response = invoke(mock.runtime(configured()), request.clone(), responses).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "responses={responses}, stream={stream}"
            );
            assert_eq!(response.headers()["x-slotline-semantic-target"], "large");
            let bytes = axum::body::to_bytes(response.into_body(), 16384)
                .await
                .unwrap();
            if stream {
                assert!(String::from_utf8_lossy(&bytes).contains(if responses {
                    "response.completed"
                } else {
                    "[DONE]"
                }));
            } else {
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["model"],
                    "large"
                );
            }
            request["model"] = json!("synthetic");
            assert_eq!(mock.calls()[1], request);
        }
    }
}

#[tokio::test]
async fn target_validation_does_not_strip_unknown_or_incompatible_fields() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    let mut value = configured();
    value["aliases"]["large"]["capabilities"] = json!(["text", "responses"]);
    let mut request = user_request();
    request["tools"] =
        json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]);
    let response = invoke(mock.runtime(value), request, false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.calls().len(), 1);
}

#[tokio::test]
async fn real_endpoint_dispatch_discovery_embeddings_and_authentication_boundaries() {
    let mock = Mock::start(valid_decision(), Duration::ZERO, StatusCode::OK).await;
    let runtime = mock.runtime(configured());
    let app_state = Arc::new(AppState {
        current: RwLock::new(runtime),
        reload_lock: tokio::sync::Mutex::new(()),
        path: PathBuf::from("unused-fixture"),
        hindsight_admission: hindsight::Admission::default(),
    });
    let router = Router::new()
        .route("/v1/models", get(current_models))
        .route("/v1/chat/completions", post(current_chat))
        .route("/v1/responses", post(current_responses))
        .route("/v1/embeddings", post(current_embeddings))
        .with_state(app_state)
        .layer(axum::middleware::from_fn(request_logging::correlate));
    for (path, method, payload, authorized, status) in [
        ("/v1/models", Method::GET, json!(null), true, StatusCode::OK),
        (
            "/v1/chat/completions",
            Method::POST,
            user_request(),
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/v1/embeddings",
            Method::POST,
            json!({"model":"auto","input":"hello"}),
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/v1/chat/completions",
            Method::POST,
            user_request(),
            true,
            StatusCode::OK,
        ),
        (
            "/v1/responses",
            Method::POST,
            json!({"model":"auto","input":"hello","max_output_tokens":16}),
            true,
            StatusCode::OK,
        ),
    ] {
        let mut builder = Request::builder().uri(path).method(method);
        if authorized {
            builder = builder.header(
                header::AUTHORIZATION,
                headers()[header::AUTHORIZATION].clone(),
            );
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        let body = body_json(response).await;
        if path == "/v1/models" {
            assert!(
                body["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|model| model["id"] == "auto")
            );
        }
    }
    assert_eq!(mock.calls().len(), 4);
}
