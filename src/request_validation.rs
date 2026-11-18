use super::*;

#[derive(Clone, Copy)]
pub(crate) struct ProjectionError {
    pub code: &'static str,
    pub message: &'static str,
}
// Keep Axum Response as the concrete HTTP validation contract.
#[allow(clippy::result_large_err)]
pub(super) fn normalize_extra_body(
    value: &mut Value,
    rewrite_thinking: bool,
) -> Result<(), Response> {
    project_extra_body(value, rewrite_thinking)
        .map_err(|error| json_error(StatusCode::BAD_REQUEST, error.code, error.message))
}
// The offline inspector shares exactly the same one-level merge and errors.
pub(crate) fn project_extra_body(
    value: &mut Value,
    rewrite_thinking: bool,
) -> Result<(), ProjectionError> {
    let object = value.as_object_mut().ok_or(ProjectionError {
        code: "invalid_request",
        message: "JSON object required",
    })?;
    if let Some(extra) = object.remove("extra_body") {
        let map = extra.as_object().ok_or(ProjectionError {
            code: "unsupported_parameter",
            message: "extra_body must be an object",
        })?;
        for (key, item) in map {
            if object.contains_key(key) {
                return Err(ProjectionError {
                    code: "unsupported_parameter",
                    message: "Duplicate extra_body parameter",
                });
            }
            object.insert(key.clone(), item.clone());
        }
    }
    if rewrite_thinking && let Some(value) = object.remove("enable_thinking") {
        if object.contains_key("chat_template_kwargs") {
            return Err(ProjectionError {
                code: "unsupported_parameter",
                message: "Duplicate thinking parameter",
            });
        }
        object.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking":value}),
        );
    }
    Ok(())
}

pub(super) fn safe_extra_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    const DENIED: &[&str] = &[
        "api_base",
        "base_url",
        "url",
        "upstream_url",
        "endpoint",
        "provider",
        "backend",
        "route",
        "api_key",
        "authorization",
        "headers",
        "credential",
        "credentials",
        "model",
        "messages",
        "tools",
        "tool_choice",
        "functions",
        "function_call",
        "response_format",
        "modalities",
        "audio",
        "image",
        "image_url",
        "video",
        "input",
        "metadata",
        "prompt",
        "decision",
        "web_search_options",
        "parallel_tool_calls",
        "json_schema",
        "grammar",
    ];
    !DENIED.contains(&name)
        && ![
            "image_",
            "audio_",
            "video_",
            "document_",
            "tool_",
            "guided_",
            "upstream_",
            "provider_",
            "backend_",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}
pub(super) fn safe_extra_value(value: &Value, depth: u8) -> bool {
    if depth > 3 {
        return false;
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(s) => s.len() <= 1024 && !s.contains("://") && !s.contains("Bearer "),
        Value::Array(items) => {
            items.len() <= 128 && items.iter().all(|v| safe_extra_value(v, depth + 1))
        }
        Value::Object(items) => {
            items.len() <= 32
                && items
                    .iter()
                    .all(|(k, v)| safe_extra_name(k) && safe_extra_value(v, depth + 1))
        }
    }
}

pub(super) fn tool_error(message: &str) -> Response {
    json_error(StatusCode::BAD_REQUEST, "invalid_tools", message)
}

pub(super) fn valid_function_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub(super) fn local_schema_refs_only(value: &Value, depth: u8) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Value::Object(map) => map.iter().all(|(key, child)| {
            if ["$ref", "$dynamicRef", "$recursiveRef"].contains(&key.as_str()) {
                child
                    .as_str()
                    .is_some_and(|reference| reference.starts_with('#'))
            } else {
                local_schema_refs_only(child, depth + 1)
            }
        }),
        Value::Array(items) => items
            .iter()
            .all(|child| local_schema_refs_only(child, depth + 1)),
        _ => true,
    }
}

