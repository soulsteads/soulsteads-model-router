//! Offline selection fixtures; deliberately separate from reviewed outcome quality.
use crate::{config, model_routing, routing_plan};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Case {
    schema_version: u32,
    id: String,
    endpoint: config::RoutingEndpoint,
    grants: Grants,
    raw_body: String,
    input_bytes: usize,
    #[serde(default)]
    judge_response: Option<Value>,
    #[serde(default)]
    expected: Option<Prediction>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grants {
    synthetic: bool,
    credential_class: String,
    aliases: Vec<String>,
    allowed_routes: Vec<String>,
    cloud_allowed: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Prediction {
    state: String,
    outcome: String,
    target: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dataset {
    schema_version: u32,
    declared_cases: usize,
    cases: Vec<Case>,
}
#[derive(Serialize)]
pub(crate) struct Explanation {
    id: String,
    prediction: Prediction,
    // A proposed target is never a claim of live availability or successful inference.
    deferred: Vec<&'static str>,
    candidates: Vec<String>,
    advisory: Option<String>,
    policy_sha256: Option<String>,
    matched: Option<bool>,
    authorization_assertion: bool,
}
fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= 128 && !label.chars().any(char::is_control)
}
fn unique(items: &[String]) -> bool {
    items.len() <= 256
        && items.iter().all(|item| valid_label(item))
        && items.iter().collect::<HashSet<_>>().len() == items.len()
}
fn credential(grants: &Grants) -> config::Credential {
    config::Credential {
        name: "synthetic-offline".into(),
        key_file: None,
        aliases: grants.aliases.clone(),
        allowed_routes: grants.allowed_routes.clone(),
        class: grants.credential_class.clone(),
        cloud_allowed: grants.cloud_allowed,
        requests_per_minute: 1,
        burst: 1,
        max_in_flight: 1,
        operator: false,
        agent_affinity: false,
        expires_at: None,
    }
}
// Defer tokenizer-dependent prompt sizing while retaining all known bounds.
fn static_fit(
    config: &config::Config,
    credential: &config::Credential,
    target: &str,
    endpoint: config::RoutingEndpoint,
    request: &Value,
) -> bool {
    routing_plan::request_fits_offline(config, credential, target, endpoint, request)
}
fn deferred_facts(config: &config::Config, targets: &[String]) -> Vec<&'static str> {
    let mut facts = vec!["runtime_availability"];
    if targets.iter().any(|target| {
        config
            .aliases
            .get(target)
            .is_some_and(|alias| alias.tokenizer.is_some())
    }) {
        facts.push("tokenizer_context");
    }
    if targets.iter().any(|target| {
        config.codex.alias.as_deref() == Some(target)
            || config.copilot.model_for_alias(target).is_some()
    }) {
        facts.push("provider_account_network");
    }
    if targets
        .iter()
        .any(|target| config.copilot.model_for_alias(target).is_some())
    {
        facts.push("copilot_request_capabilities");
    }
    facts
}
pub(crate) fn explain(config: &config::Config, case: &Case) -> Result<Explanation, &'static str> {
    if case.schema_version != 1
        || !valid_label(&case.id)
        || !case.grants.synthetic
        || !valid_label(&case.grants.credential_class)
        || !unique(&case.grants.aliases)
        || !unique(&case.grants.allowed_routes)
        || case.raw_body.len() > MAX_BYTES
        || case.input_bytes != case.raw_body.len()
        || case
            .judge_response
            .as_ref()
            .is_some_and(|value| value.to_string().len() > 16384)
        || case.grants.aliases.iter().any(|name| {
            !config.aliases.contains_key(name)
                && !config.routing_policies.contains_key(name)
                && config.codex.alias.as_deref() != Some(name)
                && config.copilot.model_for_alias(name).is_none()
        })
        || case.grants.allowed_routes.iter().any(|name| {
            name != "codex"
                && name != "copilot"
                && !config.routes.iter().any(|route| route.name == *name)
        })
    {
        return Err("invalid selection case");
    }
    if let Some(expected) = &case.expected
        && (!matches!(expected.state.as_str(), "deferred" | "rejected")
            || !matches!(
                expected.outcome.as_str(),
                "rule"
                    | "default"
                    | "shadow"
                    | "fixture_decision"
                    | "judge_not_run"
                    | "invalid_request"
                    | "unsupported_parameter"
                    | "invalid_decision"
                    | "model_not_allowed"
                    | "decision_not_allowed"
                    | "decision_input_too_large"
                    | "no_routing_target"
                    | "incompatible"
            )
            || expected
                .target
                .as_ref()
                .is_some_and(|target| !valid_label(target)))
    {
        return Err("invalid selection expectation");
    }
    let credential = credential(&case.grants);
    let request: Value =
        serde_json::from_str(&case.raw_body).map_err(|_| "invalid request JSON")?;
    let requested = request["model"].as_str().ok_or("invalid request model")?;
    if !config.routing_policies.contains_key(requested) {
        return Err("unknown routing policy");
    }
    let mut candidates = Vec::new();
    let mut advisory = None;
    let mut facts = Vec::new();
    let result =
        routing_plan::inspect(&request, case.endpoint, case.input_bytes).and_then(|inspection| {
            routing_plan::plan(config, &credential, requested, &inspection, |target| {
                static_fit(config, &credential, target, case.endpoint, &request)
            })
        });
    let prediction = match result {
        Ok(routing_plan::Plan::Selected {
            target,
            reason,
            advisory: advice,
            ..
        }) => {
            if case.judge_response.is_some() {
                return Err("unused judge response");
            }
            if !static_fit(config, &credential, &target, case.endpoint, &request) {
                Prediction {
                    state: "rejected".into(),
                    outcome: "incompatible".into(),
                    target: None,
                }
            } else {
                facts = deferred_facts(config, std::slice::from_ref(&target));
                advisory = advice;
                Prediction {
                    state: "deferred".into(),
                    outcome: reason.into(),
                    target: Some(target),
                }
            }
        }
        Ok(routing_plan::Plan::Judge {
            candidates: eligible,
        }) => {
            candidates = eligible;
            facts = deferred_facts(config, &candidates);
            // Judge input validation, tokenizer context and admission are runtime facts too.
            facts.push("judge_runtime");
            if let Some(judge) = config.routing_policies[requested].decision_model.as_ref()
                && config
                    .aliases
                    .get(judge)
                    .is_some_and(|alias| alias.tokenizer.is_some())
                && !facts.contains(&"tokenizer_context")
            {
                facts.push("tokenizer_context");
            }
            if let Some(payload) = &case.judge_response {
                match routing_plan::parse_decision(payload, &candidates) {
                    Ok(target) => Prediction {
                        state: "deferred".into(),
                        outcome: "fixture_decision".into(),
                        target: Some(target),
                    },
                    Err(code) => Prediction {
                        state: "rejected".into(),
                        outcome: code.into(),
                        target: None,
                    },
                }
            } else {
                facts.push("judge_not_run");
                Prediction {
                    state: "deferred".into(),
                    outcome: "judge_not_run".into(),
                    target: None,
                }
            }
        }
        Err(code) => {
            if case.judge_response.is_some() {
                return Err("unused judge response");
            }
            Prediction {
                state: "rejected".into(),
                outcome: code.into(),
                target: None,
            }
        }
    };
    let authorized = prediction
        .target
        .iter()
        .chain(advisory.iter())
        .chain(candidates.iter())
        .all(|target| routing_plan::target_usable(config, &credential, target, case.endpoint));
    if !authorized {
        return Err("selection authorization assertion failed");
    }
    let matched = case
        .expected
        .as_ref()
        .map(|expected| *expected == prediction);
    Ok(Explanation {
        id: case.id.clone(),
        prediction,
        deferred: facts,
        candidates,
        advisory,
        policy_sha256: Some(model_routing::policy_hash(config, requested)),
        matched,
        authorization_assertion: authorized,
    })
}
fn read_regular(path: &Path) -> Result<Vec<u8>, &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "input unavailable")?;
    if !file.metadata().map_err(|_| "input unavailable")?.is_file() {
        return Err("regular file required");
    }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "input unreadable")?;
    if bytes.len() > MAX_BYTES {
        return Err("input exceeds size limit");
    }
    Ok(bytes)
}
pub(crate) fn parse_config(bytes: &[u8]) -> Result<config::Config, &'static str> {
    if bytes.len() > MAX_BYTES {
        return Err("input exceeds size limit");
    }
    let config: config::Config =
        serde_yaml::from_slice(bytes).map_err(|_| "invalid configuration")?;
    config::validate_provider_config_pure(&config).map_err(|_| "invalid configuration")?;
    let alias_re = regex::Regex::new("^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$").expect("constant regex");
    config::validate_routing_policies(&config, &alias_re)
        .map_err(|_| "invalid routing configuration")?;
    let routes: HashSet<_> = config.routes.iter().map(|route| &route.name).collect();
    if routes.len() != config.routes.len()
        || config.aliases.values().any(|alias| {
            alias
                .routes
                .iter()
                .any(|entry| !routes.contains(&entry.route))
        })
    {
        return Err("invalid route references");
    }
    Ok(config)
}
fn execute(args: &[String]) -> Result<(Value, bool), &'static str> {
    let [command, config_path, input_path, options @ ..] = args else {
        return Err(
            "usage: routing explain CONFIG CASE | routing evaluate-selection CONFIG DATASET [--require-match] [--min-score SCORE]",
        );
    };
    let mut min_score = 0.0;
    let mut require_match = false;
    let mut remaining = options;
    while !remaining.is_empty() {
        match remaining {
            [option, rest @ ..] if option == "--require-match" => {
                require_match = true;
                remaining = rest;
            }
            [option, value, rest @ ..] if option == "--min-score" => {
                min_score = value
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
                    .ok_or("invalid score threshold")?;
                remaining = rest;
            }
            _ => return Err("invalid selection options"),
        }
    }
    let config = parse_config(&read_regular(Path::new(config_path))?)?;
    let bytes = read_regular(Path::new(input_path))?;
    if command == "explain" {
        if !options.is_empty() {
            return Err("explain does not accept evaluation options");
        }
        let case: Case = serde_json::from_slice(&bytes).map_err(|_| "invalid selection case")?;
        return Ok((
            serde_json::to_value(explain(&config, &case)?).map_err(|_| "report unavailable")?,
            true,
        ));
    }
    if command != "evaluate-selection" {
        return Err("unknown selection command");
    }
    let dataset: Dataset =
        serde_json::from_slice(&bytes).map_err(|_| "invalid selection dataset")?;
    if dataset.schema_version != 1
        || dataset.cases.is_empty()
        || dataset.cases.len() > 4096
        || dataset.declared_cases != dataset.cases.len()
    {
        return Err("invalid selection dataset");
    }
    let mut ids = HashSet::new();
    let mut results = Vec::new();
    let mut outcomes = BTreeMap::<String, usize>::new();
    let mut matched = 0;
    let mut proposed = 0;
    for case in dataset.cases {
        if case.expected.is_none() || !ids.insert(case.id.clone()) {
            return Err("missing expectation or duplicate case");
        }
        let result = explain(&config, &case)?;
        matched += usize::from(result.matched == Some(true));
        proposed += usize::from(result.prediction.target.is_some());
        *outcomes
            .entry(result.prediction.outcome.clone())
            .or_default() += 1;
        results.push(result);
    }
    let total = results.len();
    let score = matched as f64 / total as f64;
    let pass = score >= min_score && (!require_match || matched == total);
    Ok((
        json!({"schema_version":1,"kind":"routing_selection","cases":results,
        "aggregate":{"total":total,"matched":matched,"mismatched":total-matched,
            "expectation_match_rate":score,"planned_target_coverage":proposed as f64 / total as f64,
            "unauthorized_selections":0,"outcomes":outcomes},"threshold_passed":pass}),
        pass,
    ))
}
pub(crate) fn run_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match execute(args) {
        Ok((value, pass)) => {
            println!("{}", serde_json::to_string(&value)?);
            if pass {
                Ok(())
            } else {
                Err("selection threshold failed".into())
            }
        }
        Err(code) => {
            println!("{}", json!({"error":code}));
            Err("offline selection failed".into())
        }
    }
}
