//! Pure canonical policy planning. No runtime, files, accounts, or network access.
use crate::config;
use serde_json::Value;

pub(crate) struct Inspection {
    pub normalized: Value,
    pub text: Option<String>,
    pub endpoint: config::RoutingEndpoint,
    pub input_bytes: usize,
}
impl Inspection {
    pub fn shape(&self) -> config::RoutingRequestShape<'_> {
        let data = match self.endpoint {
            config::RoutingEndpoint::Chat => self.normalized.get("messages"),
            config::RoutingEndpoint::Responses => self.normalized.get("input"),
        };
        config::RoutingRequestShape {
            user_text: self.text.as_deref(),
            endpoint: self.endpoint,
            stream: self.normalized["stream"].as_bool().unwrap_or(false),
            tools: self.normalized["tools"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
                || self.endpoint == config::RoutingEndpoint::Responses
                    && self.normalized["input"].as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item["type"] == "additional_tools"
                                && item["tools"]
                                    .as_array()
                                    .is_some_and(|tools| !tools.is_empty())
                        })
                    }),
            image: data.is_some_and(has_image),
            input_bytes: self.input_bytes,
        }
    }
}
fn has_image(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "image_url" | "input_image"))
                || object.values().any(has_image)
        }
        Value::Array(items) => items.iter().any(has_image),
        _ => false,
    }
}
// Only the last user turn participates. Non-text parts do not erase text in that turn.
pub(crate) fn latest_user_text(
    request: &Value,
    endpoint: config::RoutingEndpoint,
) -> Option<String> {
    let content = match endpoint {
        config::RoutingEndpoint::Chat => request
            .get("messages")?
            .as_array()?
            .iter()
            .rev()
            .find(|item| item["role"] == "user")?
            .get("content")?,
        config::RoutingEndpoint::Responses => {
            let input = request.get("input")?;
            if let Some(text) = input.as_str() {
                return Some(text.to_owned());
            }
            input
                .as_array()?
                .iter()
                .rev()
                .find(|item| {
                    item["role"] == "user" && item.get("type").is_none_or(|kind| kind == "message")
                })?
                .get("content")?
        }
    };
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|part| part["type"] == "text" || part["type"] == "input_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>();
    (!text.is_empty()).then(|| text.join("\n"))
}
pub(crate) fn inspect(
    request: &Value,
    endpoint: config::RoutingEndpoint,
    input_bytes: usize,
) -> Result<Inspection, &'static str> {
    let mut normalized = request.clone();
    crate::request_validation::project_extra_body(&mut normalized, true)
        .map_err(|error| error.code)?;
    if normalized.get("stream").is_some_and(|v| !v.is_boolean())
        || normalized.get("tools").is_some_and(|v| !v.is_array())
        || match endpoint {
            config::RoutingEndpoint::Chat => normalized.get("messages").is_none(),
            config::RoutingEndpoint::Responses => normalized.get("input").is_none(),
        }
    {
        return Err("invalid_request");
    }
    let text = latest_user_text(&normalized, endpoint);
    Ok(Inspection {
        normalized,
        text,
        endpoint,
        input_bytes,
    })
}

pub(crate) fn target_usable(
    config: &config::Config,
    credential: &config::Credential,
    target: &str,
    endpoint: config::RoutingEndpoint,
) -> bool {
    if !credential.aliases.iter().any(|alias| alias == target) {
        return false;
    }
    if config.codex.alias.as_deref() == Some(target) {
        return crate::codex::eligible(credential, &config.codex);
    }
    if let Some(model) = config.copilot.model_for_alias(target) {
        return crate::copilot::eligible(credential, &config.copilot)
            && matches!(
                (endpoint, model.endpoint),
                (
                    config::RoutingEndpoint::Chat,
                    config::CopilotEndpoint::ChatCompletions
                ) | (
                    config::RoutingEndpoint::Responses,
                    config::CopilotEndpoint::Responses
                )
            );
    }
    config.aliases.get(target).is_some_and(|alias| {
        alias.endpoint == config::Endpoint::Chat
            && alias.passthrough_route.is_none()
            && (endpoint != config::RoutingEndpoint::Responses
                || alias.capabilities.iter().any(|c| c == "responses"))
            && alias.routes.iter().any(|entry| {
                entry.enabled
                    && (credential.allowed_routes.is_empty()
                        || credential.allowed_routes.contains(&entry.route))
                    && config
                        .routes
                        .iter()
                        .any(|route| route.name == entry.route && route.enabled)
            })
    })
}

