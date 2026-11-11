//! Authenticated alias-scoped passthrough. JSON buffering and model translation
//! are necessary for alias authorization; response bytes are not interpreted.
use super::*;
use axum::{extract::Request, http::Method};
use std::{collections::HashSet, sync::OnceLock};

fn transport_headers(headers: &HeaderMap) -> HeaderMap {
    let mut blocked: HashSet<String> = [
        "authorization",
        "cookie",
        "set-cookie",
        "proxy-authorization",
        "host",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for value in headers.get_all("connection") {
        if let Ok(value) = value.to_str() {
            blocked.extend(value.split(',').map(|s| s.trim().to_ascii_lowercase()));
        }
    }
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            !blocked.contains(name)
                && !name.contains("api-key")
                && !name.contains("apikey")
                && !name.contains("token")
                && !name.contains("secret")
                && !name.contains("credential")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

pub(crate) async fn forward(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    method: Method,
    headers: &HeaderMap,
    body: Vec<u8>,
    credential: Option<&str>,
) -> Response {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let mut clean = transport_headers(headers);
    clean.remove(header::CONTENT_LENGTH);
    let mut builder = client.request(method, url).headers(clean).body(body);
    if let Some(secret) = credential {
        builder = builder.bearer_auth(secret);
    }
    let target = crate::audit::passthrough_target();
    let mut attempt = target
        .as_ref()
        .map(|(runtime, idx)| crate::audit::Attempt::new(runtime, *idx));
    let upstream = match builder.send().await {
        Ok(response) => response,
        Err(_) => {
            if let Some(attempt) = &mut attempt {
                attempt.finish("transport", None);
            }
            return json_error(
                StatusCode::BAD_GATEWAY,
                "upstream_transport",
                "Upstream transport failed",
            );
        }
    };
    let status = upstream.status();
    if let Some(attempt) = &mut attempt {
        attempt.finish(
            if status.is_success() {
                "passthrough_headers"
            } else {
                "status"
            },
            Some(status.as_u16()),
        );
    }
    let clean = transport_headers(upstream.headers());
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = clean;
    response
}

pub(crate) async fn dispatch(runtime: Arc<Runtime>, req: Request, endpoint: &str) -> Response {
    crate::audit::bind(&runtime);
    // Authenticate before collecting a potentially unbounded body. Do not call
    // authorize(): that includes the managed-path token-bucket admission policy.
    let headers = req.headers().clone();
    let credential = match authenticate(&runtime, &headers).await {
        Ok(Some(c)) => c,
        Ok(None) => return unauthorized(),
        Err(e) => return e,
    };
    if credential.expires_at.is_some_and(|expiry| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|d| d.as_secs() as i64 >= expiry)
    }) {
        return unauthorized();
    }
    crate::audit::category(&credential.class);
    let may_passthrough = credential.aliases.iter().any(|name| {
        runtime
            .policy
            .config
            .aliases
            .get(name)
            .is_some_and(|a| a.passthrough_route.is_some())
    });
    let max = if may_passthrough {
        usize::MAX
    } else {
        8 * 1024 * 1024
    };
    let body = match axum::body::to_bytes(req.into_body(), max).await {
        Ok(body) => body,
        Err(_) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Cannot read request body",
            );
        }
    };
    let mut value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_json", "Invalid JSON"),
    };
    let Some(name) = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_model", "Missing model");
    };
    if !credential.aliases.contains(&name) {
        return json_error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Model is not authorized",
        );
    }
    if let Some(model) = runtime
        .policy
        .config
        .copilot
        .model_for_alias(&name)
        .cloned()
    {
        if !copilot_endpoint_allowed(endpoint, model.endpoint) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "wrong_endpoint",
                "Model is not authorized for this endpoint",
            );
        }
        if !copilot::eligible(&credential.config, &runtime.policy.config.copilot) {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return json_error(
                StatusCode::FORBIDDEN,
                "cloud_not_allowed",
                "Copilot requires explicit workload, model and route permission",
            );
        }
        if !rate_check(&credential) {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                "Rate limit exceeded",
            );
        }
        let class_permit = match credential.class_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "class_capacity_exhausted",
                    "Too many requests",
                );
            }
        };
        return copilot::forward(
            runtime,
            credential,
            headers,
            value,
            model.endpoint,
            class_permit,
        )
        .await;
    }
    if runtime.policy.config.codex.alias.as_ref() == Some(&name) {
        return match endpoint {
            "/chat/completions" => handlers::chat(State(runtime), headers, body).await,
            "/responses" => handlers::responses(State(runtime), headers, body).await,
            _ => json_error(
                StatusCode::BAD_REQUEST,
                "wrong_endpoint",
                "Codex does not support this endpoint",
            ),
        };
    }
    let Some(alias) = runtime.policy.config.aliases.get(&name) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "Model unavailable",
        );
    };
    if let Some(route_name) = &alias.passthrough_route {
        if (endpoint == "/embeddings") != (alias.endpoint == Endpoint::Embedding) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "wrong_endpoint",
                "Model is not authorized for this endpoint",
            );
        }
        let Some((idx, route)) = runtime
            .policy
            .routes
            .iter()
            .enumerate()
            .find(|(_, r)| &r.config.name == route_name)
        else {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "route_unavailable",
                "Route unavailable",
            );
        };
        if !route.config.enabled
            || (!credential.allowed_routes.is_empty()
                && !credential.allowed_routes.contains(route_name))
        {
            return json_error(
                StatusCode::FORBIDDEN,
                "route_not_allowed",
                "Route is not authorized",
            );
        }
        value["model"] = Value::String(route.config.model.clone());
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let client = CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .retry(reqwest::retry::never())
                .build()
                .expect("passthrough HTTP client")
        });
        runtime.requests.fetch_add(1, Ordering::Relaxed);
        runtime.route_attempts[idx].fetch_add(1, Ordering::Relaxed);
        crate::audit::passthrough(&runtime, idx);
        let key = route
            .key
            .as_deref()
            .and_then(|k| std::str::from_utf8(k).ok());
        return forward(
            client,
            &route.config.base_url,
            endpoint,
            Method::POST,
            &headers,
            serde_json::to_vec(&value).expect("JSON value serialization"),
            key,
        )
        .await;
    }
    if body.len() > 8 * 1024 * 1024 {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Request body too large",
        );
    }
    match endpoint {
        "/embeddings" => embeddings(State(runtime), headers, body).await,
        "/responses" => responses(State(runtime), headers, body).await,
        _ => chat(State(runtime), headers, body).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use tokio::net::TcpListener;
    #[tokio::test]
    async fn forwards_unmodified_response_status_type_and_bytes_without_credentials_leaking() {
        let upstream = Router::new().route(
            "/v1/embeddings",
            post(|headers: HeaderMap, body: Bytes| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer configured");
                assert!(headers.get("cookie").is_none());
                assert!(headers.get("x-secret").is_none());
                assert_eq!(body.as_ref(), b"{not-json}");
                Response::builder()
                    .status(422)
                    .header("content-type", "application/x-upstream")
                    .body(Body::from(vec![0u8, 255, 13]))
                    .unwrap()
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer caller"));
        headers.insert("cookie", HeaderValue::from_static("sid=secret"));
        headers.insert("x-secret", HeaderValue::from_static("secret"));
        let response = forward(
            &client,
            &format!("http://{addr}"),
            "/v1/embeddings",
            Method::POST,
            &headers,
            b"{not-json}".to_vec(),
            Some("configured"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(response.headers()["content-type"], "application/x-upstream");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
            &[0, 255, 13][..]
        );
        task.abort();
    }
}

// Keep native HTTP dispatch authorization aligned with the configured endpoint.
fn copilot_endpoint_allowed(endpoint: &str, configured: crate::config::CopilotEndpoint) -> bool {
    endpoint == copilot::endpoint_path(configured)
}

#[cfg(test)]
#[path = "copilot_dispatch_tests.rs"]
mod rate_regressions;

#[cfg(test)]
mod copilot_dispatch_tests {
    use super::copilot_endpoint_allowed;
    use crate::config::CopilotEndpoint;

    #[test]
    fn copilot_chat_dispatch_accepts_configured_chat_endpoint() {
        assert!(copilot_endpoint_allowed(
            "/chat/completions",
            CopilotEndpoint::ChatCompletions
        ));
    }

    #[test]
    fn copilot_responses_dispatch_accepts_configured_responses_endpoint() {
        assert!(copilot_endpoint_allowed(
            "/responses",
            CopilotEndpoint::Responses
        ));
    }

    #[test]
    fn copilot_dispatch_rejects_cross_protocol_and_other_endpoints() {
        assert!(!copilot_endpoint_allowed(
            "/responses",
            CopilotEndpoint::ChatCompletions
        ));
        assert!(!copilot_endpoint_allowed(
            "/chat/completions",
            CopilotEndpoint::Responses
        ));
        assert!(!copilot_endpoint_allowed(
            "/embeddings",
            CopilotEndpoint::ChatCompletions
        ));
    }
}
