use super::*;
use futures_util::Stream;
use std::{collections::HashMap, pin::Pin};

const PROVIDER_ERROR: &str = "Codex provider failed or returned an invalid response";
const MAX_STREAM: usize = 16 * 1024 * 1024;
type Events = Pin<Box<dyn Stream<Item = Result<Value, &'static str>> + Send>>;

pub(crate) fn eligible(credential: &config::Credential, config: &config::CodexConfig) -> bool {
    credential.can_use_codex(config)
        && config
            .alias
            .as_ref()
            .is_some_and(|alias| credential.aliases.contains(alias))
        && credential
            .allowed_routes
            .iter()
            .any(|route| route == "codex")
}

fn alias() -> config::Alias {
    config::Alias {
        passthrough_route: None,
        context_limit: 131072,
        tokenizer: None,
        endpoint: Endpoint::Chat,
        routes: Vec::new(),
        capabilities: vec!["text".into(), "tools".into(), "responses".into()],
        streaming: true,
    }
}

fn invalid() -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "unsupported_parameter",
        "Unsupported or invalid Codex request; see docs/CODEX.md",
    )
}

#[allow(clippy::result_large_err)]
pub(crate) fn chat_request(request: &Value) -> Result<Value, Response> {
    let object = request.as_object().ok_or_else(invalid)?;
    if object.keys().any(|key| {
        ![
            "model",
            "messages",
            "tools",
            "tool_choice",
            "stream",
            "stream_options",
            "reasoning_effort",
        ]
        .contains(&key.as_str())
    }) {
        return Err(invalid());
    }
    if object.get("stream_options").is_some_and(|options| {
        !options.as_object().is_some_and(|map| {
            map.keys().all(|key| key == "include_usage")
                && map.get("include_usage").is_none_or(Value::is_boolean)
        })
    }) {
        return Err(invalid());
    }
    let mut validated = request.clone();
    validated
        .as_object_mut()
        .unwrap()
        .remove("reasoning_effort");
    validate_request(&validated, &alias(), None, false)?;
    let mut input = Vec::new();
    for message in request["messages"].as_array().ok_or_else(invalid)? {
        let role = message["role"].as_str().ok_or_else(invalid)?;
        if message.get("name").is_some() {
            return Err(invalid());
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":message["content"]}));
            continue;
        }
        if let Some(reasoning) = message
            .get("reasoning_content")
            .filter(|value| !value.is_null())
        {
            input.push(
                json!({"type":"reasoning","summary":[{"type":"summary_text","text":reasoning}]}),
            );
        }
        if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
            let parts = if let Some(text) = content.as_str() {
                vec![
                    json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":text}),
                ]
            } else {
                content.as_array().ok_or_else(invalid)?.iter().map(|part| {
                    if part["type"] != "text" || !part["text"].is_string() { return Err(invalid()); }
                    Ok(json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":part["text"]}))
                }).collect::<Result<Vec<_>, _>>()?
            };
            input.push(json!({"type":"message","role":role,"content":parts}));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
            }
        }
    }
    let mut body = json!({"model":request["model"],"input":input,"stream":request.get("stream").cloned().unwrap_or(json!(false))});
    if let Some(tools) = request.get("tools") {
        body["tools"] = Value::Array(
            tools
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(|tool| {
                    let mut function = tool["function"].clone();
                    function["type"] = json!("function");
                    function
                })
                .collect(),
        );
    }
    if let Some(choice) = request.get("tool_choice") {
        body["tool_choice"] = if choice.is_object() {
            json!({"type":"function","name":choice["function"]["name"]})
        } else {
            choice.clone()
        };
    }
    if let Some(effort) = request.get("reasoning_effort") {
        if !effort
            .as_str()
            .is_some_and(|value| ["minimal", "low", "medium", "high", "xhigh"].contains(&value))
        {
            return Err(invalid());
        }
        body["reasoning"] = json!({"effort":effort});
    }
    Ok(body)
}

#[allow(clippy::result_large_err)]
// The real Codex backend can omit Content-Type while sending valid SSE.
// Missing MIME is accepted only at the header gate; events() still enforces
// bounded SSE/JSON framing and a valid terminal response before success.
fn upstream_sse_header_valid(headers: &HeaderMap) -> bool {
    match headers.get(header::CONTENT_TYPE) {
        None => true,
        Some(value) => value.to_str().ok().is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        }),
    }
}

fn native_name(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|name| {
        !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
    })
}

