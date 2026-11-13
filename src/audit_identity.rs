use crate::config::{Config, CopilotEndpoint, Endpoint, MatcherSource, RouteKind};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const BUILD: &str = env!("SLOTLINE_SOURCE_SHA256");
pub(crate) fn projection(config: &Config) -> Value {
    let routes: Vec<_> = config.routes.iter().map(|route| {
        let destination = reqwest::Url::parse(&route.base_url).ok().map(|url| json!([url.scheme(), url.host_str(), url.port_or_known_default(), url.path()]));
        json!({"name":route.name,"kind":match route.kind {RouteKind::Local=>"local",RouteKind::Headroom=>"headroom",RouteKind::Embedding=>"embedding"},"destination":destination,"health_path":route.health_path,"model":route.model,"physical_route":route.physical_route,"safe_concurrency":route.safe_concurrency,"context_limit":route.context_limit,"adapter_version":route.adapter_version,"capabilities":route.capabilities,"streaming":route.streaming,"responses_done_tail":route.responses_done_tail,"disable_thinking":route.disable_thinking,"timeout_seconds":route.timeout_seconds,"enabled":route.enabled})
    }).collect();
    let aliases: serde_json::Map<String, Value> = config.aliases.iter().map(|(name, alias)| {
        let routes: Vec<_> = alias.routes.iter().map(|route| json!({"route":route.route,"priority":route.priority,"fallback_only":route.fallback_only,"enabled":route.enabled})).collect();
        (name.clone(),json!({"context_limit":alias.context_limit,"endpoint":match alias.endpoint {Endpoint::Chat=>"chat",Endpoint::Embedding=>"embedding"},"passthrough_route":alias.passthrough_route,"routes":routes,"capabilities":alias.capabilities,"streaming":alias.streaming}))
    }).collect();
    let routing_policies: serde_json::Map<String, Value> = config.routing_policies.iter().map(|(name, policy)| {
        let rules: Vec<_> = policy.rules.iter().map(|rule| json!({"id":rule.id,"target":rule.target,"when":{"credential_classes":rule.predicate.credential_classes,"endpoints":rule.predicate.endpoints.iter().map(|endpoint| match endpoint {crate::config::RoutingEndpoint::Chat=>"chat",crate::config::RoutingEndpoint::Responses=>"responses"}).collect::<Vec<_>>(),"stream":rule.predicate.stream,"tools":rule.predicate.tools,"image":rule.predicate.image,"min_input_bytes":rule.predicate.min_input_bytes,"max_input_bytes":rule.predicate.max_input_bytes}})).collect();
        (name.clone(), json!({"rules":rules,"decision_model":policy.decision_model,"decision_targets":policy.decision_targets,"decision_timeout_seconds":policy.decision_timeout_seconds,"decision_prompt_sha256":format!("{:x}", Sha256::digest(policy.decision_prompt.as_bytes())),"default_target":policy.default_target}))
    }).collect();
    let budgets: serde_json::Map<String, Value> = config.class_timeout_budgets.iter().map(|(name,budget)| (name.clone(),json!({"attempt_seconds":budget.attempt_seconds,"total_seconds":budget.total_seconds,"min_retry_seconds":budget.min_retry_seconds,"first_output_seconds":budget.first_output_seconds,"stream_idle_seconds":budget.stream_idle_seconds}))).collect();
    let credentials: Vec<_> = config.credentials.iter().map(|credential| json!({"class":credential.class,"aliases":credential.aliases,"allowed_routes":credential.allowed_routes,"cloud_allowed":credential.cloud_allowed,"requests_per_minute":credential.requests_per_minute,"burst":credential.burst,"max_in_flight":credential.max_in_flight,"agent_affinity":credential.agent_affinity,"operator":credential.operator,"expires_at":credential.expires_at})).collect();
    let matchers: Vec<_> = config.metadata_matchers.iter().map(|matcher| json!({"class":matcher.class,"source":match matcher.source {MatcherSource::UserAgent=>"user_agent",MatcherSource::CredentialName=>"credential_name"}})).collect();
    let copilot_models: serde_json::Map<String, Value> = config.copilot.models.iter().map(|model| {
        (model.alias.clone(), json!({"model":model.model,"endpoint":match model.endpoint {CopilotEndpoint::ChatCompletions=>"chat_completions",CopilotEndpoint::Responses=>"responses"}}))
    }).collect();
    json!({"schema":4,"hindsight":{"enabled":config.hindsight.enabled,"endpoint":config.hindsight.endpoint,"timeout_ms":config.hindsight.timeout_ms,"bindings":config.hindsight.bindings},"codex":{"enabled":config.codex.enabled,"alias":config.codex.alias,"model":config.codex.model,"auth_base_url":config.codex.auth_base_url(),"responses_url":config.codex.responses_url()},"copilot":{"enabled":config.copilot.enabled,"models":copilot_models},"routes":routes,"aliases":aliases,"routing_policies":routing_policies,"class_timeout_budgets":budgets,"retry":{"max_attempts":config.retry_policy.max_attempts,"total_seconds":config.retry_policy.total_seconds},"refresh_seconds":config.refresh_seconds,"affinity_lease_seconds":config.affinity_lease_seconds,"credentials":credentials,"matcher_categories":matchers})
}
pub(crate) fn digest(config: &Config) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&projection(config)).expect("nonsecret policy"))
    )
}
pub(crate) fn status(runtime: &crate::Runtime) -> Value {
    json!({"build_source_sha256":BUILD,"build_source_revision":env!("SLOTLINE_SOURCE_REVISION"),"build_source_state":env!("SLOTLINE_SOURCE_STATE"),"policy_sha256":runtime.policy_digest,"policy_generation":runtime.policy_generation,"sink":crate::audit_sink::COUNTERS.snapshot(),"metrics":runtime.audit_metrics.snapshot()})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(json!({
            "bind":"100.127.255.254", "port":4000, "routes":[], "aliases":{},
            "copilot":{"models":[
                {"alias":"chat", "model":"synthetic-chat", "endpoint":"chat_completions"},
                {"alias":"responses", "model":"synthetic-responses", "endpoint":"responses"}
            ]}
        }))
        .unwrap()
    }

    #[test]
    fn copilot_identity_tracks_enablement_model_and_endpoint() {
        let original = config();
        for edit in ["enabled", "model", "endpoint", "alias"] {
            let mut candidate = original.clone();
            match edit {
                "enabled" => candidate.copilot.enabled = true,
                "model" => candidate.copilot.models[0].model = "synthetic-new".into(),
                "alias" => candidate.copilot.models[0].alias = "new-alias".into(),
                _ => {
                    candidate.copilot.models[0].endpoint = crate::config::CopilotEndpoint::Responses
                }
            }
            assert_ne!(digest(&original), digest(&candidate), "{edit}");
        }
    }

    #[test]
    fn copilot_identity_excludes_storage_and_credentials_and_canonicalizes_mappings() {
        let original = config();
        let mut candidate = original.clone();
        candidate.copilot.auth_source = crate::config::CopilotAuthSource::OAuthTokenFile;
        candidate.copilot.oauth_token_file = Some("PRIVATE_OAUTH_PATH_AND_CONTENT".into());
        candidate.copilot.managed_auth_file = Some("PRIVATE_MANAGED_PATH_AND_CONTENT".into());
        candidate.copilot.models.reverse();
        assert_eq!(digest(&original), digest(&candidate));
        let encoded = projection(&candidate).to_string();
        assert!(!encoded.contains("PRIVATE"));
        assert_eq!(
            projection(&original)["copilot"]["models"]["chat"]["model"],
            "synthetic-chat"
        );
    }
}
