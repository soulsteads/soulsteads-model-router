use super::*;
use crate::config::{CopilotConfig, CopilotEndpoint, CopilotModel, Credential};
use crate::copilot_auth::{API_VERSION, COPILOT_PLUGIN_VERSION, COPILOT_USER_AGENT};
use reqwest::{Client, Url, redirect::Policy};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::OwnedSemaphorePermit;

#[path = "copilot_deadline.rs"]
mod deadline;

#[path = "copilot_stream.rs"]
mod stream;

#[cfg(test)]
#[path = "copilot_timeout_tests.rs"]
mod timeout_tests;

pub(crate) fn eligible(credential: &Credential, config: &CopilotConfig) -> bool {
    config.enabled
        && credential.cloud_allowed
        && credential
            .allowed_routes
            .iter()
            .any(|route| route == "copilot")
        && config
            .models
            .iter()
            .any(|model| credential.aliases.contains(&model.alias))
}

pub(crate) fn endpoint_path(endpoint: CopilotEndpoint) -> &'static str {
    match endpoint {
        CopilotEndpoint::ChatCompletions => "/chat/completions",
        CopilotEndpoint::Responses => "/responses",
    }
}

fn inbound_endpoint_matches(configured: CopilotEndpoint, inbound: CopilotEndpoint) -> bool {
    configured == inbound
}

fn prepare_request(request: &Value, model: &CopilotModel) -> Result<Value, &'static str> {
    if request.get("model").and_then(Value::as_str) != Some(model.alias.as_str()) {
        return Err("Copilot request model does not match its configured alias");
    }
    let mut request = request.clone();
    let object = request
        .as_object_mut()
        .ok_or("Copilot request must be a JSON object")?;
    object.insert("model".into(), Value::String(model.model.clone()));
    Ok(request)
}

fn inference_url(api_endpoint: &Url, endpoint: CopilotEndpoint) -> Result<Url, &'static str> {
    api_endpoint
        .join(endpoint_path(endpoint).trim_start_matches('/'))
        .map_err(|_| "Invalid Copilot API endpoint")
}

fn inference_client() -> Result<Client, &'static str> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "Copilot transport is unavailable")
}

fn inference_request(
    client: &Client,
    url: Url,
    body: &Value,
    bearer: &str,
) -> reqwest::RequestBuilder {
    client
        .post(url)
        .bearer_auth(bearer)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("copilot-integration-id", "vscode-chat")
        .header("editor-version", "vscode/1.95.0")
        .header("editor-plugin-version", COPILOT_PLUGIN_VERSION)
        .header("openai-intent", "conversation-panel")
        .header("x-github-api-version", API_VERSION)
        .header("user-agent", COPILOT_USER_AGENT)
        .json(body)
}

async fn send_request(
    client: &Client,
    url: Url,
    body: &Value,
    bearer: &str,
) -> Result<reqwest::Response, &'static str> {
    inference_request(client, url, body, bearer)
        .send()
        .await
        .map_err(|_| "Copilot inference request failed")
}

fn provider_error(status: StatusCode) -> Response {
    let (local_status, code, message) = match status {
        StatusCode::TOO_MANY_REQUESTS => (
            StatusCode::TOO_MANY_REQUESTS,
            "upstream_rate_limited",
            "Copilot rate limit reached",
        ),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => (
            StatusCode::BAD_REQUEST,
            "upstream_invalid_request",
            "Copilot rejected the request",
        ),
        _ => (
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "Copilot inference failed",
        ),
    };
    json_error(local_status, code, message)
}

fn response_header_allowed(name: &reqwest::header::HeaderName) -> bool {
    name == reqwest::header::CONTENT_TYPE
        || name == reqwest::header::CACHE_CONTROL
        || name == reqwest::header::RETRY_AFTER
        || name == reqwest::header::HeaderName::from_static("x-request-id")
        || name == reqwest::header::HeaderName::from_static("x-github-request-id")
}

