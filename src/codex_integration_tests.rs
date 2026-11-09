use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::atomic::AtomicBool;
use tower::ServiceExt;

const WORKLOAD: &str = "synthetic-codex-workload-0123456789";
const ACCESS: &str = "synthetic-account-access-NOT-A-REAL-TOKEN";
const REFRESH: &str = "synthetic-refresh-NOT-A-REAL-TOKEN";

fn id_token(account: &str) -> String {
    format!(
        "e30.{}.synthetic",
        URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth":{"chatgpt_account_id":account}}).to_string()
        )
    )
}

fn account() -> codex_auth::Account {
    codex_auth::Account {
        access_token: ACCESS.into(),
        refresh_token: REFRESH.into(),
        id_token: id_token("fixture-account"),
        account_id: "fixture-account".into(),
        expires_at: codex_auth::now() + 3600,
        refresh_failed: false,
    }
}

fn completed(output: Value) -> Value {
    json!({"id":"resp_fixture","object":"response","model":"gpt-fixture","status":"completed","output":output,"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}}})
}

fn text_item() -> Value {
    json!({"id":"msg_fixture","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello","annotations":[]}]})
}

fn call_item() -> Value {
    json!({"id":"fc_fixture","type":"function_call","call_id":"call_fixture","name":"lookup","arguments":"{\"key\":1}","status":"completed"})
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn text_sse() -> String {
    event(
        json!({"type":"response.created","response":{"id":"resp_fixture","object":"response","model":"gpt-fixture","status":"in_progress","output":[]}}),
    ) + &event(
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hel"}),
    ) + &event(
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"lo"}),
    ) + &event(json!({"type":"response.completed","response":completed(json!([text_item()]))}))
}

fn tool_sse(empty_terminal: bool) -> String {
    let mut added = call_item();
    added["arguments"] = json!("");
    added["status"] = json!("in_progress");
    event(json!({"type":"response.output_item.added","output_index":0,"item":added}))
        + &event(
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture","delta":"{\"key\":"}),
        )
        + &event(
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_fixture","delta":"1}"}),
        )
        + &event(json!({"type":"response.output_item.done","output_index":0,"item":call_item()}))
        + &event(
            json!({"type":"response.completed","response":completed(if empty_terminal {json!([])} else {json!([call_item()])})}),
        )
}

type RecordedRequest = (String, HeaderMap, Vec<u8>);

struct FakeState {
    requests: Mutex<Vec<RecordedRequest>>,
    polls: AtomicU64,
    refreshes: AtomicU64,
    pending: AtomicU64,
    auth_mode: AtomicU64,
    provider_status: Mutex<StatusCode>,
    redirect: Mutex<String>,
    sse: Mutex<String>,
    hang: AtomicBool,
}