fn provider_kind(config: &config::Config, target: &str) -> &'static str {
    if config.codex.alias.as_deref() == Some(target) {
        "codex"
    } else if config.copilot.model_for_alias(target).is_some() {
        "copilot"
    } else {
        "local"
    }
}

/// Responses input item types that carry tool or continuation state. Codex CLI
/// sends `custom_tool_call`/`custom_tool_call_output` (apply_patch); the others are
/// the tool-bearing items of the Responses protocol. Apart from messages and
/// additional_tools catalogs, unknown item types are state (fail closed).
pub(crate) const RESPONSES_STATE_ITEMS: &[&str] = &[
    "function_call",
    "function_call_output",
    "custom_tool_call",
    "custom_tool_call_output",
    "local_shell_call",
    "local_shell_call_output",
    "mcp_call",
    "mcp_list_tools",
    "mcp_approval_request",
    "mcp_approval_response",
    "tool_search_call",
    "tool_search_output",
    "web_search_call",
    "file_search_call",
    "computer_call",
    "computer_call_output",
    "code_interpreter_call",
    "image_generation_call",
    "reasoning",
    "compaction",
    "item_reference",
];

/// True when the request already contains tool calls/results, opaque reasoning or
/// other continuation items, i.e. it continues a conversation begun elsewhere.
pub(crate) fn has_continuation_state(request: &Value, endpoint: config::RoutingEndpoint) -> bool {
    let key = match endpoint {
        config::RoutingEndpoint::Chat => "messages",
        config::RoutingEndpoint::Responses => "input",
    };
    let present = |item: &Value, field: &str| item.get(field).is_some_and(|v| !v.is_null());
    if endpoint == config::RoutingEndpoint::Responses
        && (present(request, "previous_response_id") || present(request, "conversation"))
    {
        return true;
    }
    request
        .get(key)
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                matches!(item["role"].as_str(), Some("tool" | "function"))
                    || present(item, "tool_calls")
                    || present(item, "tool_call_id")
                    || present(item, "function_call")
                    || present(item, "reasoning_content")
                    || present(item, "encrypted_content")
                    || item["type"].as_str().is_some_and(|kind| {
                        RESPONSES_STATE_ITEMS.contains(&kind)
                            || !matches!(kind, "message" | "additional_tools")
                    })
            })
        })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Selected {
        target: String,
        reason: &'static str,
        rule: Option<String>,
        advisory: Option<String>,
    },
    Judge {
        candidates: Vec<String>,
    },
}
// A fit callback is mandatory: serving supplies actual local/runtime validation;
// offline supplies known pure checks and separately reports the deferred facts.
pub(crate) fn plan(
    config: &config::Config,
    credential: &config::Credential,
    requested: &str,
    inspection: &Inspection,
    fits: impl FnMut(&str) -> bool,
) -> Result<Plan, &'static str> {
    plan_pinned(config, credential, requested, inspection, None, fits)
}

/// Every target the policy could ever select for this credential: decision targets, rule
/// targets and the default, after the same authorization/capability checks as selection.
pub(crate) fn eligible_candidates(
    config: &config::Config,
    credential: &config::Credential,
    requested: &str,
    endpoint: config::RoutingEndpoint,
) -> Vec<String> {
    let Some(policy) = config.routing_policies.get(requested) else {
        return Vec::new();
    };
    let mut targets: Vec<String> = Vec::new();
    for target in policy
        .decision_targets
        .iter()
        .chain(policy.rules.iter().map(|rule| &rule.target))
        .chain(policy.default_target.iter())
    {
        if !targets.contains(target) && target_usable(config, credential, target, endpoint) {
            targets.push(target.clone());
        }
    }
    targets
}

/// The provider kind of an eligible target: `local`, `codex` or `copilot`.
pub(crate) fn kind_of(config: &config::Config, target: &str) -> &'static str {
    provider_kind(config, target)
}

