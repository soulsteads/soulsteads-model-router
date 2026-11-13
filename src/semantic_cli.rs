use super::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read};

const HELP: &str = "slotline semantic explain CONFIG.yaml CASE.json\nslotline semantic evaluate CONFIG.yaml DATASET.json [--require-match | --min-score 0..1]\nOffline local operator tools. No credentials, tokenizers, inference, network, or mutation.\nExplicit synthetic grants are required; runtime authorization/availability remain unverified.";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Grants {
    aliases: Vec<String>,
    #[serde(default)]
    allowed_routes: Vec<String>,
    #[serde(default)]
    cloud_allowed: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Label {
    state: String,
    outcome: String,
    target: Option<String>,
}

impl Label {
    fn matches(&self, selection: &semantic::Selection) -> bool {
        self.state == selection.state
            && self.outcome == selection.outcome
            && self.target == selection.target
    }

    fn validate(&self, config: &config::Config) -> Result<(), &'static str> {
        if !["resolved", "rejected", "unresolved"].contains(&self.state.as_str())
            || ![
                "rule",
                "default",
                "model",
                "shadow",
                "classifier_not_run",
                "denied",
                "incompatible",
                "invalid",
                "low_confidence",
                "abstained",
                "timeout",
                "busy",
                "rate_limited",
                "unavailable",
                "classifier_incompatible",
                "semantic_disabled",
                "semantic_input",
                "semantic_input_limit",
                "unsupported_continuation",
                "unsupported_parameter",
            ]
            .contains(&self.outcome.as_str())
            || self.target.as_ref().is_some_and(|target| {
                !config.aliases.contains_key(target)
                    && config.codex.alias.as_ref() != Some(target)
                    && config.copilot.model_for_alias(target).is_none()
            })
        {
            return Err("invalid expected label");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Case {
    schema_version: u32,
    id: String,
    profile: String,
    endpoint: EndpointKind,
    grants: Grants,
    request: Value,
    #[serde(default)]
    recorded_classifier: Option<Value>,
    #[serde(default)]
    classifier_failure: Option<Failure>,
    #[serde(default)]
    expected: Option<Label>,
    #[serde(default)]
    expected_advisory: Option<Label>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EndpointKind {
    Chat,
    Responses,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Failure {
    Timeout,
    Busy,
    RateLimited,
    Unavailable,
    Abstained,
    Invalid,
}

impl Failure {
    fn reason(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Busy => "busy",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::Abstained => "abstained",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dataset {
    schema_version: u32,
    declared_cases: usize,
    cases: Vec<Case>,
}

#[derive(Serialize)]
struct Explanation {
    schema_version: u32,
    id: String,
    profile: String,
    endpoint: EndpointKind,
    execution: &'static str,
    policy_sha256: String,
    policy_sha256_scope: &'static str,
    policy_version: Option<String>,
    mode: semantic::Mode,
    prediction: semantic::Selection,
    advisory: Option<semantic::Selection>,
    compatibility: BTreeMap<String, &'static str>,
    authorization: &'static str,
    availability: &'static str,
    context: &'static str,
    classifier_execution: &'static str,
    unauthorized_selection: bool,
    expected: Option<Label>,
    expected_advisory: Option<Label>,
    matched: Option<bool>,
}

fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &str, limit: usize) -> Result<T, &'static str> {
    if path.contains("://") {
        return Err("local files required");
    }
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "input read failed")?;
    if !file.metadata().map_err(|_| "input read failed")?.is_file() {
        return Err("regular file required");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "input read failed")?;
    if bytes.len() > limit {
        return Err("input size limit exceeded");
    }
    if bytes
        .split(|byte| *byte == b'\n')
        .any(|line| line.len() > 65536)
    {
        return Err("input line limit exceeded");
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid input schema")
}

fn explain(config: &config::Config, case: Case) -> Result<Explanation, &'static str> {
    if case.schema_version != 1
        || !identifier(&case.id)
        || !identifier(&case.profile)
        || case.grants.aliases.len() > 64
        || case.grants.allowed_routes.len() > 128
        || case
            .grants
            .aliases
            .iter()
            .chain(&case.grants.allowed_routes)
            .any(|name| !identifier(name))
        || case.recorded_classifier.is_some() && case.classifier_failure.is_some()
        || !case.request.is_object()
        || case.request["model"] != case.profile
        || serde_json::to_vec(&case).map_err(|_| "invalid case")?.len() > 65536
        || case
            .recorded_classifier
            .as_ref()
            .is_some_and(|payload| payload.to_string().len() > 4096)
    {
        return Err("invalid case or bounds");
    }
    let aliases = case
        .grants
        .aliases
        .iter()
        .collect::<std::collections::HashSet<_>>();
    let routes = case
        .grants
        .allowed_routes
        .iter()
        .collect::<std::collections::HashSet<_>>();
    if aliases.len() != case.grants.aliases.len()
        || routes.len() != case.grants.allowed_routes.len()
        || aliases.iter().any(|name| {
            !config.aliases.contains_key(*name)
                && !config.semantic_routing.profiles.contains_key(*name)
                && config.codex.alias.as_ref() != Some(*name)
                && config.copilot.model_for_alias(name).is_none()
        })
        || routes.iter().any(|name| {
            !["codex", "copilot"].contains(&name.as_str())
                && !config.routes.iter().any(|route| &route.name == *name)
        })
    {
        return Err("duplicate or unknown synthetic grant");
    }
    for label in [&case.expected, &case.expected_advisory]
        .into_iter()
        .flatten()
    {
        label.validate(config)?;
    }
    let profile = config
        .semantic_routing
        .profiles
        .get(&case.profile)
        .ok_or("unknown profile")?;
    let credential = config::Credential {
        name: "offline-synthetic".into(),
        key_file: None,
        aliases: case.grants.aliases,
        allowed_routes: case.grants.allowed_routes,
        cloud_allowed: case.grants.cloud_allowed,
        class: "offline".into(),
        requests_per_minute: 1,
        burst: 1,
        max_in_flight: 1,
        operator: false,
        agent_affinity: false,
        expires_at: None,
    };
    let rules = semantic::compile_rules(profile).map_err(|_| "invalid rules")?;
    let inspection = semantic::inspect_request(&case.request).map_err(|_| "unsupported_parameter");
    let text = inspection
        .as_ref()
        .map_err(|reason| *reason)
        .and_then(|request| {
            semantic::latest_text(
                request,
                matches!(case.endpoint, EndpointKind::Responses),
                config.semantic_routing.max_input_bytes,
            )
        });
    let tokenizers = HashMap::new();
    let context = semantic::Context {
        config,
        credential: &credential,
        profile,
        rules: &rules,
        request: &case.request,
        responses: matches!(case.endpoint, EndpointKind::Responses),
        tokenizers: &tokenizers,
    };
    let mut compatibility = BTreeMap::new();
    for target in &profile.targets {
        if semantic::allowed_alias(config, &credential, target) {
            compatibility.insert(target.clone(), context.compatibility(target));
        }
    }
    let result = if !semantic::is_selector(config, &case.profile) {
        semantic::Selection::new("rejected", "semantic_disabled", None)
    } else if !credential.aliases.contains(&case.profile)
        || !semantic::allowed_alias(config, &credential, &profile.default_target)
    {
        semantic::Selection::new("rejected", "denied", None)
    } else {
        match &text {
            Err(reason) => semantic::Selection::new("rejected", reason, None),
            Ok(text) => context.resolve(
                text,
                match (&case.recorded_classifier, case.classifier_failure) {
                    (Some(payload), _) => semantic::ModelResult::Recorded(payload),
                    (_, Some(failure)) => semantic::ModelResult::Failure(failure.reason()),
                    _ => semantic::ModelResult::NotRun,
                },
            ),
        }
    };
    let (prediction, advisory) = if profile.mode == semantic::Mode::Shadow
        && text.is_ok()
        && semantic::is_selector(config, &case.profile)
        && credential.aliases.contains(&case.profile)
        && semantic::allowed_alias(config, &credential, &profile.default_target)
    {
        (
            semantic::Selection::new("resolved", "shadow", Some(&profile.default_target)),
            Some(result),
        )
    } else {
        (result, None)
    };
    let matched = case.expected.as_ref().map(|expected| {
        expected.matches(&prediction)
            && case.expected_advisory.as_ref().is_none_or(|expected| {
                advisory
                    .as_ref()
                    .is_some_and(|selection| expected.matches(selection))
            })
    });
    let unauthorized_selection = prediction
        .target
        .as_ref()
        .is_some_and(|target| !semantic::allowed_alias(config, &credential, target));
    Ok(Explanation {
        schema_version: 1,
        id: case.id,
        profile: case.profile,
        endpoint: case.endpoint,
        execution: "offline_plan_no_runtime_decision",
        policy_sha256: audit_identity::digest(config),
        policy_sha256_scope: "static_projection_runtime_key_store_grants_not_loaded",
        policy_version: profile.policy_version.clone(),
        mode: profile.mode,
        prediction,
        advisory,
        compatibility,
        authorization: "synthetic_grants_only_runtime_authorization_unverified",
        availability: "runtime_health_admission_and_provider_state_unverified",
        context: "configured_tokenizers_not_loaded_context_may_be_deferred_existing_target_validation_is_authoritative",
        classifier_execution: "not_run_recordings_only",
        unauthorized_selection,
        expected: case.expected,
        expected_advisory: case.expected_advisory,
        matched,
    })
}

fn execute(args: &[String]) -> Result<bool, &'static str> {
    if args == ["--help"] {
        println!("{HELP}");
        return Ok(true);
    }
    if args.len() < 3 || !["explain", "evaluate"].contains(&args[0].as_str()) {
        return Err("invalid arguments; use semantic --help");
    }
    if args[1].contains("://") {
        return Err("local files required");
    }
    let config =
        config::load_static(Path::new(&args[1])).map_err(|_| "invalid operator configuration")?;
    if args[0] == "explain" {
        if args.len() != 3 {
            return Err("invalid explain arguments");
        }
        let explanation = explain(&config, read_json(&args[2], 65536)?)?;
        println!(
            "{}",
            serde_json::to_string(&explanation).map_err(|_| "serialization failed")?
        );
        return Ok(true);
    }
    let minimum = match &args[3..] {
        [] => 0.0,
        [flag] if flag == "--require-match" => 1.0,
        [flag, score] if flag == "--min-score" => {
            score.parse::<f64>().map_err(|_| "invalid minimum score")?
        }
        _ => return Err("invalid evaluate arguments"),
    };
    if !minimum.is_finite() || !(0.0..=1.0).contains(&minimum) {
        return Err("invalid minimum score");
    }
    let dataset: Dataset = read_json(&args[2], 8 * 1024 * 1024)?;
    if dataset.schema_version != 1
        || dataset.cases.is_empty()
        || dataset.cases.len() > 1000
        || dataset.declared_cases != dataset.cases.len()
    {
        return Err("invalid dataset version or declared count");
    }
    let mut ids = std::collections::HashSet::new();
    let mut results = Vec::new();
    for case in dataset.cases {
        if !ids.insert(case.id.clone()) || case.expected.is_none() {
            return Err("duplicate id or missing expected label");
        }
        results.push(explain(&config, case)?);
    }
    let total = results.len();
    let matched = results
        .iter()
        .filter(|result| result.matched == Some(true))
        .count();
    let selected = results
        .iter()
        .filter(|result| result.prediction.target.is_some())
        .count();
    let route_matches = results
        .iter()
        .filter(|result| {
            result
                .expected
                .as_ref()
                .is_some_and(|label| label.target == result.prediction.target)
        })
        .count();
    let selected_matches = results
        .iter()
        .filter(|result| {
            result.prediction.target.is_some()
                && result
                    .expected
                    .as_ref()
                    .is_some_and(|label| label.target == result.prediction.target)
        })
        .count();
    let rejected = results
        .iter()
        .filter(|result| result.prediction.state == "rejected")
        .count();
    let unresolved = results
        .iter()
        .filter(|result| result.prediction.state == "unresolved")
        .count();
    let unsafe_selections = results
        .iter()
        .filter(|result| result.unauthorized_selection)
        .count();
    let advisory_divergences = results
        .iter()
        .filter(|result| {
            result.advisory.as_ref().is_some_and(|advisory| {
                advisory.target.is_some() && advisory.target != result.prediction.target
            })
        })
        .count();
    let mut confusion: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut outcomes: BTreeMap<&str, usize> = BTreeMap::new();
    for result in &results {
        let expected = result
            .expected
            .as_ref()
            .expect("required label")
            .target
            .as_deref()
            .unwrap_or("<none>");
        let predicted = result.prediction.target.as_deref().unwrap_or("<none>");
        *confusion
            .entry(expected.into())
            .or_default()
            .entry(predicted.into())
            .or_default() += 1;
        *outcomes.entry(result.prediction.outcome).or_default() += 1;
    }
    let score = matched as f64 / total as f64;
    println!(
        "{}",
        json!({"schema_version":1,"cases":results,"aggregate":{
        "total":total,"matched":matched,"mismatched":total-matched,"selected":selected,"not_selected":total-selected,
        "match_score":score,"route_selection_accuracy":route_matches as f64 / total as f64,
        "route_selection_coverage":selected as f64 / total as f64,"route_matches":route_matches,
        "selected_route_accuracy":(selected > 0).then(|| selected_matches as f64 / selected as f64),
        "rejected":rejected,"unresolved":unresolved,"unauthorized_selections":unsafe_selections,
        "advisory_divergences":advisory_divergences,
        "confusion":confusion,"outcomes":outcomes,"minimum_score":minimum,"passed":score>=minimum,
        "metric_scope":"synthetic_fixture_label_agreement_not_answer_quality_cost_latency_or_calibrated_probability"}})
    );
    Ok(score >= minimum)
}

pub(crate) fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match execute(args) {
        Ok(true) => Ok(()),
        Ok(false) => Err("semantic evaluation requirements not met".into()),
        Err(reason) => {
            println!("{}", json!({"schema_version":1,"error":reason}));
            Err("semantic command failed".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_v3_offline_codex_compatibility_matches_provider_projection() {
        let mut value: Value =
            serde_yaml::from_str(include_str!("../tests/fixtures/semantic-policy-v1.yaml"))
                .unwrap();
        value["codex"] = json!({"enabled":true,"alias":"cloud","model":"synthetic"});
        value["semantic_routing"]["profiles"]["preflight"]["targets"] = json!(["small", "cloud"]);
        value["semantic_routing"]["profiles"]["preflight"]["rules"] =
            json!([{"id":"code","priority":1,"target":"cloud","regex":"code|debug"}]);
        for default in [false, true] {
            value["semantic_routing"]["profiles"]["preflight"]["default_target"] =
                json!(if default { "cloud" } else { "small" });
            let config: config::Config = serde_json::from_value(value.clone()).unwrap();
            for responses in [false, true] {
                for depth in 0..=2 {
                    for mut payload in [json!({}), json!({"stream":false})] {
                        let mut request = if responses {
                            json!({"model":"preflight","input":"hello"})
                        } else {
                            json!({"model":"preflight","messages":[{"role":"user","content":"hello"}]})
                        };
                        for _ in 0..depth {
                            payload = json!({"extra_body":payload});
                        }
                        request
                            .as_object_mut()
                            .unwrap()
                            .extend(payload.as_object().unwrap().clone());
                        let original = request.clone();
                        let mut projected = request.clone();
                        normalize_extra_body(&mut projected, false).unwrap();
                        let compatible = if responses {
                            codex::native_request(&projected).is_ok()
                        } else {
                            codex::chat_request(&projected)
                                .is_ok_and(|body| codex::native_request(&body).is_ok())
                        };
                        assert_eq!(compatible, depth < 2);
                        let case: Case = serde_json::from_value(json!({
                            "schema_version":1,"id":"projection","profile":"preflight",
                            "endpoint":if responses { "responses" } else { "chat" },
                            "grants":{"aliases":["preflight","small","cloud","judge"],"allowed_routes":["native","codex"],"cloud_allowed":true},
                            "request":request
                        })).unwrap();
                        let result = explain(&config, case).unwrap();
                        assert_eq!(
                            result.compatibility["cloud"] != "incompatible",
                            compatible,
                            "{request}"
                        );
                        if !compatible && default {
                            assert_eq!(result.prediction.outcome, "incompatible");
                            assert!(result.prediction.target.is_none());
                        } else {
                            assert_eq!(
                                result.prediction.candidates.contains(&"cloud".to_owned()),
                                compatible
                            );
                        }
                        assert_eq!(request, original);
                    }
                }
            }
        }
    }

    #[test]
    fn repair_v3_offline_managed_projection_retains_thinking_rewrite() {
        let mut config =
            config::load_static(Path::new("tests/fixtures/semantic-policy-v1.yaml")).unwrap();
        config.aliases.get_mut("small").unwrap().streaming = false;
        for responses in [false, true] {
            for depth in 0..=2 {
                for mut payload in [
                    json!({"enable_thinking":false}),
                    json!({"enable_thinking":"invalid"}),
                    json!({"stream":true}),
                ] {
                    let mut request = if responses {
                        json!({"model":"preflight","input":"hello"})
                    } else {
                        json!({"model":"preflight","messages":[{"role":"user","content":"hello"}]})
                    };
                    for _ in 0..depth {
                        payload = json!({"extra_body":payload});
                    }
                    request
                        .as_object_mut()
                        .unwrap()
                        .extend(payload.as_object().unwrap().clone());
                    if depth == 2 && payload.to_string().contains("enable_thinking") {
                        request["enable_thinking"] = json!(true);
                    }
                    let mut projected = request.clone();
                    normalize_extra_body(&mut projected, true).unwrap();
                    let alias = &config.aliases["small"];
                    let compatible = if responses {
                        validate_responses_request(&projected, alias, None, true)
                            .is_ok_and(|(stream, ..)| !stream)
                    } else {
                        validate_request(&projected, alias, None, true)
                            .is_ok_and(|(stream, ..)| !stream)
                    };
                    let case: Case = serde_json::from_value(json!({
                        "schema_version":1,"id":"managed-projection","profile":"preflight",
                        "endpoint":if responses { "responses" } else { "chat" },
                        "grants":{"aliases":["preflight","small","large","judge"],"allowed_routes":["native"]},
                        "request":request
                    })).unwrap();
                    let result = explain(&config, case).unwrap();
                    assert_eq!(
                        result.compatibility["small"] != "incompatible",
                        compatible,
                        "{request}"
                    );
                    assert_eq!(
                        result.prediction.outcome,
                        if compatible {
                            "classifier_not_run"
                        } else {
                            "incompatible"
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn strict_versions_counts_and_errors() {
        assert!(
            serde_json::from_value::<Dataset>(
                json!({"schema_version":1,"declared_cases":0,"cases":[],"url":"https://invalid"})
            )
            .is_err()
        );
        for value in ["NaN", "-1", "1.1"] {
            assert!(
                !value
                    .parse::<f64>()
                    .is_ok_and(|score| score.is_finite() && (0.0..=1.0).contains(&score))
            );
        }
        assert!(read_json::<Value>("https://invalid/private", 1).is_err());
    }

    #[test]
    fn checked_in_synthetic_labels_and_counts_match_shared_engine() {
        let config =
            config::load_static(Path::new("tests/fixtures/semantic-policy-v1.yaml")).unwrap();
        let dataset: Dataset =
            serde_json::from_str(include_str!("../tests/fixtures/semantic-v1.json")).unwrap();
        assert_eq!(dataset.schema_version, 1);
        assert_eq!(dataset.declared_cases, dataset.cases.len());
        let mut ids = std::collections::HashSet::new();
        for case in dataset.cases {
            assert!(ids.insert(case.id.clone()));
            let result = explain(&config, case).unwrap();
            assert_eq!(
                result.matched,
                Some(true),
                "{}: {}",
                result.id,
                serde_json::to_string(&result).unwrap()
            );
        }
    }
}