struct Fake {
    base: String,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake() -> Fake {
    let state = Arc::new(FakeState {
        requests: Mutex::new(Vec::new()),
        polls: AtomicU64::new(0),
        refreshes: AtomicU64::new(0),
        pending: AtomicU64::new(0),
        auth_mode: AtomicU64::new(0),
        provider_status: Mutex::new(StatusCode::OK),
        redirect: Mutex::new("/should-never-follow".into()),
        sse: Mutex::new(text_sse()),
        hang: AtomicBool::new(false),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("synthetic Codex fixture requires loopback bind");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(post(fake_handler))
        .with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Fake { base, state, task }
}

async fn fake_handler(
    State(state): State<Arc<FakeState>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let path = uri.path();
    state
        .requests
        .lock()
        .unwrap()
        .push((path.into(), headers.clone(), bytes.to_vec()));
    let mode = state.auth_mode.load(Ordering::Relaxed);
    match path {
        "/api/accounts/deviceauth/usercode" => {
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"client_id":codex_auth::CLIENT_ID})
            );
            if mode == 13 {
                return axum::Json(json!({"user_code":"missing-device"})).into_response();
            }
            axum::Json(
                json!({"device_auth_id":"synthetic-device","user_code":"TEST-CODE","interval":"5"}),
            )
            .into_response()
        }
        "/api/accounts/deviceauth/token" => {
            let poll = state.polls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"device_auth_id":"synthetic-device","user_code":"TEST-CODE"})
            );
            if mode == 1 {
                return (StatusCode::UNAUTHORIZED, ACCESS).into_response();
            }
            if mode == 2 {
                return axum::Json(json!({"authorization_code":"incomplete"})).into_response();
            }
            if poll < state.pending.load(Ordering::Relaxed) {
                return (
                    if poll.is_multiple_of(2) {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::NOT_FOUND
                    },
                    "not yet",
                )
                    .into_response();
            }
            axum::Json(json!({"authorization_code":"synthetic-code","code_challenge":"synthetic-challenge","code_verifier":"synthetic-verifier"})).into_response()
        }
        "/oauth/token" => {
            if headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
            {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    value,
                    json!({"client_id":codex_auth::CLIENT_ID,"grant_type":"refresh_token","refresh_token":REFRESH,"scope":"openid profile email"})
                );
                state.refreshes.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(50)).await;
            } else {
                let form = String::from_utf8(bytes.to_vec()).unwrap();
                assert!(form.contains("grant_type=authorization_code"));
                assert!(form.contains("code=synthetic-code"));
                assert!(form.contains("code_verifier=synthetic-verifier"));
                assert!(form.contains(
                    "redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"
                ));
            }
            if mode == 3 {
                return (
                    StatusCode::UNAUTHORIZED,
                    format!("secret {ACCESS} {REFRESH}"),
                )
                    .into_response();
            }
            if mode == 4 {
                return axum::http::Response::builder()
                    .status(307)
                    .header("location", state.redirect.lock().unwrap().as_str())
                    .body(Body::from(ACCESS))
                    .unwrap();
            }
            let mut value = json!({"access_token":"synthetic-rotated-access","refresh_token":"synthetic-rotated-refresh","id_token":id_token("fixture-account"),"expires_in":3600});
            if mode == 5 {
                value.as_object_mut().unwrap().remove("access_token");
            }
            if mode == 6 {
                value["id_token"] = json!("e30.e30.synthetic");
            }
            if mode == 7 {
                value["expires_in"] = json!(0);
            }
            if mode == 8 {
                value.as_object_mut().unwrap().remove("refresh_token");
                value.as_object_mut().unwrap().remove("id_token");
            }
            if mode == 9 {
                value["id_token"] = json!(id_token("different-account"));
            }
            if mode == 11 {
                return Response::new(Body::from("x".repeat(128 * 1024 + 1)));
            }
            axum::Json(value).into_response()
        }
        "/backend-api/codex/responses" => {
            let status = *state.provider_status.lock().unwrap();
            if status != StatusCode::OK {
                return axum::http::Response::builder()
                    .status(status)
                    .header("location", state.redirect.lock().unwrap().as_str())
                    .body(Body::from(format!("{ACCESS} {REFRESH} prompt-secret")))
                    .unwrap();
            }
            let sse = state.sse.lock().unwrap().clone();
            let hang = state.hang.load(Ordering::Relaxed);
            let output = async_stream::stream! {
                if hang {
                    yield Ok::<_, io::Error>(Bytes::from(event(json!({"type":"response.output_text.delta","delta":"hello"}))));
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                for chunk in sse.as_bytes().chunks(7) { yield Ok(Bytes::copy_from_slice(chunk)); }
            };
            axum::http::Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
                .body(Body::from_stream(output))
                .unwrap()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "unexpected fixture path").into_response(),
    }
}

use axum::response::IntoResponse;

struct Fixture {
    _dir: tempfile::TempDir,
    app: Arc<AppState>,
    router: Router,
    config: config::CodexConfig,
}

fn fixture(fake: &Fake, cloud: Option<bool>, route: bool) -> Fixture {
    fixture_inner(&fake.base, cloud, route, true)
}

fn fixture_inner(base: &str, cloud: Option<bool>, route: bool, persist: bool) -> Fixture {
    fixture_with_operator(base, cloud, route, persist, false)
}

