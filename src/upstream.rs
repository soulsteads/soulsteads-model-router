use super::*;
use base64::Engine;

pub(super) const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum ResponseContract {
    Chat,
    Decision,
    Responses,
    Embeddings { count: usize, base64: bool },
}

impl ResponseContract {
    fn path(self) -> &'static str {
        match self {
            Self::Chat | Self::Decision => "chat/completions",
            Self::Responses => "responses",
            Self::Embeddings { .. } => "embeddings",
        }
    }

    fn valid(self, payload: &Value, model: &str) -> bool {
        match self {
            Self::Chat | Self::Decision => valid_chat(payload, model),
            Self::Responses => valid_responses(payload, model, true),
            Self::Embeddings { count, base64 } => valid_embeddings(payload, model, count, base64),
        }
    }
}

pub(super) enum ValidatedBody {
    Json(Value),
    Stream(super::sse::PreparedStream),
}

#[derive(Clone, Copy)]
enum UpstreamFailure {
    InvalidBody,
    Transport,
    Timeout,
    FirstOutputTimeout,
    Rejected(StatusCode),
}

impl UpstreamFailure {
    fn response(self) -> Response {
        let status = match self {
            Self::Timeout | Self::FirstOutputTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Rejected(status)
                if !status.is_server_error() && status != StatusCode::TOO_MANY_REQUESTS =>
            {
                status
            }
            _ => StatusCode::BAD_GATEWAY,
        };
        json_error(
            status,
            "upstream_error",
            "Upstream response failed validation or delivery",
        )
    }

    fn category(self) -> &'static str {
        match self {
            Self::InvalidBody => "invalid_body",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::FirstOutputTimeout => "first_output_timeout",
            Self::Rejected(_) => "status",
        }
    }
}

async fn bounded_json(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Value, UpstreamFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(UpstreamFailure::InvalidBody);
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.len() > limit - bytes.len() {
                    return Err(UpstreamFailure::InvalidBody);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                return Err(if error.is_timeout() {
                    UpstreamFailure::Timeout
                } else {
                    UpstreamFailure::Transport
                });
            }
        }
    }
    serde_json::from_slice(&bytes).map_err(|_| UpstreamFailure::InvalidBody)
}

