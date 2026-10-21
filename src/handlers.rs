use super::*;

pub(super) async fn chat(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    runtime.requests.fetch_add(1, Ordering::Relaxed);
    let credential = match authorize(&runtime, &headers).await {
        Ok(x) => x,
        Err(e) => {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return e;
        }
    };
    let class = credential.class.clone();
    let matcher = matcher_id(&runtime, &credential, &headers);
    let class_permit = match credential.class_semaphore.clone().try_acquire_owned() {
        Ok(x) => x,
        Err(_) => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "class_capacity",
                "Workload capacity exceeded",
            );
        }
    };
    if body.len() > 8 * 1024 * 1024 {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Request body too large",
        );
    }
    let mut request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_json", "Invalid JSON"),
    };
    let alias_name = match request.get("model").and_then(Value::as_str) {
        Some(x) => x.to_owned(),
        None => return json_error(StatusCode::BAD_REQUEST, "invalid_model", "Missing model"),
    };
    if !credential.aliases.contains(&alias_name) {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return json_error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Model is not authorized",
        );
    }
    let alias = match runtime.policy.config.aliases.get(&alias_name) {
        Some(x) => x,
        None => {
            return json_error(
                StatusCode::NOT_FOUND,
                "model_not_found",
                "Model unavailable",
            );
        }
    };
    if alias.endpoint != Endpoint::Chat {
        return json_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "Model requires /v1/embeddings",
        );
    }
    if let Err(error) = normalize_extra_body(&mut request, true) {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return error;
    }
    let default_thinking = alias
        .routes
        .iter()
        .filter(|entry| entry.enabled)
        .filter_map(|entry| {
            runtime
                .policy
                .routes
                .iter()
                .find(|r| r.config.name == entry.route)
        })
        .any(|route| route.config.enabled && !route.config.disable_thinking);
    let tokenizer = runtime
        .policy
        .tokenizers
        .get(&alias_name)
        .map(|t| t.as_ref());
    let (stream, _reserve, required_context, needs) =
        match validate_request(&request, alias, tokenizer, default_thinking) {
            Ok(x) => x,
            Err(e) => {
                runtime.rejects.fetch_add(1, Ordering::Relaxed);
                return e;
            }
        };
    if needs.image
        && !alias.routes.iter().any(|entry| {
            entry.enabled
                && (credential.allowed_routes.is_empty()
                    || credential.allowed_routes.contains(&entry.route))
                && runtime.policy.routes.iter().any(|route| {
                    route.config.enabled
                        && route.config.name == entry.route
                        && route.config.capabilities.iter().any(|c| c == "image")
                })
        })
    {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_modality",
            "No authorized image-capable route for this model",
        );
    }
    let affinity_key = match affinity_key(&headers, &credential, &alias_name) {
        Ok(key) => key,
        Err(error) => {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return error;
        }
    };
    let started = Instant::now();
    let (route_idx, route_permit, prepared) = match forward_validated(
        &runtime,
        &credential,
        &alias_name,
        alias,
        &request,
        stream,
        required_context,
        needs,
        affinity_key.as_ref(),
        started,
        ResponseContract::Chat,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return error,
    };
    info!(alias=crate::audit::label(&alias_name),route=crate::audit::label(&runtime.policy.routes[route_idx].config.name),class=crate::audit::label(&class),matcher=matcher.as_deref().map(crate::audit::label),latency_ms=%started.elapsed().as_millis(),"request routed");
    let response = match prepared {
        ValidatedBody::Json(mut payload) => {
            payload["model"] = Value::String(alias_name);
            runtime.route_completions[route_idx].fetch_add(1, Ordering::Relaxed);
            mark_success(&runtime, route_idx);
            drop((route_permit, class_permit));
            return normalized_json(payload);
        }
        ValidatedBody::Stream(response) => response,
    };
    let mut result = Response::new(Body::empty());
    result.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    result
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    result
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    // Both permits remain owned by the SSE stream. Dropping it on disconnect releases them.
    *result.body_mut() = validated_sse(
        runtime,
        route_idx,
        route_permit,
        class_permit,
        response,
        alias_name,
        false,
    );
    result
}