// Keep Axum Response as the direct validation error contract.
#[allow(clippy::result_large_err)]
pub(super) fn validate_tool_contract(
    object: &serde_json::Map<String, Value>,
    messages: &[Value],
) -> Result<bool, Response> {
    let tools = object.get("tools");
    let mut names = std::collections::HashSet::new();
    if let Some(tools) = tools {
        let entries = tools
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 128)
            .ok_or_else(|| tool_error("tools must contain 1..128 function definitions"))?;
        if serde_json::to_vec(tools).map_or(true, |bytes| bytes.len() > 512 * 1024) {
            return Err(tool_error("tool definitions exceed the size limit"));
        }
        for entry in entries {
            let tool = entry
                .as_object()
                .ok_or_else(|| tool_error("invalid function tool"))?;
            if tool.len() != 2 || tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(tool_error("only function tools are supported"));
            }
            let function = tool
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| tool_error("function definition required"))?;
            if !function
                .keys()
                .all(|key| ["name", "description", "parameters", "strict"].contains(&key.as_str()))
            {
                return Err(tool_error("unsupported function definition field"));
            }
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| valid_function_name(name))
                .ok_or_else(|| tool_error("invalid function name"))?;
            if !names.insert(name) {
                return Err(tool_error("duplicate function name"));
            }
            if function
                .get("description")
                .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 8192))
                || function.get("parameters").is_some_and(|v| !v.is_object())
                || function.get("strict").is_some_and(|v| !v.is_boolean())
            {
                return Err(tool_error("invalid function schema"));
            }
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        let valid = match choice {
            Value::String(value) => ["none", "auto", "required"].contains(&value.as_str()),
            Value::Object(value) => {
                value.len() == 2
                    && value.get("type").and_then(Value::as_str) == Some("function")
                    && value
                        .get("function")
                        .and_then(Value::as_object)
                        .is_some_and(|function| {
                            function.len() == 1
                                && function
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .is_some_and(|name| names.contains(name))
                        })
            }
            _ => false,
        };
        if !valid || (tools.is_none() && choice != "none") {
            return Err(tool_error("invalid tool_choice"));
        }
    }
    if object
        .get("parallel_tool_calls")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(tool_error("parallel_tool_calls must be boolean"));
    }
    let mut uses_tools = tools.is_some();
    for message in messages {
        let msg = message
            .as_object()
            .ok_or_else(|| tool_error("invalid message"))?;
        if msg.get("role").and_then(Value::as_str) == Some("tool") {
            uses_tools = true;
            if !msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s.len() <= 128)
                || !msg.get("content").is_some_and(Value::is_string)
            {
                return Err(tool_error("invalid tool result message"));
            }
        }
        if let Some(calls) = msg.get("tool_calls") {
            uses_tools = true;
            let calls = calls
                .as_array()
                .filter(|v| !v.is_empty() && v.len() <= 128)
                .ok_or_else(|| tool_error("invalid assistant tool calls"))?;
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                return Err(tool_error("tool_calls require assistant role"));
            }
            for call in calls {
                let call = call
                    .as_object()
                    .ok_or_else(|| tool_error("invalid assistant tool call"))?;
                if call.len() != 3
                    || call.get("type").and_then(Value::as_str) != Some("function")
                    || !call
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty() && s.len() <= 128)
                {
                    return Err(tool_error("invalid assistant tool call"));
                }
                let function = call
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(|| tool_error("invalid assistant tool call function"))?;
                if function.len() != 2
                    || !function
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(valid_function_name)
                    || !function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.len() <= 65536)
                {
                    return Err(tool_error("invalid assistant tool call function"));
                }
            }
        }
    }
    Ok(uses_tools)
}

// Keep Axum Response as the direct validation error contract.
#[allow(clippy::result_large_err)]
pub(super) fn validate_response_format(
    value: Option<&Value>,
    alias: &config::Alias,
) -> Result<(), Response> {
    let Some(value) = value else {
        return Ok(());
    };
    let format = value.as_object().ok_or_else(|| {
        json_error(
            StatusCode::BAD_REQUEST,
            "invalid_response_format",
            "response_format must be an object",
        )
    })?;
    if format.get("type").and_then(Value::as_str) == Some("json_object") {
        if format.len() != 1 {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_response_format",
                "json_object accepts only a type field",
            ));
        }
        if !alias
            .capabilities
            .iter()
            .any(|capability| capability == "json_object")
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported_parameter",
                "JSON object output is unsupported for this alias",
            ));
        }
        return Ok(());
    }
    if !alias
        .capabilities
        .iter()
        .any(|capability| capability == "json_schema")
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "JSON schema output is unsupported for this alias",
        ));
    }
    if serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 64 * 1024) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_response_format",
            "Response schema exceeds the size limit",
        ));
    }
    if format.len() != 2 || format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_response_format",
            "Only verified json_schema response_format is supported",
        ));
    }
    let definition = format
        .get("json_schema")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_response_format",
                "JSON schema definition required",
            )
        })?;
    if !definition
        .keys()
        .all(|key| ["name", "schema", "strict", "description"].contains(&key.as_str()))
        || !definition
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(valid_function_name)
        || !definition
            .get("schema")
            .is_some_and(|schema| schema.is_object() && local_schema_refs_only(schema, 0))
        || definition.get("strict").is_some_and(|v| !v.is_boolean())
        || definition
            .get("description")
            .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 2048))
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_response_format",
            "Invalid JSON schema response_format",
        ));
    }
    Ok(())
}