#[expect(
    clippy::too_many_arguments,
    reason = "Preserve the existing forwarding interface for this scoped validation fix"
)]
#[expect(
    clippy::result_large_err,
    reason = "Return the existing Axum error response without changing handler contracts"
)]
pub(super) async fn forward_validated(
    runtime: &Runtime,
    credential: &CredentialState,
    alias_name: &str,
    alias: &config::Alias,
    request: &Value,
    stream: bool,
    required_context: u32,
    needs: RequestCapabilities,
    affinity: Option<&[u8; 32]>,
    started: Instant,
    contract: ResponseContract,
) -> Result<(usize, OwnedSemaphorePermit, ValidatedBody), Response> {
    let mut tried = Vec::new();
    let mut last_failure: Option<UpstreamFailure> = None;
    let policy = &runtime.policy.config.retry_policy;
    let budget = runtime
        .policy
        .config
        .class_timeout_budgets
        .get(&credential.class);
    loop {
        if policy.remaining(budget, started).is_none() {
            return Err(json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout_budget",
                "Request deadline exceeded",
            ));
        }
        if tried.len() >= policy.max_attempts {
            return Err(last_failure.unwrap().response());
        }
        if !tried.is_empty()
            && budget.is_some_and(|budget| {
                policy
                    .remaining(Some(budget), started)
                    .is_some_and(|remaining| {
                        remaining < Duration::from_secs(budget.min_retry_seconds)
                    })
            })
        {
            return Err(json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout_budget",
                "Insufficient request deadline remaining for another attempt",
            ));
        }
        let (idx, permit) = match select_route(
            runtime,
            alias_name,
            alias,
            stream,
            required_context,
            needs,
            &tried,
            affinity,
            &credential.allowed_routes,
        ) {
            Some(candidate) => candidate,
            None => {
                return Err(last_failure.map_or_else(
                    || {
                        runtime.rejects.fetch_add(1, Ordering::Relaxed);
                        json_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "no_serving_capacity",
                            "No compatible serving route has capacity",
                        )
                    },
                    UpstreamFailure::response,
                ));
            }
        };
        let candidate = &runtime.policy.routes[idx];
        let Some(timeout) =
            policy.attempt_timeout(candidate.config.timeout_seconds, budget, started)
        else {
            return Err(json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout_budget",
                "Request deadline exceeded",
            ));
        };
        let mut body = request.clone();
        body["model"] = Value::String(candidate.config.model.clone());
        if matches!(contract, ResponseContract::Embeddings { .. }) {
            body.as_object_mut().unwrap().remove("metadata");
        } else if candidate.config.disable_thinking && body.get("chat_template_kwargs").is_none() {
            body["chat_template_kwargs"] = json!({"enable_thinking":false});
        }
        let mut outbound = runtime
            .client
            .post(format!("{}/{}", candidate.config.base_url, contract.path()))
            .json(&body)
            .timeout(timeout);
        if let Some(key) = &candidate.key {
            outbound = outbound.bearer_auth(String::from_utf8_lossy(key).as_ref());
        }
        let mut http_status = None;
        let mut attempt = crate::audit::Attempt::new(runtime, idx);
        let attempt_started = tokio::time::Instant::now();
        // Recompute remaining total time after preparing the request: preparation
        // must not extend the absolute workload deadline for opted-in streams.
        let stream_hard_timeout =
            timeout.min(policy.remaining(budget, started).unwrap_or(Duration::ZERO));
        let first = if stream {
            budget.and_then(|budget| budget.first_output_seconds)
        } else {
            None
        };
        let send = async {
            if let Some(seconds) = first {
                tokio::time::timeout_at(
                    attempt_started + Duration::from_secs(seconds).min(stream_hard_timeout),
                    outbound.send(),
                )
                .await
                .map_err(|_| {
                    if stream_hard_timeout <= Duration::from_secs(seconds) {
                        UpstreamFailure::Timeout
                    } else {
                        UpstreamFailure::FirstOutputTimeout
                    }
                })?
                .map_err(|error| {
                    if error.is_timeout() {
                        UpstreamFailure::Timeout
                    } else {
                        UpstreamFailure::Transport
                    }
                })
            } else {
                outbound.send().await.map_err(|error| {
                    if error.is_timeout() {
                        UpstreamFailure::Timeout
                    } else {
                        UpstreamFailure::Transport
                    }
                })
            }
        };
        let failure = match send.await {
            Ok(response) if response.status().is_success() => {
                http_status = Some(response.status().as_u16());
                if stream {
                    if response
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.starts_with("text/event-stream"))
                    {
                        attempt.finish("stream_headers", http_status);
                        crate::audit::backend("unavailable");
                        let idle = budget.and_then(|budget| budget.stream_idle_seconds);
                        let clock = if first.is_some() || idle.is_some() {
                            Some(super::sse::ProgressClock::new(
                                attempt_started,
                                stream_hard_timeout,
                                first,
                                idle,
                            ))
                        } else {
                            None
                        };
                        return Ok((
                            idx,
                            permit,
                            ValidatedBody::Stream(super::sse::PreparedStream {
                                response,
                                clock,
                                handed_off: tokio::time::Instant::now(),
                            }),
                        ));
                    }
                    UpstreamFailure::InvalidBody
                } else {
                    let limit = if matches!(contract, ResponseContract::Decision) {
                        4096
                    } else {
                        MAX_RESPONSE_BYTES
                    };
                    match bounded_json(response, limit).await {
                        Ok(payload) if contract.valid(&payload, &candidate.config.model) => {
                            crate::audit::observe(&payload, None);
                            crate::audit::backend("complete");
                            attempt.finish("validated", http_status);
                            return Ok((idx, permit, ValidatedBody::Json(payload)));
                        }
                        Ok(_) => UpstreamFailure::InvalidBody,
                        Err(failure) => failure,
                    }
                }
            }
            Ok(response) => {
                http_status = Some(response.status().as_u16());
                UpstreamFailure::Rejected(response.status())
            }
            Err(failure) => failure,
        };
        attempt.finish(failure.category(), http_status);
        crate::audit::backend("failed");
        runtime.upstream_errors.fetch_add(1, Ordering::Relaxed);
        warn!(
            alias = crate::audit::label(alias_name),
            route = crate::audit::label(&candidate.config.name),
            class = crate::audit::label(&credential.class),
            error_category = failure.category(),
            "upstream request failed"
        );
        if matches!(failure, UpstreamFailure::Rejected(status) if !status.is_server_error() && status != StatusCode::TOO_MANY_REQUESTS)
        {
            return Err(failure.response());
        }
        if !(matches!(failure, UpstreamFailure::FirstOutputTimeout)
            || matches!(failure, UpstreamFailure::Timeout) && !stream)
        {
            mark_failure(runtime, idx);
        }
        tried.push(idx);
        drop(permit);
        last_failure = Some(failure);
    }
}

