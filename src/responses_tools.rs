//! Reversible request-local tool projection for managed Responses backends.
//! Catalogs grant no authority: native clients still execute their own tools.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    name: String,
    namespace: Option<String>,
    custom: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolProjection {
    bindings: BTreeMap<String, Binding>,
}

impl ToolProjection {
    fn translates(&self) -> bool {
        self.bindings
            .values()
            .any(|binding| binding.custom || binding.namespace.is_some())
    }
    /// Flatten a single namespace level. Refuse ambiguous catalogs rather than
    /// guessing which tool a generated call should execute.
    pub(crate) fn catalog(tools: Vec<Value>) -> Result<(Self, Vec<Value>), &'static str> {
        let mut projection = Self::default();
        let mut flattened = BTreeMap::new();
        let mut pending = Vec::new();
        for tool in tools {
            if tool["type"] == "namespace" {
                let ns = tool["name"]
                    .as_str()
                    .filter(|n| valid_function_name(n))
                    .ok_or("invalid tool namespace")?;
                let children = tool["tools"]
                    .as_array()
                    .ok_or("namespace tools must be an array")?;
                for child in children {
                    if !matches!(child["type"].as_str(), Some("function" | "custom")) {
                        return Err("nested namespaces and hosted namespace tools are unsupported");
                    }
                    pending.push((Some(ns.to_owned()), child.clone()));
                }
            } else {
                pending.push((None, tool));
            }
            if pending.len() > 128 {
                return Err("at most 128 projected tools are supported");
            }
        }
        for (namespace, mut tool) in pending {
            let kind = tool["type"].as_str().unwrap_or_default();
            if !matches!(kind, "function" | "custom") {
                // Existing compatibility handling reports/refuses hosted tools.
                let key = format!("hosted:{}", flattened.len());
                flattened.insert(key, tool);
                continue;
            }
            let name = tool["name"]
                .as_str()
                .filter(|n| valid_function_name(n))
                .ok_or("invalid tool name")?
                .to_owned();
            let binding = Binding {
                name: name.clone(),
                namespace,
                custom: kind == "custom",
            };
            let local_name = if let Some(ns) = &binding.namespace {
                use sha2::{Digest, Sha256};
                let hash = Sha256::digest(format!("{ns}\0{name}").as_bytes());
                let suffix: String = hash[..12].iter().map(|b| format!("{b:02x}")).collect();
                format!("{}_{}", name.chars().take(32).collect::<String>(), suffix)
            } else {
                name.clone()
            };
            if let Some(existing) = projection.bindings.get(&local_name)
                && existing != &binding
            {
                return Err("ambiguous projected tool name");
            }
            tool["name"] = json!(local_name);
            if let Some(ns) = &binding.namespace {
                let description = tool["description"].as_str().unwrap_or_default();
                tool["description"] = json!(format!("Native tool {ns}.{name}.\n{description}"));
            }
            if let Some(existing) = flattened.get(&local_name)
                && existing != &tool
            {
                return Err("conflicting declarations for one tool");
            }
            projection.bindings.insert(local_name.clone(), binding);
            flattened.insert(local_name, tool);
        }
        Ok((projection, flattened.into_values().collect()))
    }

    pub(crate) fn local_name(&self, name: &str, namespace: Option<&str>) -> Option<&str> {
        self.bindings
            .iter()
            .find(|(_, b)| b.name == name && b.namespace.as_deref() == namespace)
            .map(|(key, _)| key.as_str())
    }

    fn input(arguments: &str) -> Result<String, &'static str> {
        let value: Value =
            serde_json::from_str(arguments).map_err(|_| "invalid custom tool arguments")?;
        let object = value.as_object().ok_or("invalid custom tool arguments")?;
        if object.len() != 1 {
            return Err("invalid custom tool arguments");
        }
        object
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or("custom tool requires string input")
    }

    fn restore_item(&self, item: &mut Value, partial: bool) -> Result<(), &'static str> {
        if !self.translates() {
            return Ok(());
        }
        match item["type"].as_str() {
            Some("message" | "reasoning") => return Ok(()),
            Some("function_call") => {}
            _ => return Err("unsupported projected output item"),
        }
        let Some(binding) = item["name"]
            .as_str()
            .and_then(|name| self.bindings.get(name))
        else {
            return Err("upstream called an undeclared tool");
        };
        let map = item.as_object_mut().ok_or("invalid tool output")?;
        map.insert("name".into(), json!(binding.name));
        // The backend namespace is never authority, including for a flat binding.
        map.remove("namespace");
        if let Some(ns) = &binding.namespace {
            map.insert("namespace".into(), json!(ns));
        }
        if binding.custom {
            let arguments = map.remove("arguments").ok_or("missing custom arguments")?;
            let arguments = arguments.as_str().ok_or("invalid custom arguments")?;
            let input = if partial && arguments.is_empty() {
                String::new()
            } else {
                Self::input(arguments)?
            };
            map.insert("type".into(), json!("custom_tool_call"));
            map.insert("input".into(), json!(input));
        }
        Ok(())
    }

    pub(crate) fn restore_response(&self, response: &mut Value) -> Result<(), &'static str> {
        if let Some(items) = response.get_mut("output").and_then(Value::as_array_mut) {
            for item in items {
                self.restore_item(item, false)?;
            }
        }
        Ok(())
    }

    pub(crate) fn stream(self) -> ToolStream {
        ToolStream {
            projection: self,
            custom_items: BTreeMap::new(),
        }
    }
}