fn fixture_with_operator(
    base: &str,
    cloud: Option<bool>,
    route: bool,
    persist: bool,
    operator: bool,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let key = dir.path().join("workload.key");
    fs::write(&key, WORKLOAD).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let path = dir.path().join("policy.json");
    let mut credential = json!({"name":"fixture","key_file":key,"class":"test","aliases":["cloud-test","local-test"],"max_in_flight":2,"burst":100,"operator":operator});
    if let Some(cloud) = cloud {
        credential["cloud_allowed"] = json!(cloud);
    }
    if route {
        credential["allowed_routes"] = json!(["codex", "local"]);
    }
    let account_path = if persist {
        dir.path().join("account.json")
    } else {
        PathBuf::from("/var/lib/slotline-nonexistent-fixture/account.json")
    };
    let value = json!({"bind":"100.127.255.254","port":14022,
        "codex":{"enabled":true,"alias":"cloud-test","model":"gpt-fixture","account_file":account_path,"auth_base_url":base,"responses_url":format!("{base}/backend-api/codex/responses")},
        "credentials":[credential],
        "routes":[{"name":"local","kind":"local","base_url":format!("{base}/v1"),"model":"local-fixture","safe_concurrency":1,"context_limit":2000,"adapter_version":"synthetic","capabilities":["text","responses"],"streaming":true,"enabled":true}],
        "aliases":{"local-test":{"context_limit":2000,"capabilities":["text","responses"],"routes":[{"route":"local","priority":0}]}}
    });
    fs::write(&path, value.to_string()).unwrap();
    let policy = config::load(&path).unwrap();
    let config = policy.config.codex.clone();
    if persist {
        codex_auth::Store::open(Path::new(config.account_file.as_deref().unwrap()))
            .unwrap()
            .write(&account())
            .unwrap();
    }
    let app = Arc::new(AppState::new(path, policy).unwrap());
    let router = Router::new()
        .route("/v1/responses", post(current_responses))
        .route("/v1/chat/completions", post(current_chat))
        .route("/v1/embeddings", post(current_embeddings))
        .route("/v1/models", get(current_models))
        .with_state(app.clone());
    Fixture {
        _dir: dir,
        app,
        router,
        config,
    }
}

