//! Codex CLI compatibility for the managed LOCAL Responses path.
//!
//! The Codex CLI sends client-side bookkeeping (`client_metadata`, `prompt_cache_key`,
//! `include: ["reasoning.encrypted_content"]`), provider-bound opaque reasoning items,
//! assistant `phase`, namespaced calls and *custom* tools (freeform `apply_patch`). A local
//! backend can honour none of the bookkeeping, so it is validated, bounded and consumed
//! here and never forwarded. Custom tools are presented to the model as one-string-argument
//! function tools; the response projection restores native names and custom inputs.
//! What a local backend cannot honour (hosted tools, shell/MCP result items) is
//! refused or reported, never silently rewritten. Native Codex targets receive the original
//! request unchanged; this module is not used for them.
use super::*;

const MAX_METADATA_ENTRIES: usize = 32;
const MAX_METADATA_BYTES: usize = 32 * 1024;

/// What was consumed or dropped, for the internal audit record only.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub projection: crate::responses_tools::ToolProjection,
    pub client_metadata: bool,
    pub prompt_cache_key: bool,
    pub encrypted_reasoning_include: bool,
    pub reasoning_items_dropped: usize,
    pub custom_tools: usize,
    pub custom_items: usize,
    pub dropped_tools: Vec<String>,
    pub dropped_tool_count: usize,
}

fn refuse(code: &str, message: String) -> Response {
    json_error(StatusCode::BAD_REQUEST, code, &message)
}

#[allow(clippy::result_large_err)]
fn validate_metadata(value: &Value) -> Result<(), Response> {
    let invalid = || {
        refuse(
            "invalid_request",
            "client_metadata must be an object of at most 32 short string entries".into(),
        )
    };
    let map = value.as_object().ok_or_else(invalid)?;
    if map.len() > MAX_METADATA_ENTRIES
        || map.iter().any(|(k, v)| k.len() > 128 || !v.is_string())
        || serde_json::to_vec(value).map_or(true, |b| b.len() > MAX_METADATA_BYTES)
    {
        return Err(invalid());
    }
    Ok(())
}

fn text_of(output: &Value) -> Option<String> {
    match output {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let kind = part["type"].as_str()?;
                if !matches!(kind, "input_text" | "output_text" | "text") {
                    return None;
                }
                text.push_str(part["text"].as_str()?);
            }
            Some(text)
        }
        _ => None,
    }
}

/// Hosted or client-resolved tool declarations a local backend cannot execute.
fn droppable_tool(kind: &str) -> bool {
    matches!(
        kind,
        "namespace"
            | "web_search"
            | "web_search_preview"
            | "image_generation"
            | "local_shell"
            | "file_search"
            | "computer_use"
            | "computer_use_preview"
            | "code_interpreter"
            | "mcp"
            | "tool_search"
    )
}