pub(crate) struct ToolStream {
    projection: ToolProjection,
    // Only identities, never an unbounded accumulation of argument deltas.
    custom_items: BTreeMap<String, ()>,
}

impl ToolStream {
    pub(crate) fn restore(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, &'static str> {
        if !self.projection.translates() {
            return Ok(bytes);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "invalid SSE UTF-8")?;
        let Some(data) = text.lines().find_map(|line| line.strip_prefix("data: ")) else {
            return Ok(bytes);
        };
        if data == "[DONE]" {
            return Ok(bytes);
        }
        let mut event: Value = serde_json::from_str(data).map_err(|_| "invalid SSE JSON")?;
        let kind = event["type"].as_str().unwrap_or_default().to_owned();
        if kind.contains("_call")
            && !matches!(
                kind.as_str(),
                "response.function_call_arguments.delta" | "response.function_call_arguments.done"
            )
        {
            return Err("unsupported native tool event from projected backend");
        }
        if kind == "response.output_item.added" || kind == "response.output_item.done" {
            if kind.ends_with("added") && event["item"]["type"] == "function_call" {
                let item = &event["item"];
                if item["name"]
                    .as_str()
                    .and_then(|n| self.projection.bindings.get(n))
                    .is_some_and(|b| b.custom)
                {
                    if self.custom_items.len() >= 128 {
                        return Err("too many custom calls");
                    }
                    let id = item["id"]
                        .as_str()
                        .filter(|s| s.len() <= 128)
                        .ok_or("invalid custom item ID")?;
                    self.custom_items.insert(id.to_owned(), ());
                }
            }
            self.projection
                .restore_item(&mut event["item"], kind.ends_with("added"))?;
        } else if kind.starts_with("response.function_call_arguments.")
            && event["item_id"]
                .as_str()
                .is_some_and(|id| self.custom_items.contains_key(id))
        {
            // JSON string escaping cannot be translated chunk-by-chunk. The native
            // client receives a complete freeform input at the bounded done event.
            if kind.ends_with("delta") {
                return Ok(Vec::new());
            }
            if kind.ends_with("done") {
                let input = ToolProjection::input(
                    event["arguments"]
                        .as_str()
                        .ok_or("missing custom arguments")?,
                )?;
                event.as_object_mut().unwrap().remove("arguments");
                event["type"] = json!("response.custom_tool_call_input.done");
                event["input"] = json!(input);
            }
        } else if let Some(response) = event.get_mut("response") {
            self.projection.restore_response(response)?;
        }
        let mut restored = format!(
            "event: {}\ndata: {}\n\n",
            event["type"].as_str().unwrap_or(&kind),
            event
        )
        .into_bytes();
        if text.ends_with("data: [DONE]\n\n") {
            restored.extend_from_slice(b"data: [DONE]\n\n");
        }
        Ok(restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> (ToolProjection, Vec<Value>) {
        ToolProjection::catalog(vec![json!({"type":"namespace","name":"functions","tools":[
            {"type":"custom","name":"exec","description":"Run code"},
            {"type":"function","name":"read","parameters":{"type":"object"}}
        ]})])
        .unwrap()
    }

    #[test]
    fn namespace_and_custom_calls_round_trip_without_name_collisions() {
        let (projection, _) = tools();
        let exec = projection.local_name("exec", Some("functions")).unwrap();
        let read = projection.local_name("read", Some("functions")).unwrap();
        let mut response = json!({"output":[
            {"type":"function_call","name":exec,"call_id":"call1","arguments":json!({"input":"text('héllo')"}).to_string()},
            {"type":"function_call","name":read,"call_id":"call2","arguments":"{}"}
        ]});
        projection.restore_response(&mut response).unwrap();
        assert_eq!(
            response["output"][0],
            json!({"type":"custom_tool_call","name":"exec","namespace":"functions","call_id":"call1","input":"text('héllo')"})
        );
        assert_eq!(response["output"][1]["name"], "read");
        assert_eq!(response["output"][1]["namespace"], "functions");
        let (p, _) = ToolProjection::catalog(vec![
            json!({"type":"namespace","name":"a","tools":[{"type":"function","name":"same"}]}),
            json!({"type":"namespace","name":"b","tools":[{"type":"function","name":"same"}]}),
            json!({"type":"function","name":"same"}),
        ])
        .unwrap();
        assert_ne!(
            p.local_name("same", Some("a")),
            p.local_name("same", Some("b"))
        );
        assert_ne!(p.local_name("same", Some("a")), p.local_name("same", None));
    }

    #[test]
    fn projected_outputs_cannot_change_a_flat_tool_namespace_or_bypass_binding() {
        let (p, _) = ToolProjection::catalog(vec![
            json!({"type":"custom","name":"exec"}),
            json!({"type":"namespace","name":"other","tools":[{"type":"custom","name":"exec"}]}),
        ])
        .unwrap();
        let mut response = json!({"output":[{"type":"function_call","name":"exec","namespace":"other","arguments":"{\"input\":\"safe\"}"}]});
        p.restore_response(&mut response).unwrap();
        assert_eq!(response["output"][0]["name"], "exec");
        assert!(response["output"][0].get("namespace").is_none());
        let mut stream = p.stream();
        for kind in [
            "custom_tool_call",
            "mcp_call",
            "local_shell_call",
            "web_search_call",
        ] {
            let item =
                json!({"type":kind,"name":"undeclared","namespace":"other","input":"payload"});
            assert!(
                stream
                    .projection
                    .restore_response(&mut json!({"output":[item.clone()]}))
                    .is_err()
            );
            for event_type in ["response.output_item.added", "response.output_item.done"] {
                assert!(
                    stream
                        .restore(
                            format!("data: {}\n\n", json!({"type":event_type,"item":item}))
                                .into_bytes()
                        )
                        .is_err()
                );
            }
        }
        for kind in [
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.done",
            "response.mcp_call_arguments.delta",
            "response.web_search_call.in_progress",
        ] {
            assert!(stream.restore(format!("data: {}\n\n",json!({"type":kind,"item_id":"forged","input":"payload","delta":"payload"})).into_bytes()).is_err());
        }
    }

    #[test]
    fn malformed_catalogs_and_generated_arguments_fail_closed() {
        for catalog in [
            vec![json!({"type":"namespace","name":"x","tools":null})],
            vec![
                json!({"type":"namespace","name":"x","tools":[{"type":"namespace","name":"y","tools":[]}]}),
            ],
            vec![
                json!({"type":"custom","name":"same"}),
                json!({"type":"function","name":"same"}),
            ],
            (0..129)
                .map(|i| json!({"type":"function","name":format!("f{i}")}))
                .collect(),
        ] {
            assert!(ToolProjection::catalog(catalog).is_err());
        }
        let (p, _) = tools();
        let name = p.local_name("exec", Some("functions")).unwrap();
        for args in [
            "not json",
            "{}",
            "{\"input\":1}",
            "{\"input\":\"x\",\"other\":0}",
        ] {
            assert!(
                p.restore_response(
                    &mut json!({"output":[{"type":"function_call","name":name,"arguments":args}]})
                )
                .is_err()
            );
        }
        assert!(p.restore_response(&mut json!({"output":[{"type":"function_call","name":"unadvertised","arguments":"{}"}]})).is_err());
    }

    #[test]
    fn custom_sse_hides_json_fragments_and_restores_complete_input_and_terminal() {
        let (p, _) = tools();
        let name = p.local_name("exec", Some("functions")).unwrap().to_owned();
        let mut stream = p.stream();
        let encode = |v: Value| format!("data: {v}\n\n").into_bytes();
        let decode = |bytes: Vec<u8>| -> Value {
            let text = String::from_utf8(bytes).unwrap();
            serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap())
                .unwrap()
        };
        let added = stream.restore(encode(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item1","call_id":"call1","name":name,"arguments":""}}))).unwrap();
        assert_eq!(decode(added)["item"]["type"], "custom_tool_call");
        assert!(stream.restore(encode(json!({"type":"response.function_call_arguments.delta","item_id":"item1","delta":"{\"input\":"}))).unwrap().is_empty());
        let args = json!({"input":"line1\n雪"}).to_string();
        let done = decode(stream.restore(encode(json!({"type":"response.function_call_arguments.done","item_id":"item1","arguments":args}))).unwrap());
        assert_eq!(done["type"], "response.custom_tool_call_input.done");
        assert_eq!(done["input"], "line1\n雪");
        let item = json!({"type":"function_call","id":"item1","name":name,"call_id":"call1","arguments":args});
        let done = decode(
            stream
                .restore(encode(
                    json!({"type":"response.output_item.done","item":item}),
                ))
                .unwrap(),
        );
        assert_eq!(done["item"]["namespace"], "functions");
        assert_eq!(done["item"]["input"], "line1\n雪");
        let mut terminal =
            encode(json!({"type":"response.completed","response":{"output":[item]}}));
        terminal.extend_from_slice(b"data: [DONE]\n\n");
        let terminal = stream.restore(terminal).unwrap();
        assert!(terminal.ends_with(b"data: [DONE]\n\n"));
        assert_eq!(decode(terminal)["response"]["output"][0]["name"], "exec");
    }
}
