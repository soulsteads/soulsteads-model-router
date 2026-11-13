use super::*;

#[derive(Debug, Serialize)]
pub(crate) struct Selection {
    pub(crate) state: &'static str,
    pub(crate) outcome: &'static str,
    pub(crate) target: Option<String>,
    pub(crate) rule: Option<String>,
    pub(crate) candidates: Vec<String>,
}

impl Selection {
    pub(crate) fn new(state: &'static str, outcome: &'static str, target: Option<&str>) -> Self {
        Self {
            state,
            outcome,
            target: target.map(str::to_owned),
            rule: None,
            candidates: Vec::new(),
        }
    }
}

pub(crate) enum ModelResult<'value> {
    NotRun,
    Recorded(&'value Value),
    Failure(&'static str),
}

pub(crate) struct Context<'context> {
    pub(crate) config: &'context config::Config,
    pub(crate) credential: &'context config::Credential,
    pub(crate) profile: &'context Profile,
    pub(crate) rules: &'context [CompiledRule],
    /// Original, unnormalized body, including for synthetic classifier requests.
    /// Compatibility owns the per-alias projection; never supply an inspection copy.
    pub(crate) request: &'context Value,
    pub(crate) responses: bool,
    pub(crate) tokenizers: &'context HashMap<String, Arc<Tokenizer>>,
}

impl Context<'_> {
    pub(crate) fn compatibility(&self, name: &str) -> &'static str {
        if !supports_endpoint(self.config, name, self.responses) {
            return "incompatible";
        }
        let alias = self.config.aliases.get(name);
        let mut normalized = self.request.clone();
        // Project once, matching managed handlers' thinking rewrite and Codex's no-rewrite path.
        if normalize_extra_body(&mut normalized, alias.is_some()).is_err() {
            return "incompatible";
        }
        if self.config.codex.enabled && self.config.codex.alias.as_deref() == Some(name) {
            let compatible = if self.responses {
                codex::native_request(&normalized).is_ok()
            } else {
                codex::chat_request(&normalized)
                    .is_ok_and(|body| codex::native_request(&body).is_ok())
            };
            return if compatible {
                "static_compatible_runtime_deferred"
            } else {
                "incompatible"
            };
        }
        let Some(alias) = alias else {
            return "provider_validation_deferred";
        };
        let tokenizer = self.tokenizers.get(name).map(Arc::as_ref);
        let context_deferred = alias.tokenizer.is_some() && tokenizer.is_none();
        let mut contract = alias.clone();
        if context_deferred {
            contract.context_limit = u32::MAX;
            let reserve = if self.responses {
                normalized.get("max_output_tokens")
            } else {
                normalized
                    .get("max_completion_tokens")
                    .or_else(|| normalized.get("max_tokens"))
            };
            if reserve.and_then(Value::as_u64).unwrap_or(4096) > alias.context_limit as u64 {
                return "incompatible";
            }
        }
        let default_thinking = alias
            .routes
            .iter()
            .filter(|entry| entry.enabled)
            .any(|entry| {
                self.config.routes.iter().any(|route| {
                    route.name == entry.route && route.enabled && !route.disable_thinking
                })
            });
        let checked = if self.responses {
            validate_responses_request(&normalized, &contract, tokenizer, default_thinking).map(
                |(stream, context, tools, schema, object)| {
                    (
                        stream,
                        context,
                        RequestCapabilities {
                            tools,
                            responses: true,
                            responses_json_schema: schema,
                            responses_json_object: object,
                            ..Default::default()
                        },
                    )
                },
            )
        } else {
            validate_request(&normalized, &contract, tokenizer, default_thinking)
                .map(|(stream, _, context, needs)| (stream, context, needs))
        };
        let Ok((stream, required_context, needs)) = checked else {
            return "incompatible";
        };
        if stream && !alias.streaming {
            return "incompatible";
        }
        if !alias
            .routes
            .iter()
            .filter(|entry| entry.enabled)
            .any(|entry| {
                (self.credential.allowed_routes.is_empty()
                    || self.credential.allowed_routes.contains(&entry.route))
                    && self.config.routes.iter().any(|route| {
                        route.enabled
                            && route.name == entry.route
                            && (context_deferred || route.context_limit >= required_context)
                            && missing_capabilities(route, stream, needs).is_empty()
                    })
            })
        {
            return "incompatible";
        }
        if context_deferred {
            "context_deferred"
        } else {
            "static_compatible_runtime_deferred"
        }
    }

    fn permitted(&self, name: &str) -> bool {
        allowed_alias(self.config, self.credential, name)
    }

    fn eligible(&self, name: &str) -> bool {
        self.permitted(name)
            && supports_endpoint(self.config, name, self.responses)
            && (!self.profile.compatibility_preflight || self.compatibility(name) != "incompatible")
    }

    pub(crate) fn resolve(&self, text: &str, model: ModelResult<'_>) -> Selection {
        let profile = self.profile;
        if !self.permitted(&profile.default_target) {
            return Selection::new("rejected", "denied", None);
        }
        if profile.compatibility_preflight
            && self.compatibility(&profile.default_target) == "incompatible"
        {
            return Selection::new("rejected", "incompatible", None);
        }
        if let Some((id, target)) = match_rule(self.rules, text) {
            if !self.permitted(target) {
                return Selection::new("rejected", "denied", None);
            }
            if profile.compatibility_preflight && self.compatibility(target) == "incompatible" {
                return Selection::new("rejected", "incompatible", None);
            }
            let mut result = Selection::new("resolved", "rule", Some(target));
            result.rule = Some(id.to_owned());
            return result;
        }
        let Some(decision) = &profile.decision else {
            return Selection::new("resolved", "default", Some(&profile.default_target));
        };
        if !self.permitted(&decision.classifier) {
            return Selection::new("rejected", "denied", None);
        }
        let candidates: Vec<_> = profile
            .targets
            .iter()
            .filter(|target| self.eligible(target))
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Selection::new("rejected", "denied", None);
        }
        if profile.compatibility_preflight {
            let classifier_request = classifier_request(
                decision,
                &candidates.iter().map(String::as_str).collect::<Vec<_>>(),
                text,
            );
            let classifier_context = Context {
                request: &classifier_request,
                responses: false,
                ..*self
            };
            if classifier_context.compatibility(&decision.classifier) == "incompatible" {
                return if decision.on_failure == FailurePolicy::Default {
                    Selection::new(
                        "resolved",
                        "classifier_incompatible",
                        Some(&profile.default_target),
                    )
                } else {
                    Selection::new("rejected", "classifier_incompatible", None)
                };
            }
        }
        let parsed = match model {
            ModelResult::NotRun => {
                let mut result = Selection::new("unresolved", "classifier_not_run", None);
                result.candidates = candidates;
                return result;
            }
            ModelResult::Recorded(payload) => parse_decision(
                payload,
                &candidates.iter().map(String::as_str).collect::<Vec<_>>(),
                decision.confidence_threshold,
            ),
            ModelResult::Failure(reason) => Err(reason),
        };
        match parsed {
            Ok(target) => Selection::new("resolved", "model", Some(&target)),
            Err(reason) if decision.on_failure == FailurePolicy::Default => {
                Selection::new("resolved", reason, Some(&profile.default_target))
            }
            Err(reason) => Selection::new("rejected", reason, None),
        }
    }
}