async fn invoke(fixture: &Fixture, endpoint: &str, value: Value) -> Response {
    fixture
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/{endpoint}"))
                .header(header::AUTHORIZATION, format!("Bearer {WORKLOAD}"))
                .header("ChatGPT-Account-Id", "attacker-account")
                .header("cookie", "private-caller-cookie")
                .header("x-api-key", "private-caller-api-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn codex_handlers_default_deny_all_cloud_and_direct_paths() {
    let fake = fake().await;
    for (cloud, route) in [(None, true), (Some(false), true), (Some(true), false)] {
        let fixture = fixture(&fake, cloud, route);
        for (endpoint, request) in [
            ("responses", json!({"model":"cloud-test","input":"hello"})),
            (
                "chat/completions",
                json!({"model":"cloud-test","messages":[{"role":"user","content":"hello"}]}),
            ),
            ("responses", json!({"model":"gpt-fixture","input":"hello"})),
            (
                "chat/completions",
                json!({"model":"gpt-fixture","messages":[{"role":"user","content":"hello"}]}),
            ),
        ] {
            assert_eq!(
                invoke(&fixture, endpoint, request).await.status(),
                StatusCode::FORBIDDEN
            );
        }
        let response = fixture
            .router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, format!("Bearer {WORKLOAD}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let catalog = json_body(response).await;
        assert_eq!(catalog["data"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["data"][0]["id"], "local-test");
        let anonymous = fixture
            .router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(fake.state.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn codex_handlers_responses_and_chat_stream_and_nonstream_are_native_and_private() {
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    for chat in [false, true] {
        for streaming in [false, true] {
            let request = if chat {
                json!({"model":"cloud-test","messages":[{"role":"system","content":"system text"},{"role":"user","content":"hello"}],"stream":streaming,"stream_options":{"include_usage":true}})
            } else {
                json!({"model":"cloud-test","input":"hello","stream":streaming})
            };
            let response = invoke(
                &fixture,
                if chat {
                    "chat/completions"
                } else {
                    "responses"
                },
                request,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let runtime = fixture.app.current();
            assert_eq!(runtime.codex_semaphore.available_permits(), 7);
            assert_eq!(runtime.semaphores[0].available_permits(), 1);
            assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
            let bytes = axum::body::to_bytes(response.into_body(), MAX_TEST_BODY)
                .await
                .unwrap();
            if streaming {
                let output = String::from_utf8(bytes.to_vec()).unwrap();
                assert!(output.contains(if chat {
                    "chat.completion.chunk"
                } else {
                    "response.completed"
                }));
                assert!(output.contains(if chat { "[DONE]" } else { "output_text" }));
                assert!(!output.contains("gpt-fixture"));
                if chat {
                    assert!(output.contains("prompt_tokens"));
                }
            } else {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["model"], "cloud-test");
                if chat {
                    assert_eq!(value["choices"][0]["message"]["content"], "hello");
                    assert_eq!(value["usage"]["prompt_tokens"], 7);
                } else {
                    assert_eq!(value["output"][0], text_item());
                }
            }
            assert_eq!(runtime.codex_semaphore.available_permits(), 8);
        }
    }
    for (path, headers, bytes) in fake.state.requests.lock().unwrap().iter() {
        assert_eq!(path, "/backend-api/codex/responses");
        assert_eq!(headers[header::AUTHORIZATION], format!("Bearer {ACCESS}"));
        assert_eq!(headers["chatgpt-account-id"], "fixture-account");
        assert_eq!(headers["originator"], "codex_cli_rs");
        for private in ["cookie", "x-api-key", "x-slotline-session"] {
            assert!(headers.get(private).is_none());
        }
        let value: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(value["model"], "gpt-fixture");
        assert_eq!(value["store"], false);
        assert_eq!(value["stream"], true);
        assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
        assert!(value["input"].is_array());
        assert!(value.get("messages").is_none());
        assert!(!String::from_utf8_lossy(bytes).contains(WORKLOAD));
    }
}

const MAX_TEST_BODY: usize = 2 * 1024 * 1024;

#[tokio::test]
async fn codex_native_tool_done_recovery_and_history_round_trip() {
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    *fake.state.sse.lock().unwrap() = tool_sse(true);
    let value = json_body(
        invoke(
            &fixture,
            "responses",
            json!({"model":"cloud-test","input":"lookup"}),
        )
        .await,
    )
    .await;
    assert_eq!(value["output"], json!([call_item()]));
    *fake.state.sse.lock().unwrap() = text_sse();
    let history = json!([
        {"type":"reasoning","id":"rs_fixture","summary":[{"type":"summary_text","text":"planning"}],"encrypted_content":"synthetic-encrypted-reasoning"},
        call_item(),
        {"type":"function_call_output","call_id":"call_fixture","output":"tool-result"},
        text_item(),
        {"role":"user","content":"continue"}
    ]);
    json_body(
        invoke(
            &fixture,
            "responses",
            json!({"model":"cloud-test","input":history,"include":["reasoning.encrypted_content"]}),
        )
        .await,
    )
    .await;
    let requests = fake.state.requests.lock().unwrap();
    let sent: Value = serde_json::from_slice(&requests.last().unwrap().2).unwrap();
    assert_eq!(sent["input"], history);
}

#[tokio::test]
async fn codex_chat_tools_stream_arguments_then_tool_result() {
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    let tools = json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"key":{"type":"integer"}},"required":["key"]}}}]);
    for streaming in [false, true] {
        *fake.state.sse.lock().unwrap() = tool_sse(false);
        let response = invoke(&fixture, "chat/completions", json!({"model":"cloud-test","messages":[{"role":"user","content":"lookup"}],"tools":tools,"stream":streaming})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_TEST_BODY)
            .await
            .unwrap();
        if streaming {
            let output = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(output.contains("tool_calls"));
            assert!(output.contains("call_fixture"));
            assert!(output.contains("[DONE]"));
            assert!(!output.contains("response."));
        } else {
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(value["choices"][0]["message"]["content"].is_null());
            assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(
                value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
                "{\"key\":1}"
            );
        }
    }
    *fake.state.sse.lock().unwrap() = text_sse();
    json_body(invoke(&fixture, "chat/completions", json!({"model":"cloud-test","messages":[
        {"role":"user","content":"lookup"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_fixture","type":"function","function":{"name":"lookup","arguments":"{\"key\":1}"}}]},
        {"role":"tool","tool_call_id":"call_fixture","content":"tool-result"}
    ],"tools":tools})).await).await;
    let requests = fake.state.requests.lock().unwrap();
    let sent: Value = serde_json::from_slice(&requests.last().unwrap().2).unwrap();
    assert_eq!(sent["input"][1]["type"], "function_call");
    assert_eq!(
        sent["input"][2],
        json!({"type":"function_call_output","call_id":"call_fixture","output":"tool-result"})
    );
    assert_eq!(sent["tools"][0]["name"], "lookup");
    assert!(sent["tools"][0].get("function").is_none());
}