/// `pinned` is the target this conversation was placed on earlier (memory-only pin).
/// Eligibility is always evaluated first: a pin is honoured only while the credential is
/// still authorized for it and the request still fits, and it never widens authority.
pub(crate) fn plan_pinned(
    config: &config::Config,
    credential: &config::Credential,
    requested: &str,
    inspection: &Inspection,
    pinned: Option<&str>,
    mut fits: impl FnMut(&str) -> bool,
) -> Result<Plan, &'static str> {
    if !credential.aliases.iter().any(|name| name == requested) {
        return Err("model_not_allowed");
    }
    let policy = config
        .routing_policies
        .get(requested)
        .ok_or("no_routing_target")?;
    let shape = inspection.shape();
    let rule = policy
        .rules
        .iter()
        .find(|rule| rule.predicate.matches(&credential.class, shape));
    let allowed = |target: &str| target_usable(config, credential, target, inspection.endpoint);
    if policy.mode == config::RoutingMode::Shadow {
        let target = policy
            .default_target
            .as_deref()
            .ok_or("no_routing_target")?;
        if !allowed(target) || !fits(target) {
            return Err("model_not_allowed");
        }
        let advisory = rule
            .filter(|rule| allowed(&rule.target) && fits(&rule.target))
            .map(|rule| rule.target.clone());
        return Ok(Plan::Selected {
            target: target.into(),
            reason: "shadow",
            rule: None,
            advisory,
        });
    }
    // A conversation with an explicit client session identity that was already placed
    // stays on its first target (first placement wins until TTL/eviction/restart),
    // whether or not this turn carries tool state. The pin is honoured only while its
    // target is still one of the policy's current, authorized targets that fits.
    if let Some(pin) = pinned {
        let current = eligible_candidates(config, credential, requested, inspection.endpoint);
        if !current.iter().any(|target| target == pin) || !fits(pin) {
            return Err("pin_unavailable");
        }
        return Ok(Plan::Selected {
            target: pin.into(),
            reason: "pin",
            rule: None,
            advisory: None,
        });
    }
    // No pin: active policy never judges or reselects a stateful continuation the router
    // did not place (no session identity, restart, eviction or expiry). Fail closed even
    // when every candidate currently shares a provider kind: routes can still differ.
    if has_continuation_state(&inspection.normalized, inspection.endpoint) {
        return Err("continuation_pinned");
    }
    if let Some(target) = policy.rule_target(&credential.class, shape) {
        if !allowed(target) {
            return Err("model_not_allowed");
        }
        return Ok(Plan::Selected {
            target: target.into(),
            reason: "rule",
            rule: rule.map(|r| r.id.clone()),
            advisory: None,
        });
    }
    if let Some(model) = &policy.decision_model {
        if !target_usable(config, credential, model, config::RoutingEndpoint::Chat) {
            return Err("decision_not_allowed");
        }
        let candidates: Vec<_> = policy
            .decision_targets
            .iter()
            .filter(|target| allowed(target))
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Err("model_not_allowed");
        }
        if inspection.normalized.to_string().len() > 32768 {
            return Err("decision_input_too_large");
        }
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|target| fits(target))
            .collect();
        if candidates.is_empty() {
            return Err("model_not_allowed");
        }
        return Ok(Plan::Judge { candidates });
    }
    let target = policy
        .default_target
        .as_deref()
        .ok_or("no_routing_target")?;
    if !allowed(target) {
        return Err("model_not_allowed");
    }
    Ok(Plan::Selected {
        target: target.into(),
        reason: "default",
        rule: None,
        advisory: None,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    target: String,
}
pub(crate) fn parse_decision(
    payload: &Value,
    candidates: &[String],
) -> Result<String, &'static str> {
    let choices = payload["choices"]
        .as_array()
        .filter(|items| items.len() == 1)
        .ok_or("invalid_decision")?;
    let choice = &choices[0];
    let message = &choice["message"];
    if choice["finish_reason"] != "stop"
        || message
            .get("tool_calls")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|calls| !calls.is_empty()))
        || message.get("function_call").is_some_and(|v| !v.is_null())
    {
        return Err("invalid_decision");
    }
    let content = message["content"]
        .as_str()
        .filter(|content| content.len() <= 1024)
        .ok_or("invalid_decision")?;
    let decision: Decision = serde_json::from_str(content).map_err(|_| "invalid_decision")?;
    if !candidates.contains(&decision.target) {
        return Err("invalid_decision");
    }
    Ok(decision.target)
}

