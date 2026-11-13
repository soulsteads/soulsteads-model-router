use super::*;
use base64::Engine as _;

// Inline images only: never ask an upstream to fetch a caller-supplied URL.
fn validate_image_part(part: &serde_json::Map<String, Value>) -> bool {
    if part.len() != 2 {
        return false;
    }
    let Some(image) = part.get("image_url").and_then(Value::as_object) else {
        return false;
    };
    if !image.keys().all(|key| key == "url" || key == "detail")
        || image
            .get("detail")
            .is_some_and(|v| !matches!(v.as_str(), Some("auto" | "low" | "high")))
    {
        return false;
    }
    let Some(url) = image.get("url").and_then(Value::as_str) else {
        return false;
    };
    let Some((mime, encoded)) = url
        .strip_prefix("data:")
        .and_then(|data| data.split_once(";base64,"))
    else {
        return false;
    };
    if !["image/png", "image/jpeg", "image/webp"].contains(&mime)
        || encoded.is_empty()
        || encoded.len() > 1_400_000
    {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
        return false;
    }
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

pub(super) fn tokenized_prompt(
    messages: &[Value],
    kwargs: Option<&Value>,
    tokenizer: &Tokenizer,
    default_thinking: bool,
) -> Option<u64> {
    let thinking = if let Some(kwargs) = kwargs {
        let map = kwargs.as_object()?;
        if map.keys().any(|key| key != "enable_thinking") {
            return None;
        }
        map.get("enable_thinking")
            .and_then(Value::as_bool)
            .unwrap_or(default_thinking)
    } else {
        default_thinking
    };
    let mut count = 7u64 + if thinking { 40 } else { 0 };
    for message in messages {
        let role = message.get("role")?.as_str()?;
        let content = message.get("content")?;
        let plain = if let Some(text) = content.as_str() {
            text.to_string()
        } else {
            content
                .as_array()?
                .iter()
                .map(|part| part.get("text")?.as_str())
                .collect::<Option<Vec<_>>>()?
                .concat()
        };
        count = count.saturating_add(if role == "assistant" { 9 } else { 5 });
        count = count.saturating_add(tokenizer.encode(plain, false).ok()?.len() as u64);
        if let Some(reasoning) = message.get("reasoning_content") {
            match reasoning {
                Value::Null => {}
                Value::String(text) if role == "assistant" => {
                    count = count.saturating_add(
                        tokenizer.encode(text.as_str(), false).ok()?.len() as u64 + 16,
                    );
                }
                _ => return None,
            }
        }
    }
    Some(count.saturating_add(32))
}

pub(super) fn tokenized_prompt_with_tools(
    messages: &[Value],
    tools: Option<&Value>,
    tool_choice: Option<&Value>,
    kwargs: Option<&Value>,
    tokenizer: &Tokenizer,
    default_thinking: bool,
) -> Option<u64> {
    let thinking = if let Some(kwargs) = kwargs {
        let map = kwargs.as_object()?;
        if map.keys().any(|key| key != "enable_thinking") {
            return None;
        }
        map.get("enable_thinking")
            .and_then(Value::as_bool)
            .unwrap_or(default_thinking)
    } else {
        default_thinking
    };
    let encode_len = |value: &str| {
        tokenizer
            .encode(value, false)
            .ok()
            .map(|tokens| tokens.len() as u64)
    };
    let mut count = 7u64 + if thinking { 40 } else { 0 };
    for message in messages {
        let role = message.get("role")?.as_str()?;
        count = count.saturating_add(if role == "assistant" { 9 } else { 5 });
        let content = message.get("content")?;
        if let Some(text) = content.as_str() {
            count = count.saturating_add(encode_len(text)?);
        } else if let Some(parts) = content.as_array() {
            let text = parts
                .iter()
                .map(|part| part.get("text")?.as_str())
                .collect::<Option<Vec<_>>>()?
                .concat();
            count = count.saturating_add(encode_len(&text)?);
        } else if !content.is_null() {
            return None;
        }
        if let Some(reasoning) = message.get("reasoning_content") {
            match reasoning {
                Value::Null => {}
                Value::String(text) if role == "assistant" => {
                    count = count.saturating_add(encode_len(text)? + 16);
                }
                _ => return None,
            }
        }
        if let Some(calls) = message.get("tool_calls") {
            let rendered = serde_json::to_string(calls).ok()?;
            count = count.saturating_add(encode_len(&rendered)?);
            count = count.saturating_add(calls.as_array()?.len() as u64 * 32);
        }
        if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
            count = count.saturating_add(encode_len(id)? + 8);
        }
    }
    if let Some(tools) = tools {
        let rendered = serde_json::to_string(tools).ok()?;
        let number = tools.as_array()?.len() as u64;
        count = count.saturating_add(encode_len(&rendered)?);
        count = count.saturating_add((number * 96).max(512));
    } else {
        count = count.saturating_add(512);
    }
    if let Some(choice) = tool_choice {
        count = count.saturating_add(encode_len(&serde_json::to_string(choice).ok()?)? + 16);
    }
    Some(count.saturating_add(32))
}