pub(super) fn valid_envelope(payload: &Value, model: &str, require_model: bool) -> bool {
    payload.is_object()
        && payload.get("error").is_none_or(Value::is_null)
        && match payload.get("model") {
            Some(value) => value.as_str() == Some(model),
            None => !require_model,
        }
}

fn valid_function(function: &Value) -> bool {
    valid_function_with_limit(function, 65536)
}

fn valid_function_with_limit(function: &Value, arguments_limit: usize) -> bool {
    function.is_object()
        && function
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(super::request_validation::valid_function_name)
        && function
            .get("arguments")
            .and_then(Value::as_str)
            .is_some_and(|args| args.len() <= arguments_limit)
}

pub(super) fn valid_chat(payload: &Value, model: &str) -> bool {
    valid_envelope(payload, model, true)
        && payload
            .get("object")
            .is_none_or(|object| object.as_str() == Some("chat.completion"))
        && payload
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                !choices.is_empty()
                    && choices.iter().all(|choice| {
                        choice.is_object()
                            && choice.get("error").is_none_or(Value::is_null)
                            && choice
                                .get("index")
                                .is_none_or(|index| index.as_u64().is_some())
                            && choice
                                .get("finish_reason")
                                .is_none_or(|reason| reason.is_null() || reason.is_string())
                            && choice.get("message").is_some_and(|message| {
                                valid_envelope(message, model, false)
                                    && message
                                        .get("role")
                                        .is_none_or(|role| role.as_str() == Some("assistant"))
                                    && message.get("content").is_none_or(|content| {
                                        content.is_null() || content.is_string()
                                    })
                                    && ["reasoning", "reasoning_content"].iter().all(|key| {
                                        message.get(key).is_none_or(|value| {
                                            value.is_null() || value.is_string()
                                        })
                                    })
                                    && message.get("tool_calls").is_none_or(|calls| {
                                        calls.as_array().is_some_and(|calls| {
                                            calls.iter().all(|call| {
                                                call.is_object()
                                                    && call
                                                        .get("id")
                                                        .and_then(Value::as_str)
                                                        .is_some_and(|id| {
                                                            !id.is_empty() && id.len() <= 128
                                                        })
                                                    && call.get("type").and_then(Value::as_str)
                                                        == Some("function")
                                                    && call.get("function").is_some_and(
                                                        |function| {
                                                            valid_function(function)
                                                                && function
                                                                    .get("name")
                                                                    .and_then(Value::as_str)
                                                                    .is_some_and(|name| {
                                                                        name.len() <= 256
                                                                    })
                                                                && function
                                                                    .get("arguments")
                                                                    .and_then(Value::as_str)
                                                                    .is_some_and(|args| {
                                                                        args.len() <= 1024 * 1024
                                                                    })
                                                        },
                                                    )
                                            })
                                        })
                                    })
                                    // Optional legacy call: null means absent, not malformed.
                                    // Non-null calls still obey the continuation contract.
                                    && message.get("function_call").is_none_or(|function| {
                                        function.is_null() || valid_function(function)
                                    })
                                    && ([
                                        "content",
                                        "tool_calls",
                                        "function_call",
                                        "reasoning",
                                        "reasoning_content",
                                    ]
                                    .iter()
                                    .any(|key| message.get(key).is_some())
                                        || choice.get("finish_reason").and_then(Value::as_str)
                                            == Some("length"))
                            })
                    })
            })
}

