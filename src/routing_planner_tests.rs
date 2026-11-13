use super::*;

#[test]
fn routing_additions_mode_version_and_text_are_canonical_conditions() {
    for condition in [
        json!({"keywords":["debug", "rust"]}),
        json!({"regex":"fix.*bug", "case_sensitive":true}),
    ] {
        let parsed = serde_json::from_value::<config::RoutingPolicy>(json!({
            "mode":"shadow", "version":"synthetic-v1", "default_target":"local",
            "rules":[{"id":"Unicode λ rule", "target":"local", "when":{"user_text":condition}}]
        }));
        assert!(
            parsed.is_ok(),
            "canonical mode/version/user_text must parse"
        );
    }
}

#[test]
fn routing_additions_provenance_tracks_new_authority() {
    let base = json!({"bind":"100.127.255.254","port":4000,"routes":[],"aliases":{},
        "routing_policies":{"smart":{"default_target":"local"}}});
    let original: config::Config = serde_json::from_value(base.clone()).unwrap();
    for (key, value) in [
        ("mode", json!("shadow")),
        ("version", json!("synthetic-v1")),
    ] {
        let mut changed = base.clone();
        changed["routing_policies"]["smart"][key] = value;
        let parsed = serde_json::from_value::<config::Config>(changed);
        assert!(parsed.is_ok(), "new policy authority must parse");
        assert_ne!(
            audit_identity::digest(&original),
            audit_identity::digest(&parsed.unwrap())
        );
    }
}

#[test]
fn routing_combined_audit_projection_preserves_hindsight_and_routing_authority() {
    let mut combined = pure_config(json!({
        "mode":"shadow", "version":"synthetic-combined-v1", "default_target":"small",
        "rules":[{"id":"synthetic-rule", "target":"small",
            "when":{"user_text":{"keywords":["debug"]}}}]
    }));
    combined.hindsight = serde_json::from_value(json!({
        "enabled":true, "endpoint":"https://memory.example.invalid", "timeout_ms":1000,
        "api_key_env":"SYNTHETIC_MEMORY_TOKEN_REFERENCE",
        "bindings":{"synthetic":{"bank_id":"synthetic-bank", "allow_retain":true}}
    }))
    .unwrap();
    let projection = audit_identity::projection(&combined);
    assert_eq!(projection["schema"], 5);
    assert_eq!(projection["hindsight"]["enabled"], true);
    assert_eq!(
        projection["hindsight"]["bindings"]["synthetic"]["bank_id"],
        "synthetic-bank"
    );
    assert_eq!(projection["routing_policies"]["smart"]["mode"], "shadow");
    assert_eq!(
        projection["routing_policies"]["smart"]["version"],
        "synthetic-combined-v1"
    );
    assert_eq!(
        projection["routing_policies"]["smart"]["rules"][0]["when"]["user_text"]["keywords"],
        json!(["debug"])
    );
    assert!(
        !projection
            .to_string()
            .contains("SYNTHETIC_MEMORY_TOKEN_REFERENCE")
    );
    let original = audit_identity::digest(&combined);
    let mut changed = combined.clone();
    changed
        .hindsight
        .bindings
        .get_mut("synthetic")
        .unwrap()
        .allow_retain = false;
    assert_ne!(original, audit_identity::digest(&changed));
    let mut changed = combined.clone();
    changed.routing_policies.get_mut("smart").unwrap().version =
        Some("synthetic-combined-v2".into());
    assert_ne!(original, audit_identity::digest(&changed));
    combined.hindsight.api_key_env = Some("SYNTHETIC_OTHER_TOKEN_REFERENCE".into());
    assert_eq!(original, audit_identity::digest(&combined));
}