// Offline can defer only prompt sizing; reserve parsing and known bounds still apply.
#[derive(Clone, Copy)]
pub(crate) enum PromptSizing<'a> {
    Estimate(Option<&'a Tokenizer>),
    Deferred,
}
impl<'a> PromptSizing<'a> {
    #[allow(clippy::result_large_err)]
    fn tokens(
        self,
        estimate: impl FnOnce(Option<&'a Tokenizer>) -> Result<u64, Response>,
    ) -> Result<u64, Response> {
        match self {
            Self::Estimate(tokenizer) => estimate(tokenizer),
            Self::Deferred => Ok(0),
        }
    }
}

// Keep Axum Response as the direct validation error contract.
#[allow(clippy::result_large_err)]
pub(super) fn validate_request(
    value: &Value,
    alias: &config::Alias,
    tokenizer: Option<&Tokenizer>,
    default_thinking: bool,
) -> Result<(bool, u32, u32, RequestCapabilities), Response> {
    validate_request_with_prompt_sizing(
        value,
        alias,
        PromptSizing::Estimate(tokenizer),
        default_thinking,
    )
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_request_with_prompt_sizing(
    value: &Value,
    alias: &config::Alias,
    prompt_sizing: PromptSizing<'_>,
    default_thinking: bool,
) -> Result<(bool, u32, u32, RequestCapabilities), Response> {
    let object = value.as_object().ok_or_else(|| {
        json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "JSON object required",
        )
    })?;
    const ALLOWED: &[&str] = &[
        "model",
        "messages",
        "stream",
        "stream_options",
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "stop",
        "n",
        "seed",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "user",
        "chat_template_kwargs",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "response_format",
    ];
    for (field, item) in object {
        if ALLOWED.contains(&field.as_str()) {
            continue;
        }
        if !safe_extra_name(field) || !safe_extra_value(item, 0) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported_parameter",
                &format!("Unsupported model argument: {field}"),
            ));
        }
    }
    if let Some(kwargs) = object.get("chat_template_kwargs")
        && !kwargs.as_object().is_some_and(|m| {
            m.len() <= 32
                && m.iter().all(|(k, v)| {
                    safe_extra_name(k)
                        && safe_extra_value(v, 1)
                        && (k != "enable_thinking" || v.is_boolean())
                })
        })
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "Invalid chat_template_kwargs",
        ));
    }
    let stream = object
        .get("stream")
        .map(Value::as_bool)
        .unwrap_or(Some(false))
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "stream must be boolean",
            )
        })?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "messages must be a nonempty array",
            )
        })?;
    let uses_tools = validate_tool_contract(object, messages)?;
    validate_response_format(object.get("response_format"), alias)?;
    if uses_tools
        && !alias
            .capabilities
            .iter()
            .any(|capability| capability == "tools")
    {
        return Err(tool_error("tools are unsupported for this alias"));
    }
    let mut image_count = 0u64;
    for message in messages {
        let obj = message.as_object().ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid message",
            )
        })?;
        if !obj.keys().all(|key| {
            [
                "role",
                "content",
                "name",
                "tool_calls",
                "tool_call_id",
                "reasoning_content",
            ]
            .contains(&key.as_str())
        }) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported_message_field",
                "Unsupported message field",
            ));
        }
        if !obj
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| ["system", "developer", "user", "assistant", "tool"].contains(&r))
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid message role",
            ));
        }
        if let Some(reasoning) = obj.get("reasoning_content")
            && (obj.get("role").and_then(Value::as_str) != Some("assistant")
                || !(reasoning.is_null() || reasoning.is_string()))
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "reasoning_content requires an assistant message and string or null value",
            ));
        }
        // Tool contracts were validated above. OpenAI permits omitted content
        // only for assistant messages carrying nonempty, valid tool calls.
        let content = obj
            .get("content")
            .or_else(|| {
                (obj.get("role").and_then(Value::as_str) == Some("assistant")
                    && obj
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| !calls.is_empty()))
                .then_some(&Value::Null)
            })
            .ok_or_else(|| {
                json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "missing content",
                )
            })?;
        if let Some(parts) = content.as_array() {
            if parts.is_empty() || parts.len() > 128 {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Invalid message parts",
                ));
            }
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str);
                let valid = match kind {
                    Some("text") => part.as_object().is_some_and(|p| {
                        p.len() == 2 && p.get("text").is_some_and(Value::is_string)
                    }),
                    Some("image_url")
                        if obj.get("role").and_then(Value::as_str) == Some("user") =>
                    {
                        image_count += 1;
                        part.as_object().is_some_and(validate_image_part)
                    }
                    _ => false,
                };
                if !valid || image_count > 4 {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "unsupported_modality",
                        "Only bounded inline image data and text are supported in chat messages",
                    ));
                }
            }
        } else if !content.is_string()
            && !(content.is_null()
                && obj.get("role").and_then(Value::as_str) == Some("assistant")
                && obj.contains_key("tool_calls"))
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported_modality",
                "Unsupported message content",
            ));
        }
    }
    if image_count > 0
        && !alias
            .capabilities
            .iter()
            .any(|capability| capability == "image")
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_modality",
            "Image input is unsupported for this alias",
        ));
    }
    let max = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
        .map(Value::as_u64)
        .unwrap_or(Some(4096))
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid completion reserve",
            )
        })?;
    if max == 0 || max > alias.context_limit as u64 {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            "Completion reserve exceeds alias context",
        ));
    }
    let estimated_prompt = prompt_sizing.tokens(|tokenizer| {
        let text_messages: Vec<Value> = messages
            .iter()
            .map(|message| {
                let mut cleaned = message.clone();
                if let Some(parts) = cleaned.get_mut("content").and_then(Value::as_array_mut) {
                    parts.retain(|part| {
                        part.get("type").and_then(Value::as_str) != Some("image_url")
                    });
                }
                cleaned
            })
            .collect();
        let messages_for_budget = if image_count > 0 {
            text_messages.as_slice()
        } else {
            messages.as_slice()
        };
        let fallback_prompt = serde_json::to_vec(messages_for_budget)
            .map_err(|_| {
                json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "invalid messages",
                )
            })?
            .len() as u64
            + messages.len() as u64 * 8
            + 64
            + if uses_tools {
                serde_json::to_vec(
                &json!({"tools": object.get("tools"), "tool_choice": object.get("tool_choice")}),
            )
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(0)
                + 512
            } else {
                0
            };
        Ok(if uses_tools {
            tokenizer.and_then(|t| {
                tokenized_prompt_with_tools(
                    messages_for_budget,
                    object.get("tools"),
                    object.get("tool_choice"),
                    object.get("chat_template_kwargs"),
                    t,
                    default_thinking,
                )
            })
        } else {
            tokenizer.and_then(|t| {
                tokenized_prompt(
                    messages_for_budget,
                    object.get("chat_template_kwargs"),
                    t,
                    default_thinking,
                )
            })
        }
        .unwrap_or(fallback_prompt)
        .saturating_add(image_count * 16_384))
    })?;
    if estimated_prompt.saturating_add(max) > alias.context_limit as u64 {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            "Request exceeds the alias context contract",
        ));
    }
    Ok((
        stream,
        max as u32,
        estimated_prompt.saturating_add(max) as u32,
        RequestCapabilities {
            image: image_count > 0,
            tools: uses_tools,
            json_schema: object
                .get("response_format")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("json_schema"),
            json_object: object
                .get("response_format")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("json_object"),
            responses: false,
            responses_json_schema: false,
            responses_json_object: false,
        },
    ))
}