fn native_tools_valid(tools: &[Value], depth: usize) -> bool {
    if depth > 4 || tools.len() > 128 {
        return false;
    }
    tools.iter().all(|tool| {
        let Some(object) = tool.as_object() else {
            return false;
        };
        if object
            .get("description")
            .is_some_and(|value| !value.is_string())
        {
            return false;
        }
        match object.get("type").and_then(Value::as_str) {
            Some("function") => {
                native_name(object.get("name"))
                    && object.get("parameters").is_none_or(Value::is_object)
                    && object.get("strict").is_none_or(Value::is_boolean)
            }
            Some("namespace") => {
                native_name(object.get("name"))
                    && object
                        .get("tools")
                        .and_then(Value::as_array)
                        .is_some_and(|nested| native_tools_valid(nested, depth + 1))
            }
            Some("custom") => {
                native_name(object.get("name"))
                    && object.get("format").is_none_or(|format| {
                        format.as_object().is_some_and(|format| {
                            match format.get("type").and_then(Value::as_str) {
                                Some("text") => true,
                                Some("grammar") => {
                                    format
                                        .get("syntax")
                                        .and_then(Value::as_str)
                                        .is_some_and(|syntax| ["lark", "regex"].contains(&syntax))
                                        && format
                                            .get("definition")
                                            .and_then(Value::as_str)
                                            .is_some_and(|text| !text.is_empty())
                                }
                                _ => false,
                            }
                        })
                    })
            }
            Some("web_search" | "web_search_preview") => true,
            _ => false,
        }
    })
}

// Validate native-only fields separately. The local-model validator receives a
// projection; the original request, including namespace/custom items, is forwarded.
// Match the existing HTTP validators: return the prepared error response.
#[allow(clippy::result_large_err)]
fn native_validation_projection(validation: &mut Value) -> Result<(), Response> {
    if validation
        .get("client_metadata")
        .is_some_and(|value| !value.is_object())
        || validation
            .get("parallel_tool_calls")
            .is_some_and(|value| !value.is_boolean())
        || validation
            .get("prompt_cache_key")
            .is_some_and(|value| !value.as_str().is_some_and(|key| key.len() <= 512))
    {
        return Err(invalid());
    }
    if validation.get("tools").is_some_and(|tools| {
        tools
            .as_array()
            .is_none_or(|tools| !native_tools_valid(tools, 0))
    }) {
        return Err(invalid());
    }
    if let Some(choice) = validation.get("tool_choice") {
        let valid = match choice {
            Value::String(choice) => ["auto", "none", "required"].contains(&choice.as_str()),
            Value::Object(choice) => {
                choice
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| ["function", "custom"].contains(&kind))
                    && native_name(choice.get("name"))
                    && choice
                        .get("namespace")
                        .is_none_or(|value| native_name(Some(value)))
            }
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
    }
    let object = validation.as_object_mut().ok_or_else(invalid)?;
    for key in [
        "client_metadata",
        "parallel_tool_calls",
        "prompt_cache_key",
        "tools",
        "tool_choice",
    ] {
        object.remove(key);
    }
    if let Some(items) = validation.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            let Some(object) = item.as_object_mut() else {
                continue;
            };
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if [
                "function_call",
                "function_call_output",
                "custom_tool_call",
                "custom_tool_call_output",
            ]
            .contains(&kind.as_str())
            {
                if object
                    .get("namespace")
                    .is_some_and(|value| !native_name(Some(value)))
                {
                    return Err(invalid());
                }
                object.remove("namespace");
            }
            if kind == "custom_tool_call" {
                let input = object
                    .remove("input")
                    .filter(Value::is_string)
                    .ok_or_else(invalid)?;
                object.insert("type".into(), json!("function_call"));
                object.insert(
                    "arguments".into(),
                    json!(json!({"input": input}).to_string()),
                );
            } else if kind == "custom_tool_call_output" {
                object.insert("type".into(), json!("function_call_output"));
            }
        }
    }
    Ok(())
}

// Match the existing HTTP validators: return the prepared error response.
#[allow(clippy::result_large_err)]
pub(crate) fn native_request(request: &Value) -> Result<(), Response> {
    let object = request.as_object().ok_or_else(invalid)?;
    if object.keys().any(|key| {
        ![
            "client_metadata",
            "parallel_tool_calls",
            "prompt_cache_key",
            "model",
            "input",
            "instructions",
            "stream",
            "tools",
            "tool_choice",
            "reasoning",
            "include",
            "store",
            "truncation",
            "service_tier",
        ]
        .contains(&key.as_str())
    }) {
        return Err(invalid());
    }
    if let Some(tier) = object.get("service_tier")
        && !tier
            .as_str()
            .is_some_and(|value| ["default", "priority", "fast"].contains(&value))
    {
        return Err(invalid());
    }
    if let Some(include) = object.get("include")
        && !include.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|item| item == "reasoning.encrypted_content")
        })
    {
        return Err(invalid());
    }
    let mut validation = request.clone();
    native_validation_projection(&mut validation)?;
    validation.as_object_mut().unwrap().remove("include");
    let mut reasoning_size = 0;
    if let Some(items) = validation["input"].as_array_mut() {
        for item in items.iter().filter(|item| item["type"] == "reasoning") {
            let map = item.as_object().ok_or_else(invalid)?;
            if !map.keys().all(|key| {
                ["type", "id", "summary", "encrypted_content", "status"].contains(&key.as_str())
            }) || map
                .get("encrypted_content")
                .is_some_and(|content| !content.is_string())
                || map.get("summary").is_some_and(|parts| {
                    !parts.as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .all(|part| part["type"] == "summary_text" && part["text"].is_string())
                    })
                })
            {
                return Err(invalid());
            }
            reasoning_size += item.to_string().len();
        }
        items.retain(|item| item["type"] != "reasoning");
        for item in items {
            if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    if let Some(map) = part.as_object_mut() {
                        if map
                            .get("annotations")
                            .is_some_and(|value| !value.is_array())
                            || map.get("logprobs").is_some_and(|value| !value.is_array())
                        {
                            return Err(invalid());
                        }
                        map.remove("annotations");
                        map.remove("logprobs");
                    }
                }
            }
        }
    }
    if reasoning_size > 512 * 1024 {
        return Err(invalid());
    }
    if reasoning_size > 0 {
        let instructions = validation["instructions"].as_str().unwrap_or_default();
        validation["instructions"] = json!(format!("{instructions}{}", "x".repeat(reasoning_size)));
    }
    validate_responses_request(&validation, &alias(), None, false)?;
    Ok(())
}