fn pure_config(policy: Value) -> config::Config {
    serde_json::from_value(json!({"bind":"100.127.255.254","port":4000,
        "routes":[{"name":"local","kind":"local","base_url":"http://127.0.0.1:9/v1",
            "model":"synthetic","safe_concurrency":1,"context_limit":32768,
            "adapter_version":"synthetic","enabled":true,"capabilities":["text","responses","tools"],"streaming":true}],
        "aliases":{"small":{"context_limit":32768,"capabilities":["text","responses","tools"],
            "routes":[{"route":"local","priority":0}]},
            "judge":{"context_limit":32768,"routes":[{"route":"local","priority":0}]}},
        "routing_policies":{"smart":policy},
        "codex":{"enabled":true,"alias":"cloud","model":"synthetic"},
        "copilot":{"enabled":true,"models":[{"alias":"copilot-chat","model":"synthetic","endpoint":"chat_completions"}]}
    })).unwrap()
}
fn grants() -> config::Credential {
    serde_json::from_value(json!({"name":"synthetic","class":"batch",
        "aliases":["smart","small","judge","cloud","copilot-chat"],
        "allowed_routes":["local","codex","copilot"],"cloud_allowed":true}))
    .unwrap()
}
fn chat_value(text: &str) -> Value {
    json!({"model":"smart","messages":[{"role":"user","content":text}]})
}