// Keep Axum Response as the direct validation error contract.
#[allow(clippy::result_large_err)]
pub(super) fn validate_responses_tools(
    object: &serde_json::Map<String, Value>,
) -> Result<bool, Response> {
    let tools = object.get("tools");
    let mut names = std::collections::HashSet::new();
    if let Some(tools) = tools {
        let entries = tools
            .as_array()
            .filter(|items| items.len() <= 128)
            .ok_or_else(|| tool_error("tools must contain at most 128 function definitions"))?;
        if serde_json::to_vec(tools).map_or(true, |bytes| bytes.len() > 512 * 1024) {
            return Err(tool_error("tool definitions exceed the size limit"));
        }
        for entry in entries {
            let tool = entry
                .as_object()
                .ok_or_else(|| tool_error("invalid Responses function tool"))?;
            if !tool.keys().all(|key| {
                ["type", "name", "description", "parameters", "strict"].contains(&key.as_str())
            }) || tool.get("type").and_then(Value::as_str) != Some("function")
            {
                return Err(tool_error("only Responses function tools are supported"));
            }
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| valid_function_name(name))
                .ok_or_else(|| tool_error("invalid Responses function name"))?;
            if !names.insert(name) {
                return Err(tool_error("duplicate Responses function name"));
            }
            if tool
                .get("description")
                .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 8192))
                || tool
                    .get("parameters")
                    .is_some_and(|v| !v.is_object() || !local_schema_refs_only(v, 0))
                || tool.get("strict").is_some_and(|v| !v.is_boolean())
            {
                return Err(tool_error("invalid Responses function schema"));
            }
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        let valid = match choice {
            Value::String(value) => {
                ["none", "auto", "required"].contains(&value.as_str())
                    && (value == "none" || !names.is_empty())
            }
            Value::Object(value) => {
                value.len() == 2
                    && value.get("type").and_then(Value::as_str) == Some("function")
                    && value
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| names.contains(name))
            }
            _ => false,
        };
        if !valid {
            return Err(tool_error("invalid Responses tool_choice"));
        }
    }
    if object
        .get("parallel_tool_calls")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(tool_error("parallel_tool_calls must be boolean"));
    }
    Ok(!names.is_empty())
}