// Exact pure request/capability/context prefilter for managed aliases and Codex.
pub(crate) fn request_fits(
    config: &config::Config,
    credential: &config::Credential,
    target: &str,
    endpoint: config::RoutingEndpoint,
    original_request: &Value,
    tokenizer: Option<&tokenizers::Tokenizer>,
) -> bool {
    request_fits_on_routes(
        config,
        credential,
        target,
        endpoint,
        original_request,
        tokenizer,
        config.routes.iter(),
    )
}
// Preserve declared alias/authorized route reserve bounds when prompt sizing is unknown.
pub(crate) fn request_fits_offline(
    config: &config::Config,
    credential: &config::Credential,
    target: &str,
    endpoint: config::RoutingEndpoint,
    original_request: &Value,
) -> bool {
    if config
        .aliases
        .get(target)
        .is_some_and(|alias| alias.tokenizer.is_some())
    {
        request_fits_with_prompt_sizing(
            config,
            credential,
            target,
            endpoint,
            original_request,
            crate::validation::PromptSizing::Deferred,
            config.routes.iter(),
        )
    } else {
        request_fits(config, credential, target, endpoint, original_request, None)
    }
}

// Serving supplies the effective loaded routes; offline supplies declared routes.
pub(crate) fn request_fits_on_routes<'a>(
    config: &config::Config,
    credential: &config::Credential,
    target: &str,
    endpoint: config::RoutingEndpoint,
    original_request: &Value,
    tokenizer: Option<&tokenizers::Tokenizer>,
    routes: impl Iterator<Item = &'a config::Route> + Clone,
) -> bool {
    request_fits_with_prompt_sizing(
        config,
        credential,
        target,
        endpoint,
        original_request,
        crate::validation::PromptSizing::Estimate(tokenizer),
        routes,
    )
}

fn request_fits_with_prompt_sizing<'a>(
    config: &config::Config,
    credential: &config::Credential,
    target: &str,
    endpoint: config::RoutingEndpoint,
    original_request: &Value,
    prompt_sizing: crate::validation::PromptSizing<'_>,
    routes: impl Iterator<Item = &'a config::Route> + Clone,
) -> bool {
    let Some(alias) = config.aliases.get(target) else {
        if config.codex.alias.as_deref() == Some(target) {
            return crate::codex::project_request(
                original_request,
                endpoint == config::RoutingEndpoint::Chat,
            )
            .is_ok();
        }
        // Copilot account/network capabilities are unavailable to pure validation.
        return true;
    };
    let mut request = original_request.clone();
    if crate::normalize_extra_body(&mut request, true).is_err() {
        return false;
    }
    request["model"] = Value::String(target.to_owned());
    if endpoint == config::RoutingEndpoint::Responses
        && crate::responses_compat::prepare_local(&mut request).is_err()
    {
        return false;
    }
    let default_thinking = alias
        .routes
        .iter()
        .filter(|entry| entry.enabled)
        .filter_map(|entry| routes.clone().find(|route| route.name == entry.route))
        .any(|route| route.enabled && !route.disable_thinking);
    let (stream, required_context, needs) = match endpoint {
        config::RoutingEndpoint::Chat => {
            let Ok((stream, _, required_context, needs)) =
                crate::validation::validate_request_with_prompt_sizing(
                    &request,
                    alias,
                    prompt_sizing,
                    default_thinking,
                )
            else {
                return false;
            };
            (stream, required_context, needs)
        }
        config::RoutingEndpoint::Responses => {
            let Ok((
                stream,
                required_context,
                needs_tools,
                needs_responses_json_schema,
                needs_responses_json_object,
            )) = crate::validation::validate_responses_request_with_prompt_sizing(
                &request,
                alias,
                prompt_sizing,
                default_thinking,
            )
            else {
                return false;
            };
            (
                stream,
                required_context,
                crate::RequestCapabilities {
                    tools: needs_tools,
                    responses: true,
                    responses_json_schema: needs_responses_json_schema,
                    responses_json_object: needs_responses_json_object,
                    ..Default::default()
                },
            )
        }
    };
    alias.routes.iter().any(|entry| {
        entry.enabled
            && (credential.allowed_routes.is_empty()
                || credential.allowed_routes.contains(&entry.route))
            && routes.clone().any(|route| {
                route.name == entry.route
                    && route.enabled
                    && required_context <= route.context_limit
                    && crate::routing::missing_capabilities(route, stream, needs).is_empty()
            })
    })
}