pub(super) async fn responses(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    runtime.requests.fetch_add(1, Ordering::Relaxed);
    let credential = match authorize(&runtime, &headers).await {
        Ok(value) => value,
        Err(error) => {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return error;
        }
    };
    let class_permit = match credential.class_semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "class_capacity",
                "Workload capacity exceeded",
            );
        }
    };
    if body.len() > 8 * 1024 * 1024 {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Request body too large",
        );
    }
    let mut request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_json", "Invalid JSON"),
    };
    let alias_name = match request.get("model").and_then(Value::as_str) {
        Some(value) => value.to_owned(),
        None => return json_error(StatusCode::BAD_REQUEST, "invalid_model", "Missing model"),
    };
    if !credential.aliases.contains(&alias_name) {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return json_error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Model is not authorized",
        );
    }
    let Some(alias) = runtime.policy.config.aliases.get(&alias_name) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "Model unavailable",
        );
    };
    if alias.endpoint != Endpoint::Chat || !alias.capabilities.iter().any(|c| c == "responses") {
        return json_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "Model does not support /v1/responses",
        );
    }
    if let Err(error) = normalize_extra_body(&mut request, true) {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return error;
    }
    let default_thinking = alias
        .routes
        .iter()
        .filter(|entry| entry.enabled)
        .filter_map(|entry| {
            runtime
                .policy
                .routes
                .iter()
                .find(|route| route.config.name == entry.route)
        })
        .any(|route| route.config.enabled && !route.config.disable_thinking);
    let tokenizer = runtime
        .policy
        .tokenizers
        .get(&alias_name)
        .map(|t| t.as_ref());
    let (
        stream,
        required_context,
        needs_tools,
        needs_responses_json_schema,
        needs_responses_json_object,
    ) = match validate_responses_request(&request, alias, tokenizer, default_thinking) {
        Ok(value) => value,
        Err(error) => {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return error;
        }
    };
    let affinity_key = match affinity_key(&headers, &credential, &alias_name) {
        Ok(value) => value,
        Err(error) => {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return error;
        }
    };
    let started = Instant::now();
    let (route_idx, route_permit, prepared) = match forward_validated(
        &runtime,
        &credential,
        &alias_name,
        alias,
        &request,
        stream,
        required_context,
        RequestCapabilities {
            tools: needs_tools,
            responses: true,
            responses_json_schema: needs_responses_json_schema,
            responses_json_object: needs_responses_json_object,
            ..Default::default()
        },
        affinity_key.as_ref(),
        started,
        ResponseContract::Responses,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return error,
    };
    info!(alias=crate::audit::label(&alias_name),route=crate::audit::label(&runtime.policy.routes[route_idx].config.name),class=crate::audit::label(&credential.class),endpoint="responses",latency_ms=%started.elapsed().as_millis(),"request routed");
    let response = match prepared {
        ValidatedBody::Json(mut payload) => {
            payload["model"] = Value::String(alias_name);
            runtime.route_completions[route_idx].fetch_add(1, Ordering::Relaxed);
            mark_success(&runtime, route_idx);
            drop((route_permit, class_permit));
            return normalized_json(payload);
        }
        ValidatedBody::Stream(response) => response,
    };
    let mut result = Response::new(Body::empty());
    result.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    result
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    result
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    *result.body_mut() = validated_sse(
        runtime,
        route_idx,
        route_permit,
        class_permit,
        response,
        alias_name,
        true,
    );
    result
}

pub(super) async fn embeddings(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    runtime.requests.fetch_add(1, Ordering::Relaxed);
    let credential = match authorize(&runtime, &headers).await {
        Ok(x) => x,
        Err(e) => {
            runtime.rejects.fetch_add(1, Ordering::Relaxed);
            return e;
        }
    };
    let class_permit = match credential.class_semaphore.clone().try_acquire_owned() {
        Ok(x) => x,
        Err(_) => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "class_capacity",
                "Workload capacity exceeded",
            );
        }
    };
    let mut request: Value = match serde_json::from_slice(&body) {
        Ok(x) => x,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_json", "Invalid JSON"),
    };
    if let Err(error) = normalize_extra_body(&mut request, false) {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return error;
    }
    let Some(object) = request.as_object() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "JSON object required",
        );
    };
    if !object.iter().all(|(k, v)| {
        ["model", "input", "encoding_format"].contains(&k.as_str())
            || k == "metadata" && v.is_object() && safe_extra_value(v, 0)
            || safe_extra_name(k) && safe_extra_value(v, 0)
    }) || object
        .get("encoding_format")
        .is_some_and(|v| !matches!(v.as_str(), Some("float" | "base64")))
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "Unsupported embedding parameter",
        );
    }
    let Some(alias_name) = request.get("model").and_then(Value::as_str) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_model", "Missing model");
    };
    if !credential.aliases.iter().any(|a| a == alias_name) {
        return json_error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Model is not authorized",
        );
    }
    let Some(alias) = runtime.policy.config.aliases.get(alias_name) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "Model unavailable",
        );
    };
    if alias.endpoint != Endpoint::Embedding {
        return json_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "Model requires /v1/chat/completions",
        );
    }
    let Some(input) = request.get("input") else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_request", "Missing input");
    };
    let inputs: Vec<&str> = if let Some(s) = input.as_str() {
        vec![s]
    } else if let Some(a) = input.as_array() {
        if a.is_empty() || a.len() > 128 {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid input array",
            );
        }
        match a.iter().map(Value::as_str).collect::<Option<Vec<_>>>() {
            Some(v) => v,
            None => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "unsupported_modality",
                    "Embedding input must be text",
                );
            }
        }
    } else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_modality",
            "Embedding input must be text",
        );
    };
    let needed = embedding_batch_context_tokens(&inputs);
    if needed > alias.context_limit as u64 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            "Embedding input exceeds alias context",
        );
    }
    let started = Instant::now();
    let (idx, permit, prepared) = match forward_validated(
        &runtime,
        &credential,
        alias_name,
        alias,
        &request,
        false,
        needed.min(u32::MAX as u64) as u32,
        RequestCapabilities::default(),
        None,
        started,
        ResponseContract::Embeddings {
            count: inputs.len(),
            base64: request.get("encoding_format").and_then(Value::as_str) == Some("base64"),
        },
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return error,
    };
    let ValidatedBody::Json(mut payload) = prepared else {
        unreachable!()
    };
    payload["model"] = Value::String(alias_name.into());
    runtime.route_completions[idx].fetch_add(1, Ordering::Relaxed);
    mark_success(&runtime, idx);
    info!(alias=crate::audit::label(alias_name),route=crate::audit::label(&runtime.policy.routes[idx].config.name),class=crate::audit::label(&credential.class),latency_ms=%started.elapsed().as_millis(),"embedding routed");
    drop((permit, class_permit));
    normalized_json(payload)
}

fn normalized_json(payload: Value) -> Response {
    let mut result = Response::new(Body::from(payload.to_string()));
    result.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    result
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    result
}