#[test]
fn routing_additions_user_text_only_schema_and_bounds() {
    let valid = serde_json::from_value::<config::RoutingPredicate>(
        json!({"user_text":{"keywords":["debug"]}}),
    );
    assert!(
        valid.is_ok(),
        "user_text alone is a supported canonical predicate"
    );
    for text in [
        json!({}),
        json!({"keywords":[]}),
        json!({"keywords":[""]}),
        json!({"keywords":["a"],"regex":"a"}),
        json!({"regex":"("}),
        json!({"regex":"a".repeat(513)}),
        json!({"keywords":vec!["x";17]}),
        json!({"keywords":["x".repeat(129)]}),
        json!({"regex":"a","unknown":true}),
    ] {
        assert!(
            serde_json::from_value::<config::RoutingPredicate>(json!({"user_text":text})).is_err()
        );
    }
}
#[test]
fn routing_additions_text_last_user_mixed_content_case_and_rule_order() {
    let config = pure_config(json!({"rules":[
        {"id":"first λ","target":"small","when":{"user_text":{"keywords":["RUST", "fix.*"]},"credential_classes":["batch"],"min_input_bytes":10,"max_input_bytes":10}},
        {"id":"second","target":"cloud","when":{"user_text":{"regex":"rust"}}}]}));
    let policy = &config.routing_policies["smart"];
    let mut request = json!({"model":"smart","messages":[{"role":"system","content":"rust"},
        {"role":"user","content":"old rust"},{"role":"assistant","content":"rust"},
        {"role":"user","content":[{"type":"image_url","image_url":{"url":"synthetic"}},
            {"type":"text","text":"debug"},{"type":"text","text":"Rust"}]},
        {"role":"tool","content":"rust"}]});
    let inspect = |value: &Value, bytes| {
        routing_plan::inspect(value, config::RoutingEndpoint::Chat, bytes).unwrap()
    };
    let inspected = inspect(&request, 10);
    assert_eq!(inspected.text.as_deref(), Some("debug\nRust"));
    assert_eq!(
        policy.rule_target("batch", inspected.shape()),
        Some("small")
    );
    assert_eq!(
        policy.rule_target("other", inspected.shape()),
        Some("cloud")
    );
    assert_eq!(
        policy.rule_target("batch", inspect(&request, 9).shape()),
        Some("cloud")
    );
    assert_eq!(
        policy.rule_target("batch", inspect(&request, 11).shape()),
        Some("cloud")
    );
    request["messages"][3]["content"] =
        json!([{"type":"image_url","image_url":{"url":"synthetic"}}]);
    assert_eq!(
        policy.rule_target("batch", inspect(&request, 10).shape()),
        None
    );
    request["messages"][3]["content"] = json!("fixXYZ");
    assert_eq!(
        policy.rule_target("batch", inspect(&request, 10).shape()),
        None,
        "keywords are literal substrings"
    );
    let sensitive: config::RoutingPolicy = serde_json::from_value(json!({"rules":[{"id":"sensitive","target":"small","when":{"user_text":{"regex":"^Rust$","case_sensitive":true}}}]})).unwrap();
    assert_eq!(
        sensitive.rule_target("batch", inspect(&chat_value("rust"), 10).shape()),
        None
    );
    assert_eq!(
        sensitive.rule_target("batch", inspect(&chat_value("Rust"), 10).shape()),
        Some("small")
    );
}
#[test]
fn routing_additions_responses_extraction_ignores_opaque_and_tool_history() {
    let endpoint = config::RoutingEndpoint::Responses;
    for (request, expected) in [
        (json!({"input":"rust"}), Some("rust")),
        (
            json!({"input":[{"role":"system","content":"rust"},{"role":"assistant","content":"rust"},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"rust"}]},
            {"type":"function_call_output","call_id":"call_synthetic","output":"rust"}]}),
            None,
        ),
        (
            json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"synthetic"},
            {"type":"input_text","text":"rust"}]},{"type":"reasoning","encrypted_content":"synthetic"}]}),
            Some("rust"),
        ),
        (
            json!({"input":[{"role":"user","content":"old rust"},{"type":"message","role":"user","content":[]}]}),
            None,
        ),
    ] {
        assert_eq!(
            routing_plan::latest_user_text(&request, endpoint).as_deref(),
            expected
        );
    }
}
#[test]
fn routing_additions_shadow_baseline_and_advisory_have_independent_authority_no_judge() {
    let config = pure_config(
        json!({"mode":"shadow","default_target":"small","decision_model":"judge",
        "decision_targets":["cloud"],"rules":[{"id":"cloud","target":"cloud","when":{"stream":false}}]}),
    );
    let inspection =
        routing_plan::inspect(&chat_value("hello"), config::RoutingEndpoint::Chat, 80).unwrap();
    let mut credential = grants();
    credential.aliases.retain(|a| a != "judge");
    let selected = |advisory| routing_plan::Plan::Selected {
        target: "small".into(),
        reason: "shadow",
        rule: None,
        advisory,
    };
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |_| true),
        Ok(selected(Some("cloud".into())))
    );
    credential.cloud_allowed = false;
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |_| true),
        Ok(selected(None))
    );
    credential.aliases.retain(|a| a != "small");
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |_| true),
        Err("model_not_allowed")
    );
    credential = grants();
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |t| t != "small"),
        Err("model_not_allowed")
    );
}
#[test]
fn routing_additions_active_unused_default_and_one_candidate_still_requires_judge() {
    let config = pure_config(
        json!({"default_target":"cloud","decision_model":"judge","decision_targets":["small","cloud"]}),
    );
    let mut credential = grants();
    credential.cloud_allowed = false;
    let inspection =
        routing_plan::inspect(&chat_value("hello"), config::RoutingEndpoint::Chat, 80).unwrap();
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |_| true),
        Ok(routing_plan::Plan::Judge {
            candidates: vec!["small".into()]
        })
    );
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |_| false),
        Err("model_not_allowed")
    );
    credential.aliases.retain(|a| a != "judge");
    assert_eq!(
        routing_plan::plan(&config, &credential, "smart", &inspection, |_| true),
        Err("decision_not_allowed")
    );
}
#[test]
fn routing_additions_judge_parser_is_target_only_no_rescue() {
    let candidates = vec!["small".into()];
    for (content, valid) in [
        (r#"{"target":"small"}"#, true),
        (r#"{"target":"cloud"}"#, false),
        (r#"{"target":"small","confidence":1}"#, false),
        (r#"{"target":"small","target":"small"}"#, false),
        ("not JSON", false),
    ] {
        let payload = json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]});
        assert_eq!(
            routing_plan::parse_decision(&payload, &candidates).is_ok(),
            valid
        );
    }
    for patch in [
        json!({"choices":[]}),
        json!({"choices":[{"finish_reason":"length","message":{"content":"{\"target\":\"small\"}"}}]}),
        json!({"choices":[{"finish_reason":"stop","message":{"content":"{\"target\":\"small\"}","tool_calls":[{}]}}]}),
    ] {
        assert_eq!(
            routing_plan::parse_decision(&patch, &candidates),
            Err("invalid_decision")
        );
    }
}
#[test]
fn routing_additions_codex_exact_original_projection_stream_extra_and_thinking() {
    let config =
        pure_config(json!({"decision_model":"judge","decision_targets":["cloud","small"]}));
    for endpoint in [
        config::RoutingEndpoint::Chat,
        config::RoutingEndpoint::Responses,
    ] {
        let chat = endpoint == config::RoutingEndpoint::Chat;
        let mut original = if chat {
            chat_value("hello")
        } else {
            json!({"model":"smart","input":"hello"})
        };
        assert!(routing_plan::request_fits(
            &config,
            &grants(),
            "cloud",
            endpoint,
            &original,
            None
        ));
        for extra in [
            json!({"stream":"false"}),
            json!({"enable_thinking":false}),
            json!({"extra_body":{}}),
            json!({"extra_body":{"stream":false}}),
        ] {
            original["extra_body"] = extra;
            let before = original.clone();
            assert!(!routing_plan::request_fits(
                &config,
                &grants(),
                "cloud",
                endpoint,
                &original,
                None
            ));
            assert_eq!(original, before);
        }
        original["extra_body"] = json!({"stream":true});
        let (projected, stream, _) = codex::project_request(&original, chat).unwrap();
        assert!(stream);
        assert_eq!(projected["stream"], true);
        assert!(projected.get("extra_body").is_none());
        original["stream"] = json!(false);
        assert!(
            codex::project_request(&original, chat).is_err(),
            "conflicts cannot be overwritten"
        );
    }
    let mut request = chat_value("hello");
    request["extra_body"] = json!({"enable_thinking":false});
    assert!(routing_plan::request_fits(
        &config,
        &grants(),
        "small",
        config::RoutingEndpoint::Chat,
        &request,
        None
    ));
    request["chat_template_kwargs"] = json!({});
    assert!(!routing_plan::request_fits(
        &config,
        &grants(),
        "small",
        config::RoutingEndpoint::Chat,
        &request,
        None
    ));
}
#[test]
fn routing_additions_codex_history_and_copilot_body_survive_inspection() {
    let config = pure_config(json!({"default_target":"small"}));
    let history = json!({"model":"smart","input":[
        {"type":"reasoning","id":"rs_synthetic","encrypted_content":"synthetic","summary":[]},
        {"type":"function_call","namespace":"functions","name":"run","call_id":"call_synthetic","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_synthetic","output":"synthetic"}],
        "tools":[{"type":"namespace","name":"functions","tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]}],
        "include":["reasoning.encrypted_content"]});
    let (prepared, _, _) = codex::project_request(&history, false).unwrap();
    assert_eq!(prepared, history);
    let original = json!({"model":"smart","messages":[{"role":"user","content":"hi"}],
        "metadata":{"synthetic":true},"extra_body":{"enable_thinking":false}});
    let before = original.clone();
    routing_plan::inspect(
        &original,
        config::RoutingEndpoint::Chat,
        original.to_string().len(),
    )
    .unwrap();
    assert!(routing_plan::request_fits(
        &config,
        &grants(),
        "copilot-chat",
        config::RoutingEndpoint::Chat,
        &original,
        None
    ));
    assert_eq!(original, before);
}
#[test]
fn routing_additions_local_capability_and_physical_grants_remain_mandatory() {
    let mut config = pure_config(json!({"decision_model":"judge","decision_targets":["small"]}));
    let request =
        json!({"model":"smart","messages":[{"role":"user","content":"hello"}],"stream":true});
    config.routes[0].streaming = false;
    let inspect = routing_plan::inspect(
        &request,
        config::RoutingEndpoint::Chat,
        request.to_string().len(),
    )
    .unwrap();
    assert_eq!(
        routing_plan::plan(&config, &grants(), "smart", &inspect, |target| {
            routing_plan::request_fits(
                &config,
                &grants(),
                target,
                config::RoutingEndpoint::Chat,
                &request,
                None,
            )
        }),
        Err("model_not_allowed")
    );
    config.routes[0].streaming = true;
    config.routes[0].context_limit = 1;
    assert!(!routing_plan::request_fits(
        &config,
        &grants(),
        "small",
        config::RoutingEndpoint::Chat,
        &request,
        None
    ));
    let mut credential = grants();
    credential.allowed_routes = vec!["codex".into()];
    assert!(!routing_plan::target_usable(
        &config,
        &credential,
        "small",
        config::RoutingEndpoint::Chat
    ));
}

#[test]
fn routing_additions_policy_hash_scope_mode_version_text_and_reload_identity() {
    let config = pure_config(
        json!({"default_target":"small","rules":[{"id":"λ rule","target":"small","when":{"user_text":{"keywords":["rust"]}}}]}),
    );
    let original = model_routing::policy_hash(&config, "smart");
    for edit in ["mode", "version", "text", "order", "guidance"] {
        let mut changed = config.clone();
        let policy = changed.routing_policies.get_mut("smart").unwrap();
        match edit {
            "mode" => policy.mode = config::RoutingMode::Shadow,
            "version" => policy.version = Some("synthetic-v2".into()),
            "text" => {
                policy.rules[0].predicate.user_text =
                    Some(serde_json::from_value(json!({"regex":"rust"})).unwrap())
            }
            "order" => {
                let mut rule = policy.rules[0].clone();
                rule.id = "another".into();
                policy.rules.insert(0, rule);
            }
            _ => policy.decision_prompt = "synthetic guidance".into(),
        }
        assert_ne!(
            original,
            model_routing::policy_hash(&changed, "smart"),
            "{edit}"
        );
        assert_ne!(
            audit_identity::digest(&config),
            audit_identity::digest(&changed),
            "{edit}"
        );
        let as_policy = |config| config::Policy {
            config,
            credentials: vec![],
            routes: vec![],
            matchers: vec![],
            tokenizers: HashMap::new(),
            key_snapshot: None,
        };
        assert!(
            validate_reload(&as_policy(config.clone()), &as_policy(changed)).is_err(),
            "{edit}"
        );
    }
    let mut excluded = config.clone();
    excluded.routes[0].safe_concurrency = 2;
    excluded.routes[0].credential_file = Some("SYNTHETIC_SECRET_PATH".into());
    excluded.aliases.get_mut("small").unwrap().tokenizer = Some(config::TokenizerSpec {
        file: "SYNTHETIC_TOKENIZER_PATH".into(),
        sha256: "0".repeat(64),
    });
    assert_eq!(original, model_routing::policy_hash(&excluded, "smart"));
    assert_ne!(
        audit_identity::digest(&config),
        audit_identity::digest(&excluded)
    );
}
#[test]
fn routing_additions_static_validation_mode_version_without_tightening_rule_ids() {
    let config = pure_config(json!({"default_target":"small"}));
    let re = regex::Regex::new("^[a-z]+$").unwrap();
    for policy in [
        json!({"mode":"shadow","rules":[{"id":"λ rule","target":"small","when":{"stream":true}}]}),
        json!({"default_target":"small","version":""}),
        json!({"default_target":"small","version":"x".repeat(65)}),
        json!({"default_target":"small","version":"bad label"}),
        json!({"default_target":"small","version":"λ"}),
    ] {
        let mut changed = config.clone();
        changed
            .routing_policies
            .insert("smart".into(), serde_json::from_value(policy).unwrap());
        assert!(config::validate_routing_policies(&changed, &re).is_err());
    }
    let config = pure_config(
        json!({"mode":"shadow","version":"synthetic-v1","default_target":"small","rules":[{"id":"λ rule","target":"small","when":{"stream":true}}]}),
    );
    assert!(config::validate_routing_policies(&config, &re).is_ok());
}
#[test]
fn routing_additions_offline_same_class_raw_bytes_and_explicit_deferrals() {
    let mut config = pure_config(
        json!({"default_target":"cloud","rules":[{"id":"batch","target":"small","when":{"credential_classes":["batch"],"max_input_bytes":100}}]}),
    );
    let raw = format!(
        "{}{}",
        chat_value("hello"),
        " ".repeat(100 - chat_value("hello").to_string().len())
    );
    let mut fixture = json!({"schema_version":1,"id":"synthetic-case","endpoint":"chat","input_bytes":raw.len(),"raw_body":raw,
        "grants":{"synthetic":true,"credential_class":"batch","aliases":["smart","small","cloud","judge"],"allowed_routes":["local","codex"],"cloud_allowed":true}});
    let explain = |fixture: &Value| {
        let case: routing_cli::Case = serde_json::from_value(fixture.clone()).unwrap();
        serde_json::to_value(routing_cli::explain(&config, &case).unwrap()).unwrap()
    };
    let result = explain(&fixture);
    assert_eq!(
        result["prediction"],
        json!({"state":"deferred","outcome":"rule","target":"small"})
    );
    assert_eq!(result["deferred"], json!(["runtime_availability"]));
    fixture["grants"]["credential_class"] = json!("different");
    assert_eq!(explain(&fixture)["prediction"]["target"], "cloud");
    fixture["grants"]["credential_class"] = json!("batch");
    fixture["raw_body"] = json!(format!("{} ", fixture["raw_body"].as_str().unwrap()));
    fixture["input_bytes"] = json!(101);
    assert_eq!(explain(&fixture)["prediction"]["target"], "cloud");
    config.aliases.get_mut("small").unwrap().tokenizer = Some(config::TokenizerSpec {
        file: "/synthetic/unreadable-tokenizer".into(),
        sha256: "0".repeat(64),
    });
    fixture["raw_body"] = json!(chat_value("hello").to_string());
    fixture["input_bytes"] = json!(fixture["raw_body"].as_str().unwrap().len());
    let case: routing_cli::Case = serde_json::from_value(fixture.clone()).unwrap();
    let result = serde_json::to_value(routing_cli::explain(&config, &case).unwrap()).unwrap();
    assert_eq!(
        result["deferred"],
        json!(["runtime_availability", "tokenizer_context"])
    );
    fixture["input_bytes"] = json!(0);
    let case: routing_cli::Case = serde_json::from_value(fixture).unwrap();
    assert!(routing_cli::explain(&config, &case).is_err());
}

#[test]
fn routing_additions_inspection_preserves_extra_body_error_codes() {
    let mut request = chat_value("hello");
    request["stream"] = json!(false);
    request["extra_body"] = json!({"stream":true});
    assert_eq!(
        routing_plan::inspect(&request, config::RoutingEndpoint::Chat, 100).err(),
        Some("unsupported_parameter")
    );
}

#[tokio::test]
async fn routing_additions_prefilter_uses_effective_loaded_route_context() {
    let config = pure_config(json!({"mode":"shadow","default_target":"small"}));
    let mut route = config.routes[0].clone();
    route.context_limit = 1;
    let policy = config::Policy {
        config,
        credentials: vec![config::LoadedCredential {
            config: grants(),
            key: Some(b"synthetic-runtime-fixture-key-0123456789".to_vec()),
        }],
        routes: vec![config::LoadedRoute {
            config: route,
            key: None,
        }],
        matchers: vec![],
        tokenizers: HashMap::new(),
        key_snapshot: None,
    };
    let app = AppState::new(PathBuf::from("synthetic-unused.json"), policy).unwrap();
    let runtime = app.current();
    let credential = runtime.auth.read().unwrap().credentials["synthetic"].clone();
    let request = chat_value("hello");
    let result = model_routing::policy_target(
        &runtime,
        &credential,
        "smart",
        config::RoutingEndpoint::Chat,
        &request,
        100,
    )
    .await;
    assert!(
        matches!(result,Err(ref error) if error.status()==StatusCode::FORBIDDEN),
        "effective physical context must filter the baseline before dispatch"
    );
}

fn repair_offline_explanation(
    config: &config::Config,
    endpoint: &str,
    request: Value,
    routes: &[&str],
) -> Value {
    let raw = request.to_string();
    let case: routing_cli::Case = serde_json::from_value(json!({
        "schema_version":1,"id":"synthetic-repair","endpoint":endpoint,
        "raw_body":raw,"input_bytes":raw.len(),
        "grants":{"synthetic":true,"credential_class":"batch",
            "aliases":["smart","small"],"allowed_routes":routes,"cloud_allowed":false}
    }))
    .unwrap();
    serde_json::to_value(routing_cli::explain(config, &case).unwrap()).unwrap()
}

#[test]
fn routing_repair_offline_account_ancestor_git_independence_and_serving_guard() {
    let directory = tempfile::tempdir().unwrap();
    let account = directory
        .path()
        .join("storage/nested/synthetic-account.json");
    let data = json!({"bind":"100.127.255.254","port":4000,
        "routes":[{"name":"local","kind":"local","base_url":"http://127.0.0.1:9/v1",
            "model":"synthetic","safe_concurrency":1,"context_limit":32768,"adapter_version":"synthetic"}],
        "aliases":{"small":{"context_limit":32768,"routes":[{"route":"local","priority":0}]}},
        "routing_policies":{"smart":{"default_target":"small"}},
        "codex":{"enabled":true,"alias":"cloud","model":"synthetic","auth_source":"managed","account_file":account}});
    let bytes = serde_json::to_vec(&data).unwrap();
    let before = routing_cli::parse_config(&bytes)
        .expect("offline must ignore all account ancestor metadata");
    let expected = repair_offline_explanation(&before, "chat", chat_value("hello"), &["local"]);
    std::fs::create_dir(directory.path().join(".git")).unwrap();
    // The .git is above the narrow account directory; neither input changes.
    let after = routing_cli::parse_config(&bytes);
    assert!(
        after.is_ok(),
        "offline configuration must ignore account ancestor metadata"
    );
    let after = after.unwrap();
    assert_eq!(
        repair_offline_explanation(&after, "chat", chat_value("hello"), &["local"]),
        expected
    );
    assert_eq!(
        config::validate_provider_identity(&after).unwrap_err(),
        "Codex account_file must be outside a Git worktree"
    );
    let config_path = directory.path().join("policy.json");
    std::fs::write(&config_path, &bytes).unwrap();
    assert!(
        matches!(config::load(&config_path), Err(error) if error == "Codex account_file must be outside a Git worktree")
    );
    for invalid in [
        json!({"account_file":"relative/synthetic"}),
        json!({"cli_auth_file":"/synthetic/ambiguous"}),
        json!({"responses_url":"https://unsafe.invalid/responses"}),
        json!({"model":""}),
        json!({"alias":"small"}),
    ] {
        let mut invalid_data = data.clone();
        for (key, value) in invalid.as_object().unwrap() {
            invalid_data["codex"][key] = value.clone();
        }
        assert!(routing_cli::parse_config(&serde_json::to_vec(&invalid_data).unwrap()).is_err());
    }
}

#[test]
fn routing_repair_tokenizer_reserve_preserves_alias_and_authorized_route_bounds() {
    for endpoint in ["chat", "responses"] {
        for text in ["hi".to_owned(), "x".repeat(50000)] {
            for (alias_limit, route_limit, reserve, allowed, rejected) in [
                (32768, 65536, 32769, vec!["local", "larger"], true),
                (32768, 8192, 8193, vec!["local"], true),
                (32768, 8192, 8192, vec!["local"], false),
                (32768, 8192, 8193, vec!["local", "larger"], false),
                (32768, 32768, 32768, vec!["local"], false),
            ] {
                let mut config = pure_config(json!({"default_target":"small"}));
                let alias = config.aliases.get_mut("small").unwrap();
                alias.context_limit = alias_limit;
                alias.tokenizer = Some(config::TokenizerSpec {
                    file: "/synthetic/unreadable-tokenizer".into(),
                    sha256: "0".repeat(64),
                });
                alias
                    .routes
                    .push(serde_json::from_value(json!({"route":"larger","priority":1})).unwrap());
                config.routes[0].context_limit = route_limit;
                let mut larger = config.routes[0].clone();
                larger.name = "larger".into();
                larger.context_limit = 65536;
                config.routes.push(larger);
                let mut request = if endpoint == "chat" {
                    chat_value(&text)
                } else {
                    json!({"model":"smart","input":text})
                };
                // Use the original-body one-level projection for reserve parsing too.
                let field = if endpoint == "chat" {
                    "max_completion_tokens"
                } else {
                    "max_output_tokens"
                };
                request["extra_body"] = json!({field:reserve});
                let result = repair_offline_explanation(&config, endpoint, request, &allowed);
                assert_eq!(
                    result["prediction"]["state"],
                    if rejected { "rejected" } else { "deferred" },
                    "{endpoint}: alias={alias_limit} route={route_limit} reserve={reserve} grants={allowed:?}"
                );
                if !rejected {
                    assert_eq!(result["prediction"]["target"], "small");
                    assert!(
                        result["deferred"]
                            .as_array()
                            .unwrap()
                            .contains(&json!("tokenizer_context"))
                    );
                }
            }
        }
    }
}

#[test]
fn routing_repair_tokenizer_default_reserve_and_non_context_validation() {
    for endpoint in ["chat", "responses"] {
        let request = if endpoint == "chat" {
            chat_value("hi")
        } else {
            json!({"model":"smart","input":"hi"})
        };
        let field = if endpoint == "chat" {
            "max_tokens"
        } else {
            "max_output_tokens"
        };
        for (alias_limit, route_limit, rejected) in [
            (4095, 32768, true),
            (32768, 4095, true),
            (4096, 4096, false),
        ] {
            let mut config = pure_config(json!({"default_target":"small"}));
            config.aliases.get_mut("small").unwrap().context_limit = alias_limit;
            config.aliases.get_mut("small").unwrap().tokenizer = Some(config::TokenizerSpec {
                file: "/synthetic/unreadable-tokenizer".into(),
                sha256: "0".repeat(64),
            });
            config.routes[0].context_limit = route_limit;
            let result = repair_offline_explanation(&config, endpoint, request.clone(), &["local"]);
            assert_eq!(
                result["prediction"]["state"],
                if rejected { "rejected" } else { "deferred" },
                "{endpoint} default reserve alias={alias_limit} route={route_limit}"
            );
            if !rejected {
                let mut invalid_edits = vec![
                    json!({field:"16"}),
                    json!({field:0}),
                    json!({"stream":"false"}),
                    json!({"stream":false,"extra_body":{"stream":true}}),
                    json!({"tools":"invalid"}),
                ];
                if endpoint == "chat" {
                    invalid_edits.push(json!({"chat_template_kwargs":{"enable_thinking":"false"}}));
                }
                for edit in invalid_edits {
                    let mut invalid = request.clone();
                    invalid
                        .as_object_mut()
                        .unwrap()
                        .extend(edit.as_object().unwrap().clone());
                    assert_eq!(
                        repair_offline_explanation(&config, endpoint, invalid, &["local"])["prediction"]
                            ["state"],
                        "rejected",
                        "{endpoint}: {edit}"
                    );
                }
                // A second managed extra_body stays an opaque safe extra; no recursive merge.
                let mut nested = request.clone();
                nested["extra_body"] = json!({"extra_body":{"stream":"false"}});
                assert_eq!(
                    repair_offline_explanation(&config, endpoint, nested, &["local"])["prediction"]
                        ["state"],
                    "deferred"
                );
            }
        }
    }
}