// Keep Axum Response as the direct validation error contract.
#[allow(clippy::result_large_err)]
pub(super) fn validate_responses_request(
    request: &Value,
    alias: &config::Alias,
    tokenizer: Option<&Tokenizer>,
    default_thinking: bool,
) -> Result<(bool, u32, bool, bool, bool), Response> {
    validate_responses_request_with_prompt_sizing(
        request,
        alias,
        PromptSizing::Estimate(tokenizer),
        default_thinking,
    )
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_responses_request_with_prompt_sizing(
    request: &Value,
    alias: &config::Alias,
    prompt_sizing: PromptSizing<'_>,
    default_thinking: bool,
) -> Result<(bool, u32, bool, bool, bool), Response> {
    let object = request.as_object().ok_or_else(|| {
        json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "JSON object required",
        )
    })?;
    const ALLOWED: &[&str] = &[
        "model",
        "input",
        "instructions",
        "max_output_tokens",
        "stream",
        "temperature",
        "top_p",
        "truncation",
        "store",
        "background",
        "previous_response_id",
        "conversation",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "text",
        "reasoning",
        "include",
        "user",
        "safety_identifier",
        "prompt_cache_key",
        "metadata",
        "service_tier",
        "chat_template_kwargs",
        "seed",
        "top_k",
        "min_p",
        "repetition_penalty",
    ];
    for (field, value) in object {
        if !ALLOWED.contains(&field.as_str())
            && (!safe_extra_name(field) || !safe_extra_value(value, 0))
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported_parameter",
                &format!("Unsupported model argument: {field}"),
            ));
        }
        if ![
            "model",
            "input",
            "instructions",
            "tools",
            "tool_choice",
            "text",
        ]
        .contains(&field.as_str())
            && !safe_extra_value(value, 0)
        {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "unsupported_parameter",
                &format!("Invalid model argument: {field}"),
            ));
        }
    }
    if object.get("previous_response_id").is_some()
        || object.get("conversation").is_some()
        || object.get("store").is_some_and(|v| v != false)
        || object.get("background").is_some_and(|v| v != false)
        || object.get("truncation").is_some_and(|v| v != "disabled")
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "Stateful or truncating Responses options are not yet supported",
        ));
    }
    if object
        .get("include")
        .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "Additional Responses output is not yet supported",
        ));
    }
    let mut needs_tools = validate_responses_tools(object)?;
    let mut needs_responses_json_schema = false;
    let mut needs_responses_json_object = false;
    if let Some(text) = object.get("text") {
        let text = text
            .as_object()
            .filter(|m| {
                m.keys().all(|k| k == "format" || k == "verbosity")
                    && m.get("verbosity").is_none_or(|v| {
                        v.as_str()
                            .is_some_and(|s| ["low", "medium", "high"].contains(&s))
                    })
            })
            .ok_or_else(|| {
                json_error(
                    StatusCode::BAD_REQUEST,
                    "unsupported_parameter",
                    "Invalid Responses text controls",
                )
            })?;
        if let Some(format) = text.get("format") {
            let format = format.as_object().ok_or_else(|| {
                json_error(
                    StatusCode::BAD_REQUEST,
                    "unsupported_parameter",
                    "Invalid Responses text format",
                )
            })?;
            match format.get("type").and_then(Value::as_str) {
                Some("text") if format.len() == 1 => {}
                Some("json_object") if format.len() == 1 => {
                    needs_responses_json_object = true;
                }
                Some("json_schema") => {
                    if !format.keys().all(|k| {
                        ["type", "name", "description", "schema", "strict"].contains(&k.as_str())
                    }) || !format
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(valid_function_name)
                        || !format
                            .get("schema")
                            .is_some_and(|v| v.is_object() && local_schema_refs_only(v, 0))
                        || format
                            .get("description")
                            .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 8192))
                        || format.get("strict").is_some_and(|v| !v.is_boolean())
                        || serde_json::to_vec(format).map_or(true, |bytes| bytes.len() > 64 * 1024)
                    {
                        return Err(json_error(
                            StatusCode::BAD_REQUEST,
                            "unsupported_parameter",
                            "Invalid Responses JSON schema",
                        ));
                    }
                    needs_responses_json_schema = true;
                }
                _ => {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "unsupported_parameter",
                        "Unsupported Responses text format",
                    ));
                }
            }
        }
    }
    let stream = object
        .get("stream")
        .map(Value::as_bool)
        .unwrap_or(Some(false))
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "stream must be boolean",
            )
        })?;
    let max = object
        .get("max_output_tokens")
        .map(Value::as_u64)
        .unwrap_or(Some(4096))
        .ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid completion reserve",
            )
        })?;
    if max == 0 || max > alias.context_limit as u64 {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            "Completion reserve exceeds alias context",
        ));
    }
    let mut messages = Vec::new();
    let mut call_ids = std::collections::HashSet::new();
    let mut result_ids = std::collections::HashSet::new();
    if let Some(instructions) = object.get("instructions") {
        let text = instructions.as_str().ok_or_else(|| {
            json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "instructions must be text",
            )
        })?;
        messages.push(json!({"role":"developer","content":text}));
    }
    match object.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role":"user","content":text})),
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= 256 => {
            for item in items {
                let object = item.as_object().ok_or_else(|| {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "unsupported_modality",
                        "Unsupported Responses input item",
                    )
                })?;
                if object.get("type").and_then(Value::as_str) == Some("function_call") {
                    if !object.keys().all(|k| {
                        [
                            "type",
                            "id",
                            "call_id",
                            "name",
                            "arguments",
                            "status",
                            "caller",
                            "namespace",
                        ]
                        .contains(&k.as_str())
                    }) {
                        return Err(tool_error("invalid Responses function call"));
                    }
                    let call_id = object
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty() && s.len() <= 128)
                        .ok_or_else(|| tool_error("invalid Responses call ID"))?;
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| valid_function_name(s))
                        .ok_or_else(|| tool_error("invalid Responses function name"))?;
                    let arguments = object
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|s| s.len() <= 512 * 1024)
                        .ok_or_else(|| tool_error("invalid Responses function arguments"))?;
                    if !call_ids.insert(call_id.to_owned())
                        || object
                            .get("id")
                            .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 128))
                        || object.get("status").is_some_and(|v| {
                            !v.as_str().is_some_and(|s| {
                                ["in_progress", "completed", "incomplete"].contains(&s)
                            })
                        })
                        || ["caller", "namespace"].iter().any(|k| {
                            object.get(*k).is_some_and(|v| {
                                !v.is_null() && !v.as_str().is_some_and(|s| s.len() <= 128)
                            })
                        })
                    {
                        return Err(tool_error("invalid Responses function call"));
                    }
                    needs_tools = true;
                    messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}]}));
                    continue;
                }
                if object.get("type").and_then(Value::as_str) == Some("function_call_output") {
                    if !object.keys().all(|k| {
                        ["type", "id", "call_id", "output", "status"].contains(&k.as_str())
                    }) {
                        return Err(tool_error("invalid Responses function result"));
                    }
                    let call_id = object
                        .get("call_id")
                        .and_then(Value::as_str)
                        .filter(|s| call_ids.contains(*s))
                        .ok_or_else(|| {
                            tool_error("Responses function result has no preceding call")
                        })?;
                    let output = object
                        .get("output")
                        .and_then(Value::as_str)
                        .filter(|s| s.len() <= 2 * 1024 * 1024)
                        .ok_or_else(|| tool_error("invalid Responses function result"))?;
                    if !result_ids.insert(call_id.to_owned())
                        || object
                            .get("id")
                            .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 128))
                        || object.get("status").is_some_and(|v| {
                            !v.as_str().is_some_and(|s| {
                                ["in_progress", "completed", "incomplete"].contains(&s)
                            })
                        })
                    {
                        return Err(tool_error("invalid Responses function result"));
                    }
                    needs_tools = true;
                    messages.push(json!({"role":"tool","content":output,"tool_call_id":call_id}));
                    continue;
                }
                if !object
                    .keys()
                    .all(|k| ["type", "role", "content", "id", "status"].contains(&k.as_str()))
                    || object.get("type").is_some_and(|v| v != "message")
                    || object
                        .get("id")
                        .is_some_and(|v| !v.as_str().is_some_and(|s| s.len() <= 128))
                    || object.get("status").is_some_and(|v| {
                        !v.as_str().is_some_and(|s| {
                            ["in_progress", "completed", "incomplete"].contains(&s)
                        })
                    })
                {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "unsupported_modality",
                        "Unsupported Responses input item",
                    ));
                }
                let role = object
                    .get("role")
                    .and_then(Value::as_str)
                    .filter(|r| ["system", "developer", "user", "assistant"].contains(r))
                    .ok_or_else(|| {
                        json_error(
                            StatusCode::BAD_REQUEST,
                            "unsupported_modality",
                            "Unsupported Responses input role",
                        )
                    })?;
                let content = object.get("content").ok_or_else(|| {
                    json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "Missing Responses content",
                    )
                })?;
                let text = if let Some(text) = content.as_str() {
                    text.to_owned()
                } else if let Some(parts) = content.as_array() {
                    let mut text = String::new();
                    for part in parts {
                        let item = part.as_object().ok_or_else(|| {
                            json_error(
                                StatusCode::BAD_REQUEST,
                                "unsupported_modality",
                                "Unsupported Responses content part",
                            )
                        })?;
                        if !item.keys().all(|k| k == "type" || k == "text")
                            || !item
                                .get("type")
                                .and_then(Value::as_str)
                                .is_some_and(|t| t == "input_text" || t == "output_text")
                        {
                            return Err(json_error(
                                StatusCode::BAD_REQUEST,
                                "unsupported_modality",
                                "Unsupported Responses content part",
                            ));
                        }
                        let value = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                            json_error(
                                StatusCode::BAD_REQUEST,
                                "unsupported_modality",
                                "Unsupported Responses content part",
                            )
                        })?;
                        text.push_str(value);
                        text.push('\n');
                    }
                    text
                } else {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "unsupported_modality",
                        "Unsupported Responses content",
                    ));
                };
                messages.push(json!({"role":role,"content":text}));
            }
        }
        _ => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Responses input must be text or text messages",
            ));
        }
    }
    if needs_tools && !alias.capabilities.iter().any(|c| c == "tools") {
        return Err(tool_error(
            "Model does not support Responses function tools",
        ));
    }
    if needs_responses_json_schema
        && !alias
            .capabilities
            .iter()
            .any(|c| c == "responses_json_schema")
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "Model does not support Responses JSON schema",
        ));
    }
    if needs_responses_json_object
        && !alias
            .capabilities
            .iter()
            .any(|c| c == "responses_json_object")
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "Model does not support Responses JSON object",
        ));
    }
    let estimated = prompt_sizing.tokens(|tokenizer| {
        let fallback = serde_json::to_vec(&messages)
            .map(|x| x.len() as u64 + 128)
            .unwrap_or(u64::MAX);
        let fallback = object
            .get("tools")
            .and_then(|tools| serde_json::to_vec(tools).ok())
            .map(|tools| fallback.saturating_add(tools.len() as u64 + 512))
            .unwrap_or(fallback);
        let format_bytes = object
            .get("text")
            .and_then(|text| text.get("format"))
            .and_then(|format| serde_json::to_vec(format).ok());
        let fallback = format_bytes
            .as_ref()
            .map(|bytes| fallback.saturating_add(bytes.len() as u64 + 512))
            .unwrap_or(fallback);
        Ok(tokenizer
            .and_then(|t| {
                tokenized_prompt_with_tools(
                    &messages,
                    object.get("tools"),
                    object.get("tool_choice"),
                    object.get("chat_template_kwargs"),
                    t,
                    default_thinking,
                )
            })
            .map(|tokens| {
                let format_tokens = format_bytes
                    .as_ref()
                    .and_then(|bytes| {
                        tokenizer.and_then(|t| {
                            t.encode(String::from_utf8_lossy(bytes).as_ref(), false)
                                .ok()
                        })
                    })
                    .map(|tokens| tokens.len() as u64 + 512)
                    .unwrap_or(0);
                tokens.saturating_add(128).saturating_add(format_tokens)
            })
            .unwrap_or(fallback))
    })?;
    let required = estimated.saturating_add(max);
    if required > alias.context_limit as u64 {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            "Request exceeds the alias context contract",
        ));
    }
    Ok((
        stream,
        required as u32,
        needs_tools,
        needs_responses_json_schema,
        needs_responses_json_object,
    ))
}