fn frame(event: sse::SseEvent) -> Result<Option<Value>, &'static str> {
    let Some(data) = event.data.filter(|data| !data.is_empty()) else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&data).map_err(|_| PROVIDER_ERROR)?;
    let kind = value["type"]
        .as_str()
        .filter(|kind| kind.starts_with("response."))
        .ok_or(PROVIDER_ERROR)?;
    const KINDS: &[&str] = &[
        "response.created",
        "response.in_progress",
        "response.queued",
        "response.metadata",
        "response.completed",
        "response.incomplete",
        "response.output_item.added",
        "response.output_item.done",
        "response.content_part.added",
        "response.content_part.done",
        "response.output_text.delta",
        "response.output_text.done",
        "response.output_text.annotation.added",
        "response.function_call_arguments.delta",
        "response.function_call_arguments.done",
        "response.refusal.delta",
        "response.refusal.done",
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_part.done",
        "response.reasoning_summary_text.delta",
        "response.reasoning_summary_text.done",
        "response.reasoning_text.delta",
        "response.reasoning_text.done",
    ];
    if event.event.is_some_and(|event| event != kind)
        || !KINDS.contains(&kind)
        || responses_semantic_error(&value)
        || (kind.ends_with(".delta") && !value["delta"].is_string())
    {
        return Err(PROVIDER_ERROR);
    }
    Ok(Some(value))
}

fn events(
    response: reqwest::Response,
    deadline: tokio::time::Instant,
    first: tokio::time::Instant,
    idle: Duration,
) -> Events {
    Box::pin(async_stream::try_stream! {
        let mut source = response.bytes_stream();
        let mut decoder = sse::SseDecoder::default();
        let mut total = 0usize;
        let mut items = std::collections::BTreeMap::new();
        let mut progress = first;
        while let Some(chunk) = tokio::time::timeout_at(deadline.min(progress), source.next()).await.map_err(|_| PROVIDER_ERROR)? {
            let chunk = chunk.map_err(|_| PROVIDER_ERROR)?;
            total = total.checked_add(chunk.len()).ok_or(PROVIDER_ERROR)?;
            if total > MAX_STREAM { Err(PROVIDER_ERROR)?; }
            let mut input = chunk.as_ref();
            while let Some(event) = decoder.next_event(&mut input).map_err(|_| PROVIDER_ERROR)? {
                    if tokio::time::Instant::now() >= deadline.min(progress) { Err(PROVIDER_ERROR)?; }
                    if let Some(mut value) = frame(event)? {
                        let kind = value["type"].as_str().ok_or(PROVIDER_ERROR)?;
                        let terminal = kind == "response.completed" || kind == "response.incomplete";
                        let meaningful = terminal || kind.ends_with(".delta") || kind == "response.output_item.added" || kind == "response.output_item.done";
                        if meaningful {
                            progress = tokio::time::Instant::now() + idle;
                        }
                        if kind == "response.output_item.done" {
                            let index = value["output_index"].as_u64().filter(|index| *index < 128).ok_or(PROVIDER_ERROR)?;
                            if !value["item"].is_object() || items.insert(index, value["item"].clone()).is_some() { Err(PROVIDER_ERROR)?; }
                        }
                        if terminal {
                            let completed = kind == "response.completed";
                            if value["response"]["output"].as_array().is_none_or(Vec::is_empty) && !items.is_empty() {
                                value["response"]["output"] = json!(items.values().collect::<Vec<_>>());
                            }
                            if !valid_responses(&value["response"], value["response"]["model"].as_str().ok_or(PROVIDER_ERROR)?, true)
                                || !valid_usage(&value["response"])
                                || (completed && value["response"]["status"] != "completed")
                                || (!completed && value["response"]["status"] != "incomplete")
                                || (completed && value["response"]["output"].as_array().is_none_or(Vec::is_empty))
                            { Err(PROVIDER_ERROR)?; }
                            yield value;
                            return;
                        }
                        yield value;
                    }
            }
        }
        Err(PROVIDER_ERROR)?;
    })
}

fn usage(response: &Value) -> Value {
    let usage = &response["usage"];
    if !usage.is_object() {
        return Value::Null;
    }
    json!({"prompt_tokens":usage["input_tokens"],"completion_tokens":usage["output_tokens"],"total_tokens":usage["total_tokens"],"prompt_tokens_details":usage["input_tokens_details"],"completion_tokens_details":usage["output_tokens_details"]})
}