#[allow(clippy::result_large_err)]
pub(crate) fn prepare_local(request: &mut Value) -> Result<Report, Response> {
    let mut report = Report::default();
    let Some(object) = request.as_object_mut() else {
        return Ok(report);
    };
    if let Some(metadata) = object.get("client_metadata") {
        validate_metadata(metadata)?;
        object.remove("client_metadata");
        report.client_metadata = true;
    }
    if let Some(key) = object.get("prompt_cache_key") {
        if !key.as_str().is_some_and(|k| k.len() <= 512) {
            return Err(refuse(
                "invalid_request",
                "prompt_cache_key must be a string of at most 512 bytes".into(),
            ));
        }
        object.remove("prompt_cache_key");
        report.prompt_cache_key = true;
    }
    if let Some(include) = object.get_mut("include") {
        let items = include
            .as_array_mut()
            .ok_or_else(|| refuse("invalid_request", "include must be an array".into()))?;
        let before = items.len();
        items.retain(|item| item != "reasoning.encrypted_content");
        report.encrypted_reasoning_include = items.len() != before;
        if items.is_empty() {
            object.remove("include");
        }
    }
    // Codex known models supply catalogs as developer input records. Consume only
    // explicit declarations; these are not tool results or continuation state.
    let mut declarations = match object.remove("tools") {
        Some(Value::Array(tools)) => tools,
        Some(_) => return Err(refuse("invalid_tools", "tools must be an array".into())),
        None => Vec::new(),
    };
    if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
        let mut kept = Vec::new();
        for mut item in items.drain(..) {
            if item["type"] == "additional_tools" {
                if item["role"] != "developer"
                    || !item.as_object().is_some_and(|o| {
                        o.keys()
                            .all(|k| ["type", "id", "role", "tools"].contains(&k.as_str()))
                    })
                    || item
                        .get("id")
                        .is_some_and(|id| !id.as_str().is_some_and(|s| s.len() <= 128))
                {
                    return Err(refuse(
                        "invalid_tools",
                        "invalid additional_tools declaration".into(),
                    ));
                }
                let tools = item
                    .get_mut("tools")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| {
                        refuse(
                            "invalid_tools",
                            "additional_tools needs a tool array".into(),
                        )
                    })?;
                declarations.append(tools);
                if declarations.len() > 128 {
                    return Err(refuse("invalid_tools", "too many tool declarations".into()));
                }
            } else {
                kept.push(item);
            }
        }
        *items = kept;
    }
    let (projection, declarations) = crate::responses_tools::ToolProjection::catalog(declarations)
        .map_err(|message| refuse("invalid_tools", message.into()))?;
    report.projection = projection;
    if !declarations.is_empty() {
        object.insert("tools".into(), json!(declarations));
    }
    // Tools. Custom tools become one-string-argument functions; hosted ones are reported.
    let mut custom_names = std::collections::HashSet::new();
    let mut dropped_names = std::collections::HashSet::new();
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        let mut kept = Vec::new();
        for tool in tools.drain(..) {
            let kind = tool["type"].as_str().unwrap_or_default().to_owned();
            if kind == "custom" {
                let name = tool["name"].as_str().filter(|n| valid_function_name(n));
                let Some(name) = name else {
                    return Err(refuse(
                        "invalid_tool",
                        "custom tool needs a valid name".into(),
                    ));
                };
                let mut description = tool["description"].as_str().unwrap_or_default().to_owned();
                if let Some(format) = tool["format"]["syntax"].as_str() {
                    description.push_str(&format!(
                        "\n\nPass the complete freeform input ({format} syntax) in the string argument `input`."
                    ));
                } else {
                    description.push_str(
                        "\n\nPass the complete freeform input in the string argument `input`.",
                    );
                }
                // String::truncate panics if a byte limit splits a Unicode scalar.
                let end = description.floor_char_boundary(description.len().min(8192));
                description.truncate(end);
                custom_names.insert(name.to_owned());
                report.custom_tools += 1;
                kept.push(
                    json!({"type":"function","name":name,"description":description,
                    "parameters":{"type":"object","properties":{"input":{"type":"string"}},
                        "required":["input"],"additionalProperties":false}}),
                );
            } else if droppable_tool(&kind) {
                let label = tool["name"].as_str().unwrap_or(&kind).to_owned();
                dropped_names.insert(label.clone());
                report.dropped_tool_count += 1;
                // Client-controlled names are bounded and sanitized before auditing.
                if report.dropped_tools.len() < 16 {
                    let safe = |text: &str| -> String {
                        text.chars()
                            .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
                            .take(32)
                            .collect()
                    };
                    report
                        .dropped_tools
                        .push(format!("{}:{}", safe(&kind), safe(&label)));
                }
            } else {
                kept.push(tool);
            }
        }
        *tools = kept;
        if tools.is_empty() {
            object.remove("tools");
            object.remove("parallel_tool_calls");
            if object.get("tool_choice").is_some_and(Value::is_string) {
                object.remove("tool_choice");
            }
        }
    }
    if let Some(choice) = object.get_mut("tool_choice")
        && let Some(map) = choice.as_object_mut()
    {
        let name = map.get("name").and_then(Value::as_str).unwrap_or_default();
        let namespace = map.get("namespace").and_then(Value::as_str);
        if let Some(local) = report.projection.local_name(name, namespace) {
            map.insert("name".into(), json!(local));
            map.remove("namespace");
        } else if namespace.is_some() {
            return Err(refuse(
                "unsupported_tool_choice",
                "unknown tool namespace".into(),
            ));
        }
        {
            let name = map.get("name").and_then(Value::as_str).unwrap_or_default();
            if dropped_names.contains(name)
                || name.is_empty()
                    && map.get("type") != Some(&json!("function"))
                    && map.get("type") != Some(&json!("custom"))
            {
                return Err(refuse(
                    "unsupported_tool_choice",
                    "tool_choice names a tool this local route cannot execute".into(),
                ));
            }
            if map.get("type") == Some(&json!("custom")) {
                map.insert("type".into(), json!("function"));
            }
        }
    }
    // Input history.
    if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
        let mut kept = Vec::with_capacity(items.len());
        for mut item in items.drain(..) {
            let kind = item["type"].as_str().unwrap_or("message").to_owned();
            match kind.as_str() {
                // Opaque, provider-bound reasoning never goes to a local model.
                "reasoning" => {
                    report.reasoning_items_dropped += 1;
                    continue;
                }
                "message" => {
                    if let Some(map) = item.as_object_mut() {
                        map.remove("phase");
                    }
                }
                "function_call" | "custom_tool_call" => {
                    let Some(map) = item.as_object_mut() else {
                        continue;
                    };
                    let name = map.get("name").and_then(Value::as_str).unwrap_or_default();
                    let namespace = map.get("namespace").and_then(Value::as_str);
                    if let Some(local) = report.projection.local_name(name, namespace) {
                        map.insert("name".into(), json!(local));
                    } else if namespace.is_some() {
                        return Err(refuse(
                            "invalid_tools",
                            "history refers to an undeclared namespace tool".into(),
                        ));
                    }
                    map.remove("namespace");
                    if kind == "custom_tool_call" {
                        let input =
                            map.remove("input")
                                .filter(Value::is_string)
                                .ok_or_else(|| {
                                    refuse(
                                        "invalid_request",
                                        "custom_tool_call needs string input".into(),
                                    )
                                })?;
                        map.insert("type".into(), json!("function_call"));
                        map.insert(
                            "arguments".into(),
                            json!(json!({"input": input}).to_string()),
                        );
                        report.custom_items += 1;
                    }
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let Some(map) = item.as_object_mut() else {
                        continue;
                    };
                    map.remove("namespace");
                    if let Some(text) = map.get("output").and_then(text_of) {
                        map.insert("output".into(), json!(text));
                    }
                    if kind == "custom_tool_call_output" {
                        map.insert("type".into(), json!("function_call_output"));
                        report.custom_items += 1;
                    }
                }
                other => {
                    return Err(refuse(
                        "unsupported_input_item",
                        format!(
                            "Responses input item type `{other}` cannot be honoured by a local model route; use a native cloud route for this conversation"
                        ),
                    ));
                }
            }
            kept.push(item);
        }
        *items = kept;
    }
    let _ = custom_names;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mut request: Value) -> (Result<Report, Response>, Value) {
        let result = prepare_local(&mut request);
        (result, request)
    }

    #[test]
    fn codex_bookkeeping_is_validated_consumed_and_never_forwarded() {
        let (report, request) = run(json!({"model":"m","stream":true,"store":false,
            "client_metadata":{"session_id":"s","x-codex-turn-metadata":"{\"a\":1}"},
            "prompt_cache_key":"abc","include":["reasoning.encrypted_content"],
            "input":[{"type":"message","role":"assistant","phase":"commentary","content":"x"}]}));
        let report = report.unwrap();
        assert!(
            report.client_metadata && report.prompt_cache_key && report.encrypted_reasoning_include
        );
        for key in ["client_metadata", "prompt_cache_key", "include"] {
            assert!(request.get(key).is_none(), "{key}");
        }
        assert!(request["input"][0].get("phase").is_none());
        assert_eq!(request["store"], false);
    }

    #[test]
    fn client_metadata_and_cache_key_are_bounded() {
        let too_many: serde_json::Map<String, Value> =
            (0..33).map(|i| (format!("k{i}"), json!("v"))).collect();
        for bad in [
            json!({"client_metadata":"str"}),
            json!({"client_metadata":{"k":1}}),
            json!({"client_metadata":too_many}),
            json!({"client_metadata":{"k":"v".repeat(40_000)}}),
            json!({"client_metadata":{"k".repeat(129):"v"}}),
            json!({"prompt_cache_key":"x".repeat(513)}),
            json!({"prompt_cache_key":7}),
            json!({"include":"reasoning.encrypted_content"}),
        ] {
            let (result, _) = run(bad.clone());
            assert!(result.is_err(), "{bad}");
        }
        // Unrelated includes are left for the existing validator to refuse.
        let (result, request) = run(json!({"include":["message.output_text.logprobs"]}));
        assert!(result.is_ok());
        assert_eq!(request["include"], json!(["message.output_text.logprobs"]));
    }

    #[test]
    fn custom_tools_and_history_become_function_form() {
        let (report, request) = run(json!({"model":"m",
            "tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","description":"Patch files","format":{"type":"grammar","syntax":"lark","definition":"x"}},
                     {"type":"function","name":"exec_command","parameters":{"type":"object"}}]}],
            "tool_choice":{"type":"custom","name":"apply_patch","namespace":"functions"},
            "input":[
              {"type":"reasoning","id":"rs","summary":[],"encrypted_content":"opaque"},
              {"type":"custom_tool_call","call_id":"c1","name":"apply_patch","input":"*** Begin Patch","namespace":"functions"},
              {"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"ok"}]},
              {"type":"function_call","call_id":"c2","name":"exec_command","arguments":"{}","namespace":"functions"},
              {"type":"function_call_output","call_id":"c2","output":"done"}]}));
        let report = report.unwrap();
        assert_eq!(
            (
                report.custom_tools,
                report.custom_items,
                report.reasoning_items_dropped
            ),
            (1, 2, 1)
        );
        assert_eq!(request["tools"][0]["type"], "function");
        assert_eq!(
            request["tools"][0]["parameters"]["required"],
            json!(["input"])
        );
        assert_eq!(request["tool_choice"]["type"], "function");
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(
            input[0]["arguments"],
            json!(json!({"input":"*** Begin Patch"}).to_string())
        );
        assert!(input[0].get("namespace").is_none() && input[2].get("namespace").is_none());
        assert_eq!(
            input[1],
            json!({"type":"function_call_output","call_id":"c1","output":"ok"})
        );
    }

    #[test]
    fn hosted_tools_are_reported_and_unsupported_items_are_refused_explainably() {
        let (report, request) = run(json!({"model":"m","tools":[
            {"type":"web_search","external_web_access":false},
            {"type":"namespace","name":"mcp__x","tools":[]},
            {"type":"function","name":"f","parameters":{"type":"object"}}]}));
        let report = report.unwrap();
        assert_eq!(report.dropped_tools, vec!["web_search:web_search"]);
        assert_eq!(request["tools"].as_array().unwrap().len(), 1);
        // Choosing a tool that cannot run locally is refused, not rerouted.
        let (result, _) = run(
            json!({"tools":[{"type":"web_search"}],"tool_choice":{"type":"web_search","name":"web_search"}}),
        );
        assert!(result.is_err());
        for kind in [
            "local_shell_call",
            "mcp_call",
            "web_search_call",
            "item_reference",
            "compaction",
        ] {
            let (result, _) = run(json!({"input":[{"type":kind,"id":"x"}]}));
            let response = result.unwrap_err();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{kind}");
        }
    }

    #[test]
    fn dropped_tool_audit_labels_are_bounded_and_sanitized() {
        let tools: Vec<Value> = (0..40)
            .map(|i| json!({"type":"web_search","name":format!("n{i}\"{}", "x".repeat(200))}))
            .collect();
        let (report, _) = run(json!({"model":"m","tools":tools}));
        let report = report.unwrap();
        assert_eq!(report.dropped_tool_count, 40);
        assert_eq!(report.dropped_tools.len(), 16);
        assert!(report.dropped_tools.iter().all(|label| {
            label.len() <= 65
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-:".contains(c))
        }));
    }

    #[test]
    fn plain_requests_are_untouched() {
        let original = json!({"model":"m","input":[{"role":"user","content":"hi"}],"tools":[{"type":"function","name":"f","parameters":{"type":"object"}}],"tool_choice":"auto"});
        let (report, request) = run(original.clone());
        let mut report = report.unwrap();
        assert_eq!(report.projection.local_name("f", None), Some("f"));
        report.projection = Default::default();
        assert_eq!(report, Report::default());
        assert_eq!(request, original);
    }

    #[test]
    fn additional_tools_are_catalogs_not_continuations_and_namespace_history_is_preserved() {
        let original = json!({"input":[
            {"type":"additional_tools","id":"at_1","role":"developer","tools":[
                {"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec","description":"x".repeat(8191)+"雪"}]}]},
            {"role":"user","content":"make a file"}]});
        assert!(!crate::routing_plan::has_continuation_state(
            &original,
            config::RoutingEndpoint::Responses
        ));
        let (report, mut request) = run(original.clone());
        let report = report.unwrap();
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        let name = report
            .projection
            .local_name("exec", Some("functions"))
            .unwrap();
        assert_eq!(request["tools"][0]["name"], name);
        assert!(request["tools"][0]["description"].as_str().unwrap().len() <= 8192);
        request = original;
        request["input"].as_array_mut().unwrap().extend([
            json!({"type":"custom_tool_call","namespace":"functions","name":"exec","call_id":"c","input":"text(1)"}),
            json!({"type":"custom_tool_call_output","call_id":"c","output":"1"})]);
        assert!(crate::routing_plan::has_continuation_state(
            &request,
            config::RoutingEndpoint::Responses
        ));
        let (_, request) = run(request);
        assert_eq!(request["input"][1]["name"], name);
        assert_eq!(
            request["input"][1]["arguments"],
            json!({"input":"text(1)"}).to_string()
        );
        for item in [
            json!({"type":"additional_tools","role":"user","tools":[]}),
            json!({"type":"additional_tools","role":"developer","tools":{},"id":"at"}),
            json!({"type":"additional_tools","role":"developer","tools":[],"execution":"hidden"}),
        ] {
            assert!(run(json!({"input":[item]})).0.is_err());
        }
    }
}