fn stream_response(
    upstream: reqwest::Response,
    class_permit: OwnedSemaphorePermit,
    clock: crate::sse::ProgressClock,
    endpoint: CopilotEndpoint,
) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let sse = clock.tracks_progress()
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
    let body = stream::forward(upstream, class_permit, clock, endpoint, sse);
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = status;
    for (name, value) in headers {
        if let Some(name) = name
            && response_header_allowed(&name)
            && let Ok(value) = HeaderValue::from_bytes(value.as_bytes())
        {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

pub(crate) async fn forward(
    runtime: Arc<Runtime>,
    credential: Arc<CredentialState>,
    headers: HeaderMap,
    request: Value,
    endpoint: CopilotEndpoint,
    class_permit: OwnedSemaphorePermit,
) -> Response {
    let upstream_errors = runtime.upstream_errors.clone();
    let response = forward_inner(
        runtime,
        credential,
        headers,
        request,
        endpoint,
        class_permit,
    )
    .await;
    if matches!(
        response.status(),
        StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT
    ) {
        upstream_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    response
}

async fn forward_inner(
    runtime: Arc<Runtime>,
    credential: Arc<CredentialState>,
    _headers: HeaderMap,
    request: Value,
    endpoint: CopilotEndpoint,
    class_permit: OwnedSemaphorePermit,
) -> Response {
    let config = &runtime.policy.config.copilot;
    let Some(alias) = request.get("model").and_then(Value::as_str) else {
        runtime
            .rejects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return json_error(StatusCode::BAD_REQUEST, "invalid_model", "Missing model");
    };
    let Some(model) = config.model_for_alias(alias).cloned() else {
        runtime
            .rejects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return json_error(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "Model unavailable",
        );
    };
    if !eligible(&credential.config, config) {
        runtime
            .rejects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return json_error(
            StatusCode::FORBIDDEN,
            "cloud_not_allowed",
            "Copilot requires explicit workload, model and route permission",
        );
    }
    if !credential.config.aliases.contains(&model.alias) {
        runtime
            .rejects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return json_error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Model is not authorized",
        );
    }
    if !inbound_endpoint_matches(model.endpoint, endpoint) {
        runtime
            .rejects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return json_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "Model is not authorized for this endpoint",
        );
    }
    let body = match prepare_request(&request, &model) {
        Ok(body) => body,
        Err(_) => {
            runtime
                .rejects
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid request",
            );
        }
    };
    let budget = deadline::Budget::new(
        runtime
            .policy
            .config
            .class_timeout_budgets
            .get(&credential.class),
        request.get("stream").and_then(Value::as_bool) == Some(true),
    );
    let client = match inference_client() {
        Ok(client) => client,
        Err(_) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "Copilot inference failed",
            );
        }
    };
    let mut retry = false;
    loop {
        let clock = match budget.start_attempt(retry) {
            Ok(clock) => clock,
            Err(category) => return timeout_response(category),
        };
        let authenticated =
            match deadline::wait(&clock, crate::copilot_auth::session_for_config(config)).await {
                Ok(Ok(session)) => session,
                Ok(Err(_)) => {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream_auth_error",
                        "Copilot authentication is unavailable",
                    );
                }
                Err(category) => return timeout_response(category),
            };
        let url = match inference_url(&authenticated.session.api_endpoint, endpoint) {
            Ok(url) => url,
            Err(_) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Copilot inference failed",
                );
            }
        };
        let upstream = match deadline::wait(
            &clock,
            send_request(&client, url, &body, &authenticated.session.bearer),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "Copilot inference failed",
                );
            }
            Err(category) => return timeout_response(category),
        };
        if upstream.status() == StatusCode::UNAUTHORIZED && !retry {
            drop(upstream);
            if let Err(category) = deadline::wait(
                &clock,
                crate::copilot_auth::invalidate_session(
                    &authenticated.oauth_token,
                    &authenticated.session.bearer,
                ),
            )
            .await
            {
                return timeout_response(category);
            }
            retry = true;
            continue;
        }
        if !upstream.status().is_success() {
            return provider_error(upstream.status());
        }
        return stream_response(upstream, class_permit, clock, endpoint);
    }
}