pub(super) fn responses_semantic_error(payload: &Value) -> bool {
    fn visit(value: &Value, depth: usize) -> bool {
        if depth > 16 {
            return true;
        }
        let Some(object) = value.as_object() else {
            return false;
        };
        object.get("error").is_some_and(|error| !error.is_null())
            || ["response", "item", "part", "message"].iter().any(|key| {
                object
                    .get(*key)
                    .is_some_and(|child| visit(child, depth + 1))
            })
            || ["output", "content"].iter().any(|key| {
                object
                    .get(*key)
                    .and_then(Value::as_array)
                    .is_some_and(|children| children.iter().any(|child| visit(child, depth + 1)))
            })
    }
    // Traverse only protocol envelope positions, never generated text or arguments.
    visit(payload, 0)
}

pub(super) fn valid_responses(payload: &Value, model: &str, require_output: bool) -> bool {
    valid_envelope(payload, model, true)
        && !responses_semantic_error(payload)
        && payload.get("object").and_then(Value::as_str) == Some("response")
        && payload
            .get("status")
            .is_some_and(|status| matches!(status.as_str(), Some("completed" | "incomplete")))
        && match payload.get("output") {
            None => !require_output,
            Some(output) => output.as_array().is_some_and(|items| {
                let mut call_ids = std::collections::HashSet::new();
                items.iter().all(|item| {
                    item.is_object()
                        && item.get("error").is_none_or(Value::is_null)
                        && match item.get("type").and_then(Value::as_str) {
                            Some("message") => {
                                item.get("error").is_none_or(Value::is_null)
                                    && item.get("content").and_then(Value::as_array).is_some_and(
                                        |content| {
                                            content.iter().all(|part| {
                                                part.is_object()
                                                    && part.get("error").is_none_or(Value::is_null)
                                                    && match part
                                                        .get("type")
                                                        .and_then(Value::as_str)
                                                    {
                                                        Some("output_text") => part
                                                            .get("text")
                                                            .is_some_and(Value::is_string),
                                                        Some("refusal") => part
                                                            .get("refusal")
                                                            .is_some_and(Value::is_string),
                                                        _ => false,
                                                    }
                                            })
                                        },
                                    )
                            }
                            Some("function_call") => {
                                valid_function_with_limit(item, 512 * 1024)
                                    && item.get("call_id").and_then(Value::as_str).is_some_and(
                                        |id| {
                                            !id.is_empty()
                                                && id.len() <= 128
                                                && call_ids.insert(id.to_owned())
                                        },
                                    )
                                    && item.get("id").is_none_or(|id| {
                                        id.as_str().is_some_and(|id| id.len() <= 128)
                                    })
                                    && item
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .is_some_and(|name| !name.is_empty() && name.len() <= 256)
                                    && item.get("arguments").is_some_and(Value::is_string)
                            }
                            Some("reasoning") => item.get("summary").is_none_or(|summary| {
                                summary.as_array().is_some_and(|parts| {
                                    parts
                                        .iter()
                                        .all(|part| part.get("text").is_some_and(Value::is_string))
                                })
                            }),
                            _ => false,
                        }
                })
            }),
        }
}

fn valid_embeddings(payload: &Value, model: &str, count: usize, base64: bool) -> bool {
    if !valid_envelope(payload, model, true)
        || !payload
            .get("object")
            .is_none_or(|object| object.as_str() == Some("list"))
    {
        return false;
    }
    let Some(data) = payload.get("data").and_then(Value::as_array) else {
        return false;
    };
    if data.len() != count {
        return false;
    }
    let mut seen = vec![false; count];
    let mut dimensions = None;
    for item in data {
        let Some(index) = item
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
        else {
            return false;
        };
        if index >= count
            || seen[index]
            || !item
                .get("object")
                .is_none_or(|object| object.as_str() == Some("embedding"))
        {
            return false;
        }
        seen[index] = true;
        let length = if base64 {
            let Some(encoded) = item.get("embedding").and_then(Value::as_str) else {
                return false;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
                return false;
            };
            if bytes.len() % 4 != 0
                || !bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|chunk| f32::from_le_bytes(*chunk).is_finite())
            {
                return false;
            }
            bytes.len() / 4
        } else {
            let Some(vector) = item.get("embedding").and_then(Value::as_array) else {
                return false;
            };
            if !vector
                .iter()
                .all(|number| number.as_f64().is_some_and(f64::is_finite))
            {
                return false;
            }
            vector.len()
        };
        if length == 0 || dimensions.is_some_and(|previous| previous != length) {
            return false;
        }
        dimensions = Some(length);
    }
    true
}