#[tokio::test]
async fn codex_stream_failures_are_bounded_sanitized_and_not_successes() {
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    for bad in [
        "data: [DONE]\n\n".to_owned(),
        "data: {not json}\n\n".to_owned(),
        event(json!({"type":"response.output_text.delta","delta":"partial"})),
        event(json!({"type":"response.failed","response":{"error":{"message":ACCESS}}})),
        event(json!({"type":"response.completed","response":{"status":"completed","output":[]}})),
        format!("data: {}", "x".repeat(1024 * 1024 + 1)),
    ] {
        *fake.state.sse.lock().unwrap() = bad;
        let response = invoke(
            &fixture,
            "responses",
            json!({"model":"cloud-test","input":"hello"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(ACCESS));
        let response = invoke(
            &fixture,
            "responses",
            json!({"model":"cloud-test","input":"hello","stream":true}),
        )
        .await;
        let mut stream = response.into_body().into_data_stream();
        let mut failed = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Err(_) => {
                    failed = true;
                    break;
                }
                Ok(bytes) => assert!(!String::from_utf8_lossy(&bytes).contains(ACCESS)),
            }
        }
        assert!(failed);
    }
    assert_eq!(fixture.app.current().codex_semaphore.available_permits(), 8);
}

#[tokio::test]
async fn codex_provider_redirects_errors_and_cancellation_do_not_leak_or_claim_local_capacity() {
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TEMPORARY_REDIRECT,
    ] {
        codex_auth::Store::open(Path::new(fixture.config.account_file.as_deref().unwrap()))
            .unwrap()
            .write(&account())
            .unwrap();
        *fake.state.provider_status.lock().unwrap() = status;
        let response = invoke(
            &fixture,
            "responses",
            json!({"model":"cloud-test","input":"prompt-secret"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for secret in [ACCESS, REFRESH, "prompt-secret"] {
            assert!(!text.contains(secret));
        }
        if status == StatusCode::UNAUTHORIZED {
            assert_eq!(
                codex_auth::status(&fixture.config)["state"],
                "reauthentication_required"
            );
        }
    }
    assert_eq!(fake.state.requests.lock().unwrap().len(), 3);
    *fake.state.provider_status.lock().unwrap() = StatusCode::OK;
    fake.state.hang.store(true, Ordering::Relaxed);
    let runtime = fixture.app.current();
    let credential = runtime.auth.read().unwrap().credentials["fixture"].clone();
    let response = invoke(
        &fixture,
        "responses",
        json!({"model":"cloud-test","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(credential.class_semaphore.available_permits(), 1);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    let second = invoke(
        &fixture,
        "chat/completions",
        json!({"model":"cloud-test","messages":[{"role":"user","content":"hello"}],"stream":true}),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(credential.class_semaphore.available_permits(), 0);
    let denied = invoke(
        &fixture,
        "responses",
        json!({"model":"cloud-test","input":"hello"}),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(second);
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(credential.class_semaphore.available_permits(), 1);
    drop(stream);
    assert_eq!(credential.class_semaphore.available_permits(), 2);
    assert_eq!(runtime.codex_semaphore.available_permits(), 8);
    assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
    assert!(runtime.recent_routes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn codex_oauth_device_native_exchange_refresh_singleflight_and_status_redaction() {
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    fake.state.pending.store(2, Ordering::Relaxed);
    let instructions = Mutex::new(Vec::new());
    codex_auth::login(&fixture.config, |url, code| {
        instructions
            .lock()
            .unwrap()
            .push((url.to_owned(), code.to_owned()))
    })
    .await
    .unwrap();
    assert_eq!(fake.state.polls.load(Ordering::Relaxed), 3);
    assert_eq!(
        instructions.lock().unwrap()[0],
        (format!("{}/codex/device", fake.base), "TEST-CODE".into())
    );
    let store = codex_auth::Store::open(Path::new(fixture.config.account_file.as_deref().unwrap()))
        .unwrap();
    let mut expired = account();
    expired.expires_at = codex_auth::now() - 1;
    store.write(&expired).unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let config = fixture.config.clone();
        tasks.push(tokio::spawn(
            async move { codex_auth::access(&config).await },
        ));
    }
    for task in tasks {
        let fresh = task.await.unwrap().unwrap();
        assert_eq!(fresh.access_token, "synthetic-rotated-access");
        assert_eq!(fresh.refresh_token, "synthetic-rotated-refresh");
    }
    assert_eq!(fake.state.refreshes.load(Ordering::Relaxed), 1);
    let status = codex_auth::status(&fixture.config).to_string();
    for secret in [
        ACCESS,
        REFRESH,
        "fixture-account",
        "id_token",
        "synthetic-rotated",
    ] {
        assert!(!status.contains(secret));
    }
    assert!(status.contains("ready"));
}

#[tokio::test]
async fn codex_oauth_failure_and_missing_state_fail_closed_without_repeated_refresh() {
    let fake = fake().await;
    for mode in [3, 4, 5, 6, 7, 9, 11] {
        fake.state.auth_mode.store(mode, Ordering::Relaxed);
        let fixture = fixture(&fake, Some(true), true);
        let store =
            codex_auth::Store::open(Path::new(fixture.config.account_file.as_deref().unwrap()))
                .unwrap();
        let mut expired = account();
        expired.expires_at = codex_auth::now() - 1;
        store.write(&expired).unwrap();
        let before = fake.state.refreshes.load(Ordering::Relaxed);
        let (first, second) = tokio::join!(
            codex_auth::access(&fixture.config),
            codex_auth::access(&fixture.config)
        );
        assert!(first.is_err() && second.is_err());
        assert_eq!(fake.state.refreshes.load(Ordering::Relaxed), before + 1);
        assert!(
            codex_auth::status(&fixture.config)
                .to_string()
                .contains("reauthentication_required")
        );
        assert!(!first.err().unwrap().contains(ACCESS));
    }
    for mode in [1, 2, 6, 13] {
        fake.state.auth_mode.store(mode, Ordering::Relaxed);
        let fixture = fixture(&fake, Some(true), true);
        assert!(codex_auth::login(&fixture.config, |_, _| {}).await.is_err());
    }
    assert!(
        !fake
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _, _)| path == "/should-never-follow")
    );
}

#[tokio::test]
async fn codex_inprocess_handlers_enforce_permissions_without_network_or_account_state() {
    for (cloud, route) in [
        (None, true),
        (Some(false), true),
        (Some(true), false),
        (Some(true), true),
    ] {
        let fixture = fixture_inner("http://127.0.0.1:9", cloud, route, false);
        for (endpoint, body) in [
            ("responses", json!({"model":"cloud-test","input":"hello"})),
            (
                "chat/completions",
                json!({"model":"cloud-test","messages":[{"role":"user","content":"hello"}]}),
            ),
        ] {
            let response = invoke(&fixture, endpoint, body).await;
            assert_eq!(
                response.status(),
                if cloud == Some(true) && route {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::FORBIDDEN
                }
            );
            let runtime = fixture.app.current();
            assert_eq!(runtime.semaphores[0].available_permits(), 1);
            assert_eq!(runtime.codex_semaphore.available_permits(), 8);
            assert_eq!(runtime.route_attempts[0].load(Ordering::Relaxed), 0);
        }
        let direct = invoke(
            &fixture,
            "responses",
            json!({"model":"gpt-fixture","input":"hello"}),
        )
        .await;
        assert_eq!(direct.status(), StatusCode::FORBIDDEN);
        let anonymous = fixture
            .router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn codex_oauth_expiry_and_cross_origin_redirects_are_fail_closed() {
    let destination = fake().await;
    let fake = fake().await;
    let fixture = fixture(&fake, Some(true), true);
    fake.state.pending.store(u64::MAX, Ordering::Relaxed);
    let error = codex_auth::login_for(&fixture.config, |_, _| {}, Duration::from_millis(100))
        .await
        .err()
        .unwrap();
    assert_eq!(error, "Codex device sign-in expired");
    fake.state.auth_mode.store(4, Ordering::Relaxed);
    *fake.state.redirect.lock().unwrap() = format!("{}/should-never-follow", destination.base);
    let mut expired = account();
    expired.expires_at = codex_auth::now() - 1;
    codex_auth::Store::open(Path::new(fixture.config.account_file.as_deref().unwrap()))
        .unwrap()
        .write(&expired)
        .unwrap();
    assert!(codex_auth::access(&fixture.config).await.is_err());
    *fake.state.provider_status.lock().unwrap() = StatusCode::TEMPORARY_REDIRECT;
    codex_auth::Store::open(Path::new(fixture.config.account_file.as_deref().unwrap()))
        .unwrap()
        .write(&account())
        .unwrap();
    assert_eq!(
        invoke(
            &fixture,
            "responses",
            json!({"model":"cloud-test","input":"hello"})
        )
        .await
        .status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(destination.state.requests.lock().unwrap().is_empty());
}

#[test]
fn codex_storage_rejects_unsafe_permissions_symlinks_hardlinks_and_git_paths() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("account.json");
    let store = codex_auth::Store::open(&path).unwrap();
    assert!(store.read().is_err());
    store.write(&account()).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.read().is_err());
    assert!(store.write(&account()).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("link.json");
    symlink(&path, &link).unwrap();
    let linked = codex_auth::Store::open(&link).unwrap();
    assert!(linked.read().is_err());
    assert!(linked.write(&account()).is_err());
    fs::hard_link(&path, dir.path().join("hard.json")).unwrap();
    assert!(store.read().is_err());
    let nested = dir.path().join("state");
    fs::create_dir(&nested).unwrap();
    assert!(codex_auth::Store::open(&nested.join("account.json")).is_err());
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = dir.path().join("symlink-dir");
    symlink(&nested, &alias).unwrap();
    assert!(codex_auth::Store::open(&alias.join("account.json")).is_err());
    fs::create_dir(nested.join(".git")).unwrap();
    assert!(codex_auth::Store::open(&nested.join("account.json")).is_err());
    assert!(codex_auth::Store::open(&dir.path().join("state/../account.json")).is_err());
}

async fn assert_native_upstream_error_metric(fixture: &Fixture, expected: u64) {
    let runtime = fixture.app.current();
    assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), expected);
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {WORKLOAD}")).unwrap(),
    );
    let response = super::status::metrics(axum::extract::State(runtime), headers).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    let line = format!("slotline_upstream_errors_total {expected}");
    assert!(
        text.lines().any(|value| value == line),
        "counter missing from metrics export"
    );
}

fn metric_request(chat: bool, stream: bool) -> Value {
    if chat {
        json!({"model":"cloud-test","messages":[{"role":"user","content":"metric fixture"}],"stream":stream})
    } else {
        json!({"model":"cloud-test","input":"metric fixture","stream":stream})
    }
}

fn metric_truncated_sse() -> String {
    format!(
        "event: response.created\ndata: {}\n\n",
        json!({
            "type":"response.created",
            "response":{"id":"resp_metric","object":"response","model":"gpt-fixture","status":"in_progress","output":[]}
        })
    )
}

#[tokio::test]
async fn codex_upstream_error_metric_counts_http_failures_once() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_GATEWAY,
    ] {
        let fake = fake().await;
        *fake.state.provider_status.lock().unwrap() = status;
        let fixture = fixture_with_operator(&fake.base, Some(true), true, true, true);
        let response = invoke(&fixture, "responses", metric_request(false, false)).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_native_upstream_error_metric(&fixture, 1).await;
        let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_native_upstream_error_metric(&fixture, 1).await;
    }
}

#[tokio::test]
async fn codex_upstream_error_metric_counts_buffered_protocol_failures_once() {
    for chat in [false, true] {
        let fake = fake().await;
        *fake.state.sse.lock().unwrap() = metric_truncated_sse();
        let fixture = fixture_with_operator(&fake.base, Some(true), true, true, true);
        let endpoint = if chat {
            "chat/completions"
        } else {
            "responses"
        };
        let response = invoke(&fixture, endpoint, metric_request(chat, false)).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_native_upstream_error_metric(&fixture, 1).await;
    }
}

#[tokio::test]
async fn codex_upstream_error_metric_counts_stream_failures_once_after_headers() {
    for chat in [false, true] {
        let fake = fake().await;
        *fake.state.sse.lock().unwrap() = metric_truncated_sse();
        let fixture = fixture_with_operator(&fake.base, Some(true), true, true, true);
        let endpoint = if chat {
            "chat/completions"
        } else {
            "responses"
        };
        let response = invoke(&fixture, endpoint, metric_request(chat, true)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_native_upstream_error_metric(&fixture, 0).await;
        assert!(
            axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .is_err()
        );
        assert_native_upstream_error_metric(&fixture, 1).await;
        assert_native_upstream_error_metric(&fixture, 1).await;
    }
}

#[tokio::test]
async fn codex_upstream_error_metric_excludes_success_and_client_cancellation() {
    let fake = fake().await;
    let fixture = fixture_with_operator(&fake.base, Some(true), true, true, true);
    for chat in [false, true] {
        for stream in [false, true] {
            let endpoint = if chat {
                "chat/completions"
            } else {
                "responses"
            };
            let response = invoke(&fixture, endpoint, metric_request(chat, stream)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            assert_native_upstream_error_metric(&fixture, 0).await;
        }
    }
    *fake.state.sse.lock().unwrap() = metric_truncated_sse();
    let response = invoke(&fixture, "responses", metric_request(false, true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    assert_native_upstream_error_metric(&fixture, 0).await;
}

#[tokio::test]
async fn codex_upstream_error_metric_excludes_policy_and_validation_refusals() {
    let fake = fake().await;
    let denied = fixture(&fake, Some(false), true);
    let response = invoke(&denied, "responses", metric_request(false, false)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        denied.app.current().upstream_errors.load(Ordering::Relaxed),
        0
    );
    assert!(fake.state.requests.lock().unwrap().is_empty());
    let invalid = fixture(&fake, Some(true), true);
    let response = invoke(
        &invalid,
        "responses",
        json!({"model":"cloud-test","input":123}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid
            .app
            .current()
            .upstream_errors
            .load(Ordering::Relaxed),
        0
    );
    assert!(fake.state.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn codex_upstream_error_metric_counts_transport_failures_once() {
    let fake = fake().await;
    let fixture = fixture_with_operator(&fake.base, Some(true), true, true, true);
    fake.task.abort();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !fake.task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let response = invoke(&fixture, "responses", metric_request(false, false)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_native_upstream_error_metric(&fixture, 1).await;
}

#[tokio::test]
async fn codex_upstream_error_metric_excludes_missing_account_preflight() {
    let fake = fake().await;
    let fixture = fixture_with_operator(&fake.base, Some(true), true, false, true);
    let response = invoke(&fixture, "responses", metric_request(false, false)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_native_upstream_error_metric(&fixture, 0).await;
    assert!(fake.state.requests.lock().unwrap().is_empty());
}