fn timeout_response(category: &'static str) -> Response {
    json_error(
        StatusCode::GATEWAY_TIMEOUT,
        category,
        "Copilot request deadline exceeded",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CopilotEndpoint, CopilotModel};
    use serde_json::json;

    #[test]
    fn request_rewrite_preserves_stream_and_tools_while_using_provider_model_name() {
        let model: CopilotModel = serde_yaml::from_str(
            "alias: public-haiku\nmodel: claude-haiku-5.5\nendpoint: chat_completions\n",
        )
        .unwrap();
        let request = json!({
            "model": "public-haiku",
            "stream": true,
            "tools": [{"type": "function", "function": {"name": "lookup"}}],
            "messages": [{"role": "user", "content": "test"}]
        });
        let forwarded = prepare_request(&request, &model).unwrap();
        assert_eq!(forwarded["model"], "claude-haiku-5.5");
        assert_eq!(forwarded["stream"], true);
        assert_eq!(forwarded["tools"], request["tools"]);
        assert_eq!(forwarded["messages"], request["messages"]);
    }

    #[test]
    fn configured_endpoint_maps_to_its_exact_http_path() {
        assert_eq!(
            endpoint_path(CopilotEndpoint::ChatCompletions),
            "/chat/completions"
        );
        assert_eq!(endpoint_path(CopilotEndpoint::Responses), "/responses");
    }

    #[test]
    fn copilot_requires_cloud_route_and_alias_authority() {
        let config = CopilotConfig {
            enabled: true,
            auth_source: crate::config::CopilotAuthSource::Managed,
            oauth_token_file: None,
            managed_auth_file: None,
            models: vec![CopilotModel {
                alias: "public-haiku".into(),
                model: "claude-haiku-5.5".into(),
                endpoint: CopilotEndpoint::ChatCompletions,
            }],
        };
        let mut credential = Credential {
            name: "workload".into(),
            key_file: None,
            aliases: vec!["public-haiku".into()],
            allowed_routes: vec!["copilot".into()],
            class: "interactive".into(),
            requests_per_minute: 30,
            burst: 5,
            max_in_flight: 2,
            operator: false,
            cloud_allowed: true,
            agent_affinity: false,
            expires_at: None,
        };
        assert!(eligible(&credential, &config));
        credential.allowed_routes.clear();
        assert!(!eligible(&credential, &config));
        credential.allowed_routes.push("copilot".into());
        credential.cloud_allowed = false;
        assert!(!eligible(&credential, &config));
        credential.cloud_allowed = true;
        credential.aliases.clear();
        assert!(!eligible(&credential, &config));
    }

    #[tokio::test]
    async fn response_stream_cancellation_releases_class_capacity() {
        use axum::{Router, routing::get};
        use std::convert::Infallible;

        let app = Router::new().route(
            "/stream",
            get(|| async {
                let body = async_stream::stream! {
                    yield Ok::<_, Infallible>(Bytes::from_static(b"data: synthetic\\n\\n"));
                    std::future::pending::<()>().await;
                };
                let mut response = Response::new(Body::from_stream(body));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = Client::builder().no_proxy().build().unwrap();
        let upstream = client
            .get(format!("http://{address}/stream"))
            .send()
            .await
            .unwrap();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let response = stream_response(
            upstream,
            permit,
            deadline::Budget::new(None, true)
                .start_attempt(false)
                .unwrap(),
            CopilotEndpoint::ChatCompletions,
        );
        assert_eq!(semaphore.available_permits(), 0);
        drop(response);
        assert_eq!(semaphore.available_permits(), 1);
        server.abort();
    }

    #[test]
    fn provider_errors_are_mapped_to_stable_client_statuses() {
        assert_eq!(
            provider_error(StatusCode::UNAUTHORIZED).status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            provider_error(StatusCode::TOO_MANY_REQUESTS).status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            provider_error(StatusCode::BAD_REQUEST).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn responses_websocket_is_explicitly_rejected() {
        let response = crate::unsupported_responses_websocket().await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