fn valid_usage(response: &Value) -> bool {
    let Some(usage) = response.get("usage").filter(|value| !value.is_null()) else {
        return true;
    };
    let (Some(input), Some(output), Some(total)) = (
        usage["input_tokens"].as_u64(),
        usage["output_tokens"].as_u64(),
        usage["total_tokens"].as_u64(),
    ) else {
        return false;
    };
    input.checked_add(output) == Some(total)
        && usage
            .get("input_tokens_details")
            .is_none_or(|details| details.is_null() || details.is_object())
        && usage
            .get("output_tokens_details")
            .is_none_or(|details| details.is_null() || details.is_object())
}

fn finish(response: &Value, has_tools: bool) -> Result<&'static str, &'static str> {
    if response["status"] == "incomplete" {
        return match response["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => Ok("length"),
            Some("content_filter") => Ok("content_filter"),
            _ => Err(PROVIDER_ERROR),
        };
    }
    Ok(if has_tools { "tool_calls" } else { "stop" })
}

fn chat_result(response: &Value, model: &str) -> Result<Value, &'static str> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    for item in response["output"].as_array().ok_or(PROVIDER_ERROR)? {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().ok_or(PROVIDER_ERROR)? {
                    match part["type"].as_str() {
                        Some("output_text") => text.push_str(part["text"].as_str().ok_or(PROVIDER_ERROR)?),
                        Some("refusal") => refusal.push_str(part["refusal"].as_str().ok_or(PROVIDER_ERROR)?),
                        _ => return Err(PROVIDER_ERROR),
                    }
                }
            }
            Some("function_call") => calls.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}})),
            Some("reasoning") => {
                if let Some(summary) = item["summary"].as_array() {
                    for part in summary { reasoning.push_str(part["text"].as_str().ok_or(PROVIDER_ERROR)?); }
                }
            }
            _ => return Err(PROVIDER_ERROR),
        }
    }
    if text.is_empty()
        && calls.is_empty()
        && refusal.is_empty()
        && response["status"] != "incomplete"
    {
        return Err(PROVIDER_ERROR);
    }
    let finish = finish(response, !calls.is_empty())?;
    let mut message =
        json!({"role":"assistant","content":if text.is_empty() { Value::Null } else {json!(text)}});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !refusal.is_empty() {
        message["refusal"] = json!(refusal);
    }
    Ok(
        json!({"id":response["id"],"object":"chat.completion","model":model,"created":response.get("created_at").cloned().unwrap_or(json!(0)),"choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":usage(response)}),
    )
}

#[derive(Default)]
struct ChatStream {
    text: String,
    reasoning: String,
    refusal: String,
    calls: Vec<Value>,
    indexes: HashMap<u64, usize>,
}

impl ChatStream {
    fn convert(
        &mut self,
        event: &Value,
        model: &str,
        id: &str,
        include_usage: bool,
    ) -> Result<Vec<Value>, &'static str> {
        let chunk = |delta: Value, finish: Value| json!({"id":id,"object":"chat.completion.chunk","model":model,"created":0,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        let mut output = Vec::new();
        let kind = event["type"].as_str().ok_or(PROVIDER_ERROR)?;
        match kind {
            "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.refusal.delta" => {
                let delta = event["delta"].as_str().ok_or(PROVIDER_ERROR)?;
                let (name, buffer) = match kind {
                    "response.output_text.delta" => ("content", &mut self.text),
                    "response.refusal.delta" => ("refusal", &mut self.refusal),
                    _ => ("reasoning_content", &mut self.reasoning),
                };
                buffer.push_str(delta);
                let mut value = json!({});
                value[name] = json!(delta);
                output.push(chunk(value, Value::Null));
            }
            "response.output_item.added" if event["item"]["type"] == "function_call" => {
                let index = event["output_index"].as_u64().ok_or(PROVIDER_ERROR)?;
                if self.indexes.contains_key(&index) || self.calls.len() >= 128 {
                    return Err(PROVIDER_ERROR);
                }
                let item = &event["item"];
                let call_id = item["call_id"]
                    .as_str()
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .ok_or(PROVIDER_ERROR)?;
                let name = item["name"]
                    .as_str()
                    .filter(|value| valid_function_name(value))
                    .ok_or(PROVIDER_ERROR)?;
                let arguments = item["arguments"].as_str().ok_or(PROVIDER_ERROR)?;
                let tool_index = self.calls.len();
                self.indexes.insert(index, tool_index);
                let call = json!({"index":tool_index,"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}});
                self.calls.push(call.clone());
                output.push(chunk(json!({"tool_calls":[call]}), Value::Null));
            }
            "response.function_call_arguments.delta" => {
                let index = event["output_index"].as_u64().ok_or(PROVIDER_ERROR)?;
                let tool_index = *self.indexes.get(&index).ok_or(PROVIDER_ERROR)?;
                let delta = event["delta"].as_str().ok_or(PROVIDER_ERROR)?;
                let call = &mut self.calls[tool_index];
                let arguments = call["function"]["arguments"]
                    .as_str()
                    .ok_or(PROVIDER_ERROR)?;
                call["function"]["arguments"] = json!(format!("{arguments}{delta}"));
                output.push(chunk(
                    json!({"tool_calls":[{"index":tool_index,"function":{"arguments":delta}}]}),
                    Value::Null,
                ));
            }
            "response.completed" | "response.incomplete" => {
                let result = chat_result(&event["response"], model)?;
                let message = &result["choices"][0]["message"];
                for (field, buffer) in [
                    ("content", &self.text),
                    ("reasoning_content", &self.reasoning),
                    ("refusal", &self.refusal),
                ] {
                    let complete = message[field].as_str().unwrap_or_default();
                    let suffix = complete.strip_prefix(buffer).ok_or(PROVIDER_ERROR)?;
                    if !suffix.is_empty() {
                        let mut delta = json!({});
                        delta[field] = json!(suffix);
                        output.push(chunk(delta, Value::Null));
                    }
                }
                let complete = message["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if self.calls.len() > complete.len() {
                    return Err(PROVIDER_ERROR);
                }
                for (index, call) in complete.iter().enumerate() {
                    if let Some(partial) = self.calls.get(index) {
                        if partial["id"] != call["id"]
                            || partial["function"]["name"] != call["function"]["name"]
                        {
                            return Err(PROVIDER_ERROR);
                        }
                        let suffix = call["function"]["arguments"]
                            .as_str()
                            .ok_or(PROVIDER_ERROR)?
                            .strip_prefix(
                                partial["function"]["arguments"]
                                    .as_str()
                                    .ok_or(PROVIDER_ERROR)?,
                            )
                            .ok_or(PROVIDER_ERROR)?;
                        if !suffix.is_empty() {
                            output.push(chunk(json!({"tool_calls":[{"index":index,"function":{"arguments":suffix}}]}), Value::Null));
                        }
                    } else {
                        let mut call = call.clone();
                        call["index"] = json!(index);
                        output.push(chunk(json!({"tool_calls":[call]}), Value::Null));
                    }
                }
                output.push(chunk(
                    json!({}),
                    result["choices"][0]["finish_reason"].clone(),
                ));
                if include_usage {
                    output.push(json!({"id":id,"object":"chat.completion.chunk","created":0,"model":model,"choices":[],"usage":result["usage"]}));
                }
            }
            _ => {}
        }
        Ok(output)
    }
}

pub(crate) async fn forward(
    runtime: Arc<Runtime>,
    credential: Arc<CredentialState>,
    headers: HeaderMap,
    request: Value,
    chat: bool,
    class_permit: OwnedSemaphorePermit,
) -> Response {
    let upstream_errors = runtime.upstream_errors.clone();
    let response = forward_inner(runtime, credential, headers, request, chat, class_permit).await;
    // Native inference transport/protocol failures are rendered as 502s.
    // Do not count local validation, policy, or capacity refusals.
    if response.status() == StatusCode::BAD_GATEWAY {
        upstream_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return response;
    }
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        });
    if response.status() != StatusCode::OK || !is_sse {
        return response;
    }
    // A stream can fail after its 200 headers. Observe actual body errors,
    // not Drop: a client cancelling a stream is not an upstream failure.
    let (parts, body) = response.into_parts();
    let mut counted = false;
    let stream = body.into_data_stream().map(move |item| {
        if item.is_err() && !counted {
            upstream_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            counted = true;
        }
        item
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

async fn forward_inner(
    runtime: Arc<Runtime>,
    credential: Arc<CredentialState>,
    headers: HeaderMap,
    mut request: Value,
    chat: bool,
    class_permit: OwnedSemaphorePermit,
) -> Response {
    let started = tokio::time::Instant::now();
    let config = &runtime.policy.config.codex;
    if !eligible(&credential, config) {
        runtime.rejects.fetch_add(1, Ordering::Relaxed);
        return json_error(
            StatusCode::FORBIDDEN,
            "cloud_not_allowed",
            "Codex requires explicit workload cloud, model and route permission",
        );
    }
    let name = config.alias.as_deref().unwrap_or_default().to_owned();
    if let Err(error) = affinity_key(&headers, &credential, &name) {
        return error;
    }
    if let Err(error) = normalize_extra_body(&mut request, false) {
        return error;
    }
    let stream = match request.get("stream") {
        None => false,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => return invalid(),
        },
    };
    let include_usage = request["stream_options"]["include_usage"] == true;
    let mut body = if chat {
        match chat_request(&request) {
            Ok(body) => body,
            Err(error) => return error,
        }
    } else {
        request
    };
    if let Err(error) = native_request(&body) {
        return error;
    }
    let permit = match runtime.codex_semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "cloud_capacity",
                "Codex capacity exceeded",
            );
        }
    };
    let budget = runtime
        .policy
        .config
        .class_timeout_budgets
        .get(&credential.class);
    let seconds = budget.map_or(
        runtime.policy.config.retry_policy.total_seconds.min(300),
        |budget| budget.attempt_seconds.min(budget.total_seconds),
    );
    let deadline = started + Duration::from_secs(seconds);
    let first = Duration::from_secs(
        budget
            .and_then(|budget| budget.first_output_seconds)
            .unwrap_or(seconds),
    );
    let first_deadline = deadline.min(started + first);
    let idle = Duration::from_secs(
        budget
            .and_then(|budget| budget.stream_idle_seconds)
            .unwrap_or(seconds),
    );
    let account = match tokio::time::timeout_at(first_deadline, codex_auth::access(config)).await {
        Ok(Ok(account)) => account,
        Ok(Err(_)) => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "codex_auth_unavailable",
                "Codex requires local reauthentication or safe private storage",
            );
        }
        Err(_) => {
            return json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout_budget",
                "Codex request deadline exceeded",
            );
        }
    };
    body["model"] = json!(config.model);
    body["store"] = json!(false);
    body["stream"] = json!(true);
    body["include"] = json!(["reasoning.encrypted_content"]);
    if body.get("instructions").is_none() {
        body["instructions"] = json!("You are a helpful assistant.");
    }
    if body["service_tier"] == "fast" {
        body["service_tier"] = json!("priority");
    }
    if let Some(input) = body["input"].as_str() {
        body["input"] = json!([{"type":"message","role":"user","content":[{"type":"input_text","text":input}]}]);
    }
    let mut session = [0u8; 16];
    if getrandom::fill(&mut session).is_err() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "codex_session_unavailable",
            "Codex session unavailable",
        );
    }
    let session: String = session.iter().map(|byte| format!("{byte:02x}")).collect();
    let upstream = runtime
        .codex_client
        .post(config.responses_url())
        .bearer_auth(&account.access_token)
        .header("ChatGPT-Account-Id", &account.account_id)
        .header("originator", "codex_cli_rs")
        .header("session_id", session)
        .header(header::USER_AGENT, "codex_cli_rs/slotline")
        .header(header::ACCEPT, "text/event-stream")
        .json(&body)
        .send();
    let upstream_result = tokio::time::timeout_at(first_deadline, upstream).await;
    match &upstream_result {
        Ok(Ok(response)) => {
            let event_stream = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"));
            tracing::info!(
                http_status = response.status().as_u16(),
                event_stream,
                "Codex upstream response headers received"
            );
        }
        Ok(Err(error)) => tracing::warn!(
            timeout = error.is_timeout(),
            connect = error.is_connect(),
            "Codex upstream transport failed"
        ),
        Err(_) => tracing::warn!("Codex upstream response deadline exceeded"),
    }
    let upstream = match upstream_result {
        Ok(Ok(upstream)) if upstream.status() == StatusCode::UNAUTHORIZED => {
            let _ = tokio::time::timeout_at(deadline, codex_auth::reject(config, &account)).await;
            return json_error(
                StatusCode::BAD_GATEWAY,
                "codex_auth_rejected",
                PROVIDER_ERROR,
            );
        }
        Ok(Ok(upstream))
            if upstream.status().is_success() && upstream_sse_header_valid(upstream.headers()) =>
        {
            upstream
        }
        _ => return json_error(StatusCode::BAD_GATEWAY, "codex_transport", PROVIDER_ERROR),
    };
    let model = config.model.as_deref().unwrap_or_default().to_owned();
    let mut events = events(upstream, deadline, first_deadline, idle);
    let permits = (class_permit, permit);
    if !stream {
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(_) => {
                    return json_error(StatusCode::BAD_GATEWAY, "codex_response", PROVIDER_ERROR);
                }
            };
            if event["response"]
                .get("model")
                .is_some_and(|value| value.as_str() != Some(&model))
            {
                return json_error(StatusCode::BAD_GATEWAY, "codex_response", PROVIDER_ERROR);
            }
            if matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete")
            ) {
                if !valid_responses(&event["response"], &model, true) {
                    return json_error(StatusCode::BAD_GATEWAY, "codex_response", PROVIDER_ERROR);
                }
                let mut result = event["response"].clone();
                result["model"] = json!(name);
                if chat {
                    result = match chat_result(&result, &name) {
                        Ok(result) => result,
                        Err(_) => {
                            return json_error(
                                StatusCode::BAD_GATEWAY,
                                "codex_response",
                                PROVIDER_ERROR,
                            );
                        }
                    };
                }
                let bytes = Bytes::from(result.to_string());
                let stream = async_stream::stream! {
                    let _permits = permits;
                    yield Ok::<_, std::io::Error>(bytes);
                };
                let mut response = Response::new(Body::from_stream(stream));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                return response;
            }
        }
        return json_error(StatusCode::BAD_GATEWAY, "codex_response", PROVIDER_ERROR);
    }
    let output: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(
        async_stream::try_stream! {
            let _permits = permits;
            let mut bridge = ChatStream::default();
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| std::io::Error::other(PROVIDER_ERROR))?;
            let id = format!("chatcmpl-{}", random.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
            if chat {
                yield Bytes::from(format!("data: {}\n\n", json!({"id":id,"object":"chat.completion.chunk","model":name,"created":0,"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]})));
            }
            while let Some(event) = events.next().await {
                let mut event = event.map_err(std::io::Error::other)?;
                if event["response"].get("model").is_some_and(|value| value.as_str() != Some(&model)) { Err(std::io::Error::other(PROVIDER_ERROR))?; }
                let terminal = matches!(event["type"].as_str(), Some("response.completed" | "response.incomplete"));
                if terminal && !valid_responses(&event["response"], &model, true) { Err(std::io::Error::other(PROVIDER_ERROR))?; }
                if chat {
                    for chunk in bridge.convert(&event, &name, &id, include_usage).map_err(std::io::Error::other)? {
                        yield Bytes::from(format!("data: {chunk}\n\n"));
                    }
                } else {
                    if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) { response.insert("model".into(), json!(name)); }
                    yield Bytes::from(format!("event: {}\ndata: {}\n\n", event["type"].as_str().unwrap_or_default(), event));
                }
                if terminal {
                    if chat { yield Bytes::from_static(b"data: [DONE]\n\n"); }
                    return;
                }
            }
            Err(std::io::Error::other(PROVIDER_ERROR))?;
        },
    );
    let mut response = Response::new(Body::from_stream(output));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    fn complete(items: Value) -> Value {
        json!({"id":"fixture","object":"response","model":"synthetic","status":"completed","output":items,"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}})
    }

    fn tool() -> Value {
        json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}"})
    }

    fn encode(value: Value) -> String {
        format!("data: {value}\n\n")
    }

    fn source(text: &str) -> Events {
        let chunks: Vec<Result<Bytes, io::Error>> = text
            .as_bytes()
            .chunks(3)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let body = reqwest::Body::wrap_stream(futures_util::stream::iter(chunks));
        let response = axum::http::Response::builder().body(body).unwrap();
        events(
            response.into(),
            tokio::time::Instant::now() + Duration::from_secs(2),
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        )
    }

    #[tokio::test]
    async fn codex_protocol_metadata_before_terminal_preserves_tool_call() {
        // Synthetic payloads using the event order observed from the real
        // Codex backend: metadata must not truncate a completed tool call.
        let raw = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"object\":\"response\",\"model\":\"fixture-codex\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_fixture\",\"delta\":\"{}\"}\n\n",
            "event: response.metadata\ndata: {\"type\":\"response.metadata\",\"metadata\":{\"synthetic\":true}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fixture\",\"object\":\"response\",\"model\":\"fixture-codex\",\"status\":\"completed\",\"output\":[{\"id\":\"fc_fixture\",\"type\":\"function_call\",\"call_id\":\"call_fixture\",\"name\":\"exec_command\",\"arguments\":\"{}\",\"status\":\"completed\"}]}}\n\n",
        );
        let mut stream = source(raw);
        let mut types = Vec::new();
        while let Some(item) = stream.next().await {
            let value = item.expect("valid Codex metadata must not truncate the stream");
            types.push(value["type"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            types,
            [
                "response.created",
                "response.function_call_arguments.delta",
                "response.metadata",
                "response.completed"
            ]
        );
    }

    #[test]
    fn codex_protocol_accepts_missing_sse_mime_but_rejects_wrong_mime() {
        let mut headers = HeaderMap::new();
        assert!(upstream_sse_header_valid(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(upstream_sse_header_valid(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("Text/Event-Stream"),
        );
        assert!(upstream_sse_header_valid(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        assert!(!upstream_sse_header_valid(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/html"),
        );
        assert!(!upstream_sse_header_valid(&headers));
    }

    #[test]
    fn codex_protocol_real_cli_namespaces_and_metadata_are_preserved() {
        let request = json!({
            "model":"gpt-6.1-sol", "instructions":"", "input":[{"role":"user","content":"Synthetic compatibility check"}],
            "stream":true, "store":false, "parallel_tool_calls":true,
            "prompt_cache_key":"synthetic-session", "client_metadata":{"synthetic":"fixture"},
            "tool_choice":"auto", "include":["reasoning.encrypted_content"],
            "tools":[
                {"type":"function","name":"plain","parameters":{"type":"object"}},
                {"type":"namespace","name":"functions","tools":[
                    {"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}},
                    {"type":"custom","name":"apply_patch","format":{"type":"text"}}
                ]},
                {"type":"web_search"}
            ]
        });
        let original = request.clone();
        assert!(native_request(&request).is_ok());
        assert_eq!(request, original);
        let mut history = request.clone();
        history["input"] = json!([
            {"type":"function_call","namespace":"functions","name":"exec_command","call_id":"call_synthetic","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_synthetic","output":"synthetic result"}
        ]);
        assert!(native_request(&history).is_ok());
        history["input"] = json!([
            {"type":"custom_tool_call","namespace":"functions","name":"apply_patch","call_id":"call_custom","input":"synthetic patch"},
            {"type":"custom_tool_call_output","call_id":"call_custom","output":"synthetic result"}
        ]);
        assert!(native_request(&history).is_ok());
        let mut invalid_request = request.clone();
        invalid_request["client_metadata"] = json!("not an object");
        assert!(native_request(&invalid_request).is_err());
        invalid_request = request.clone();
        invalid_request["tools"][1]["tools"][0]["name"] = json!(false);
        assert!(native_request(&invalid_request).is_err());
    }

    #[test]
    fn codex_protocol_chat_tool_conversion_and_native_reasoning_history_preserve_items() {
        let request = json!({"model":"alias","messages":[
            {"role":"system","content":"instructions"},
            {"role":"user","content":[{"type":"text","text":"lookup"}]},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
            {"role":"tool","content":"result","tool_call_id":"call_1"}
        ],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}],"tool_choice":{"type":"function","function":{"name":"lookup"}},"reasoning_effort":"high"});
        let mut converted = chat_request(&request).unwrap();
        assert_eq!(converted["input"][2]["type"], "function_call");
        assert_eq!(
            converted["input"][3],
            json!({"type":"function_call_output","call_id":"call_1","output":"result"})
        );
        assert_eq!(converted["tools"][0]["name"], "lookup");
        assert_eq!(converted["reasoning"]["effort"], "high");
        assert!(native_request(&converted).is_ok());
        converted["input"].as_array_mut().unwrap().push(json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"plan"}],"encrypted_content":"fixture-encrypted"}));
        converted["input"].as_array_mut().unwrap().push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}));
        assert!(native_request(&converted).is_ok());
        assert_eq!(
            converted["input"][4]["encrypted_content"],
            "fixture-encrypted"
        );
        for field in [
            "max_tokens",
            "temperature",
            "headers",
            "provider",
            "parallel_tool_calls",
            "text",
        ] {
            let mut unsupported = request.clone();
            unsupported[field] = json!(1);
            assert!(chat_request(&unsupported).is_err(), "{field}");
        }
    }

    #[tokio::test]
    async fn codex_protocol_terminal_done_items_are_recovered_without_losing_tools() {
        let sse =
            encode(json!({"type":"response.output_item.done","output_index":0,"item":tool()}))
                + &encode(json!({"type":"response.completed","response":complete(json!([]))}));
        let mut events = source(&sse);
        assert_eq!(events.next().await.unwrap().unwrap()["item"], tool());
        let terminal = events.next().await.unwrap().unwrap();
        assert_eq!(terminal["response"]["output"], json!([tool()]));
        let chat = chat_result(&terminal["response"], "alias").unwrap();
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["usage"]["total_tokens"], 6);
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn codex_protocol_sse_truncation_errors_framing_and_empty_success_fail_closed() {
        for invalid in [
            "data: [DONE]\n\n".to_owned(),
            "data: {}\n\n".to_owned(),
            "data: {\"type\":\"response.completed\"}".to_owned(),
            encode(
                json!({"type":"response.failed","response":{"error":{"message":"synthetic-secret"}}}),
            ),
            encode(json!({"type":"response.error","message":"synthetic-secret"})),
            encode(json!({"type":"response.output_text.delta","delta":true})),
            encode(json!({"type":"response.completed","response":complete(json!([]))})),
            format!("data: {}", "x".repeat(sse::MAX_EVENT_BYTES + 1)),
        ] {
            let mut events = source(&invalid);
            let mut failed = false;
            while let Some(item) = events.next().await {
                if let Err(error) = item {
                    assert!(!error.contains("synthetic-secret"));
                    failed = true;
                    break;
                }
            }
            assert!(failed);
        }
        for separator in ["\r\n", "\r"] {
            let sse =
                encode(json!({"type":"response.completed","response":complete(json!([tool()]))}))
                    .replace('\n', separator);
            assert!(source(&sse).next().await.unwrap().is_ok());
        }
        let mut decoder = sse::SseDecoder::default();
        let mut input =
            b"event: response.created\ndata: {\"type\":\"response.completed\"}\n\n".as_slice();
        assert!(frame(decoder.next_event(&mut input).unwrap().unwrap()).is_err());
    }

    #[test]
    fn codex_protocol_chat_sse_is_incremental_and_usage_and_truncation_are_truthful() {
        let mut bridge = ChatStream::default();
        let added = json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"lookup","arguments":""}});
        let first = bridge.convert(&added, "alias", "id", true).unwrap();
        assert_eq!(
            first[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        let delta = bridge.convert(&json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),"alias","id",true).unwrap();
        assert_eq!(
            delta[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
        let terminal = bridge
            .convert(
                &json!({"type":"response.completed","response":complete(json!([tool()]))}),
                "alias",
                "id",
                true,
            )
            .unwrap();
        assert_eq!(terminal[0]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(terminal[1]["usage"]["total_tokens"], 6);
        let mut incomplete = complete(json!([]));
        incomplete["status"] = json!("incomplete");
        incomplete["incomplete_details"] = json!({"reason":"max_output_tokens"});
        assert_eq!(
            chat_result(&incomplete, "alias").unwrap()["choices"][0]["finish_reason"],
            "length"
        );
        incomplete["incomplete_details"]["reason"] = json!("unknown");
        assert!(chat_result(&incomplete, "alias").is_err());
    }
}
