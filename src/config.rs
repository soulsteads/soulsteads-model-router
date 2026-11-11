use crate::key_manager::{self, AuthSnapshot};
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::IpAddr,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
};
use tokenizers::Tokenizer;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    #[serde(default = "default_refresh")]
    pub refresh_seconds: u64,
    #[serde(default = "default_affinity_lease")]
    pub affinity_lease_seconds: u64,
    #[serde(default)]
    pub codex: CodexConfig,
    #[serde(default)]
    pub copilot: CopilotConfig,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub key_store: Option<String>,
    pub routes: Vec<Route>,
    pub aliases: HashMap<String, Alias>,
    #[serde(default)]
    pub class_timeout_budgets: HashMap<String, ClassTimeoutBudget>,
    #[serde(default)]
    pub retry_policy: RetryPolicy,
    #[serde(default)]
    pub metadata_matchers: Vec<Matcher>,
}
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub account_file: Option<String>,
    #[serde(default)]
    pub auth_source: CodexAuthSource,
    #[serde(default)]
    pub cli_auth_file: Option<String>,
    #[serde(default)]
    pub auth_base_url: Option<String>,
    #[serde(default)]
    pub responses_url: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexAuthSource {
    #[default]
    Auto,
    CodexCli,
    Managed,
}

impl CodexConfig {
    pub(crate) fn resolved_auth_source(&self) -> Result<CodexAuthSource, String> {
        match self.auth_source {
            CodexAuthSource::Auto => {
                match (self.account_file.is_some(), self.cli_auth_file.is_some()) {
                    (true, false) => Ok(CodexAuthSource::Managed),
                    (false, _) => Ok(CodexAuthSource::CodexCli),
                    (true, true) => Err("Codex auth source/path combination is ambiguous".into()),
                }
            }
            CodexAuthSource::CodexCli if self.account_file.is_none() => {
                Ok(CodexAuthSource::CodexCli)
            }
            CodexAuthSource::Managed
                if self.account_file.is_some() && self.cli_auth_file.is_none() =>
            {
                Ok(CodexAuthSource::Managed)
            }
            _ => Err("Codex auth source/path combination is invalid".into()),
        }
    }

    pub(crate) fn auth_base_url(&self) -> &str {
        self.auth_base_url
            .as_deref()
            .unwrap_or("https://auth.openai.com")
    }

    pub(crate) fn responses_url(&self) -> &str {
        self.responses_url
            .as_deref()
            .unwrap_or("https://chatgpt.com/backend-api/codex/responses")
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let auth_source = self.resolved_auth_source()?;
        for (name, value) in [
            ("auth_base_url", self.auth_base_url.as_deref()),
            ("responses_url", self.responses_url.as_deref()),
        ] {
            if let Some(value) = value {
                let url =
                    reqwest::Url::parse(value).map_err(|_| format!("invalid Codex {name}"))?;
                if !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(format!("invalid Codex {name}"));
                }
                let host = url.host_str().unwrap_or_default();
                let ip_host = host.trim_start_matches('[').trim_end_matches(']');
                let loopback = ip_host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
                let production = match name {
                    "auth_base_url" => {
                        url.scheme() == "https"
                            && host == "auth.openai.com"
                            && url.port_or_known_default() == Some(443)
                            && (url.path().is_empty() || url.path() == "/")
                    }
                    "responses_url" => {
                        url.scheme() == "https"
                            && host == "chatgpt.com"
                            && url.port_or_known_default() == Some(443)
                            && url.path() == "/backend-api/codex/responses"
                    }
                    _ => false,
                };
                let fixture = url.scheme() == "http" && loopback;
                if !production && !fixture {
                    return Err(format!(
                        "unsafe Codex {name}; use HTTPS or a loopback fixture"
                    ));
                }
            }
        }
        if self.enabled {
            let alias = self
                .alias
                .as_deref()
                .filter(|value| !value.trim().is_empty());
            let model = self
                .model
                .as_deref()
                .filter(|value| !value.trim().is_empty());
            let account_file = self
                .account_file
                .as_deref()
                .filter(|value| !value.trim().is_empty());
            if alias.is_none()
                || model.is_none()
                || (auth_source == CodexAuthSource::Managed && account_file.is_none())
            {
                return Err(
                    "enabled Codex requires alias and model, plus account_file for managed auth"
                        .into(),
                );
            }
            if auth_source == CodexAuthSource::Managed {
                let account_file = std::path::Path::new(account_file.unwrap());
                if !account_file.is_absolute() || account_file.file_name().is_none() {
                    return Err("Codex account_file must be an absolute file path".into());
                }
                if account_file
                    .ancestors()
                    .any(|ancestor| ancestor.join(".git").exists())
                {
                    return Err("Codex account_file must be outside a Git worktree".into());
                }
            }
            if let Some(path) = self
                .cli_auth_file
                .as_deref()
                .filter(|_| auth_source == CodexAuthSource::CodexCli)
            {
                let path = std::path::Path::new(path);
                if !path.is_absolute()
                    || path.file_name().is_none()
                    || path.components().any(|part| {
                        matches!(
                            part,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
                {
                    return Err(
                        "Codex cli_auth_file must be a normalized absolute file path".into(),
                    );
                }
            }
        }
        if self
            .alias
            .as_deref()
            .is_some_and(|alias| alias.trim().is_empty())
            || self
                .model
                .as_deref()
                .is_some_and(|model| model.trim().is_empty())
        {
            return Err("Codex alias and model must not be empty".into());
        }
        Ok(())
    }
}

fn default_refresh() -> u64 {
    60
}
fn default_affinity_lease() -> u64 {
    900
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub total_seconds: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            total_seconds: 1800,
        }
    }
}

impl RetryPolicy {
    pub fn remaining(
        &self,
        budget: Option<&ClassTimeoutBudget>,
        started: std::time::Instant,
    ) -> Option<std::time::Duration> {
        let total = budget.map_or(self.total_seconds, |budget| budget.total_seconds);
        std::time::Duration::from_secs(total)
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
    }

    pub fn attempt_timeout(
        &self,
        route_seconds: u64,
        budget: Option<&ClassTimeoutBudget>,
        started: std::time::Instant,
    ) -> Option<std::time::Duration> {
        Some(self.remaining(budget, started)?.min(upstream_timeout(
            route_seconds,
            budget,
            started,
        )?))
    }
}

#[cfg(test)]
mod retry_floor_tests {
    use super::*;

    #[test]
    fn streaming_progress_limits_are_optional_positive_and_bounded() {
        let omitted: ClassTimeoutBudget = serde_json::from_value(serde_json::json!({
            "attempt_seconds": 3, "total_seconds": 4
        }))
        .unwrap();
        assert!(omitted.validate().is_ok());
        let enabled: ClassTimeoutBudget = serde_json::from_value(serde_json::json!({
            "attempt_seconds": 3, "total_seconds": 4,
            "first_output_seconds": 1, "stream_idle_seconds": 2
        }))
        .unwrap();
        assert!(enabled.validate().is_ok());
        for field in ["first_output_seconds", "stream_idle_seconds"] {
            for value in [0, 4] {
                let mut json = serde_json::json!({"attempt_seconds": 3, "total_seconds": 4});
                json[field] = value.into();
                let invalid: ClassTimeoutBudget = serde_json::from_value(json).unwrap();
                assert!(invalid.validate().is_err(), "{field}={value}");
            }
        }
    }

    #[test]
    fn class_retry_floor_is_optional_and_cannot_exceed_total_budget() {
        let defaulted: ClassTimeoutBudget = serde_json::from_value(serde_json::json!({
            "attempt_seconds": 540, "total_seconds": 570
        }))
        .unwrap();
        assert_eq!(defaulted.min_retry_seconds, 0);
        let floor: ClassTimeoutBudget = serde_json::from_value(serde_json::json!({
            "attempt_seconds": 540, "total_seconds": 570, "min_retry_seconds": 120
        }))
        .unwrap();
        assert!(floor.validate().is_ok());
        let invalid: ClassTimeoutBudget = serde_json::from_value(serde_json::json!({
            "attempt_seconds": 540, "total_seconds": 570, "min_retry_seconds": 571
        }))
        .unwrap();
        assert!(invalid.validate().is_err());
        let over_attempt: ClassTimeoutBudget = serde_json::from_value(serde_json::json!({
            "attempt_seconds": 540, "total_seconds": 570, "min_retry_seconds": 541
        }))
        .unwrap();
        assert!(over_attempt.validate().is_err());
    }
}

#[cfg(test)]
mod affinity_default_tests {
    #[test]
    fn profile_affinity_defaults_to_fifteen_minutes() {
        assert_eq!(super::default_affinity_lease(), 900);
    }
}

#[cfg(test)]
mod timeout_budget_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn class_override_extends_one_attempt_but_caps_retry_at_remaining_budget() {
        let budget = ClassTimeoutBudget {
            attempt_seconds: 300,
            total_seconds: 390,
            min_retry_seconds: 0,
            ..Default::default()
        };
        let started = Instant::now();
        assert_eq!(
            upstream_timeout(120, Some(&budget), started),
            Some(Duration::from_secs(300))
        );
        let elapsed = Instant::now() - Duration::from_secs(301);
        assert!(upstream_timeout(120, Some(&budget), elapsed).unwrap() <= Duration::from_secs(89));
        let expired = Instant::now() - Duration::from_secs(391);
        assert_eq!(upstream_timeout(120, Some(&budget), expired), None);
        assert_eq!(
            upstream_timeout(120, None, expired),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn timeout_budget_rejects_zero_and_inverted_or_unbounded_limits() {
        assert!(
            ClassTimeoutBudget {
                attempt_seconds: 0,
                total_seconds: 390,
                min_retry_seconds: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ClassTimeoutBudget {
                attempt_seconds: 300,
                total_seconds: 299,
                min_retry_seconds: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ClassTimeoutBudget {
                attempt_seconds: 301,
                total_seconds: 1801,
                min_retry_seconds: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ClassTimeoutBudget {
                attempt_seconds: 300,
                total_seconds: 390,
                min_retry_seconds: 0,
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassTimeoutBudget {
    pub attempt_seconds: u64,
    pub total_seconds: u64,
    #[serde(default)]
    pub min_retry_seconds: u64,
    #[serde(default)]
    pub first_output_seconds: Option<u64>,
    #[serde(default)]
    pub stream_idle_seconds: Option<u64>,
}

impl ClassTimeoutBudget {
    fn validate(&self) -> Result<(), String> {
        if [self.first_output_seconds, self.stream_idle_seconds]
            .into_iter()
            .flatten()
            .any(|seconds| seconds == 0 || seconds > self.attempt_seconds)
        {
            return Err(
                "invalid streaming progress limit (positive and <= attempt_seconds)".into(),
            );
        }
        if self.attempt_seconds == 0
            || self.attempt_seconds > 1800
            || self.total_seconds < self.attempt_seconds
            || self.total_seconds > 1800
            || self.min_retry_seconds > self.attempt_seconds
        {
            return Err(
                "invalid class timeout budget (attempt 1..1800; total >= attempt and <= 1800)"
                    .into(),
            );
        }
        Ok(())
    }
}

pub fn upstream_timeout(
    route_seconds: u64,
    budget: Option<&ClassTimeoutBudget>,
    started: std::time::Instant,
) -> Option<std::time::Duration> {
    let Some(budget) = budget else {
        return Some(std::time::Duration::from_secs(route_seconds));
    };
    let remaining =
        std::time::Duration::from_secs(budget.total_seconds).checked_sub(started.elapsed())?;
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.min(std::time::Duration::from_secs(budget.attempt_seconds)))
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub name: String,
    #[serde(default)]
    pub key_file: Option<String>,
    pub aliases: Vec<String>,
    #[serde(default)]
    pub allowed_routes: Vec<String>,
    pub class: String,
    #[serde(default = "default_rpm")]
    pub requests_per_minute: u32,
    #[serde(default = "default_burst")]
    pub burst: u32,
    #[serde(default = "default_inflight")]
    pub max_in_flight: usize,
    #[serde(default)]
    pub operator: bool,
    #[serde(default)]
    pub cloud_allowed: bool,
    #[serde(default)]
    pub agent_affinity: bool,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl Credential {
    pub(crate) fn can_use_codex(&self, codex: &CodexConfig) -> bool {
        codex.enabled && self.cloud_allowed
    }
}

fn default_rpm() -> u32 {
    60
}
fn default_burst() -> u32 {
    10
}
fn default_inflight() -> usize {
    2
}

#[derive(Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CopilotAuthSource {
    #[default]
    Managed,
    #[serde(rename = "oauth_token_file")]
    OAuthTokenFile,
}

#[cfg(test)]
mod copilot_auth_source_wire_contract {
    use super::CopilotAuthSource;

    #[test]
    fn documented_oauth_token_file_source_name_deserializes() {
        let source = serde_yaml::from_str::<CopilotAuthSource>("oauth_token_file")
            .expect("the documented read-only source spelling must deserialize");
        assert!(matches!(source, CopilotAuthSource::OAuthTokenFile));
    }
}

#[cfg(test)]
mod copilot_key_policy_contract {
    use super::{Config, key_credentials_from_snapshot};
    use crate::key_manager::{AuthSnapshot, KeyPolicy, SnapshotKey};

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "bind": "100.127.255.254", "port": 4000,
            "refresh_seconds": 30, "affinity_lease_seconds": 300,
            "credentials": [], "routes": [], "aliases": {},
            "copilot": {
                "enabled": true, "auth_source": "oauth_token_file",
                "oauth_token_file": "/synthetic/github-oauth",
                "models": [{"alias": "copilot-test", "model": "synthetic-chat", "endpoint": "chat_completions"}]
            }
        })).unwrap()
    }

    fn snapshot(route: &str) -> AuthSnapshot {
        let policy: KeyPolicy = serde_json::from_value(serde_json::json!({
            "id": "copilot-workload", "aliases": ["copilot-test"],
            "class": "interactive", "requests_per_minute": 60,
            "burst": 10, "max_in_flight": 1, "operator": false,
            "agent_affinity": true, "allowed_routes": [route],
            "cloud_allowed": true
        }))
        .unwrap();
        AuthSnapshot::from([(
            "synthetic-prefix".into(),
            SnapshotKey {
                verifier: "synthetic-verifier".into(),
                policy,
                expires_at: None,
            },
        )])
    }

    #[test]
    fn copilot_scoped_key_snapshot_loads_without_local_alias() {
        let credentials = key_credentials_from_snapshot(&config(), &snapshot("copilot"))
            .expect("Copilot aliases must validate against their native route");
        assert_eq!(credentials.len(), 1);
        assert!(credentials[0].cloud_allowed);
    }

    #[test]
    fn copilot_alias_rejects_other_native_route_without_panicking() {
        assert!(key_credentials_from_snapshot(&config(), &snapshot("codex")).is_err());
    }

    #[test]
    fn copilot_scoped_file_policy_loads_without_local_alias() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("client-key");
        let config_path = directory.path().join("config.json");
        std::fs::write(&key_path, "synthetic-workload-key-for-native-copilot-only").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let data = serde_json::json!({
            "bind": "100.127.255.254", "port": 4000,
            "refresh_seconds": 30, "affinity_lease_seconds": 300,
            "credentials": [{
                "name": "copilot-file-workload", "key_file": key_path,
                "aliases": ["copilot-test"], "class": "interactive",
                "requests_per_minute": 60, "burst": 10, "max_in_flight": 1,
                "allowed_routes": ["copilot"], "cloud_allowed": true
            }],
            "routes": [], "aliases": {},
            "copilot": {
                "enabled": true, "auth_source": "oauth_token_file",
                "oauth_token_file": "/synthetic/github-oauth",
                "models": [{"alias": "copilot-test", "model": "synthetic-chat", "endpoint": "chat_completions"}]
            }
        });
        std::fs::write(&config_path, serde_json::to_vec(&data).unwrap()).unwrap();
        super::load(&config_path)
            .expect("Copilot-only file policies must load without local aliases");
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CopilotEndpoint {
    ChatCompletions,
    Responses,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CopilotModel {
    pub alias: String,
    pub model: String,
    pub endpoint: CopilotEndpoint,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CopilotConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auth_source: CopilotAuthSource,
    #[serde(default)]
    pub oauth_token_file: Option<String>,
    #[serde(default)]
    pub managed_auth_file: Option<String>,
    #[serde(default)]
    pub models: Vec<CopilotModel>,
}

impl Default for CopilotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auth_source: CopilotAuthSource::Managed,
            oauth_token_file: None,
            managed_auth_file: None,
            models: Vec::new(),
        }
    }
}

impl CopilotConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.enabled && self.models.is_empty() {
            return Err("Copilot requires at least one explicitly configured model".into());
        }
        match self.auth_source {
            CopilotAuthSource::Managed => {
                if self.oauth_token_file.is_some() {
                    return Err(
                        "copilot.oauth_token_file is valid only with oauth_token_file auth_source"
                            .into(),
                    );
                }
                if self
                    .managed_auth_file
                    .as_ref()
                    .is_some_and(|path| !Path::new(path).is_absolute())
                {
                    return Err("copilot.managed_auth_file must be absolute".into());
                }
            }
            CopilotAuthSource::OAuthTokenFile => {
                let Some(path) = self.oauth_token_file.as_ref() else {
                    return Err(
                        "copilot.oauth_token_file is required for oauth_token_file auth_source"
                            .into(),
                    );
                };
                if !Path::new(path).is_absolute() {
                    return Err("copilot.oauth_token_file must be absolute".into());
                }
                if self.managed_auth_file.is_some() {
                    return Err("copilot.managed_auth_file cannot be used with oauth_token_file auth_source".into());
                }
            }
        }
        let mut aliases = HashSet::new();
        for model in &self.models {
            for value in [&model.alias, &model.model] {
                if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
                    return Err(
                        "Copilot aliases and model names must be 1-128 printable characters".into(),
                    );
                }
            }
            if model.alias.trim() != model.alias || model.model.trim() != model.model {
                return Err(
                    "Copilot aliases and model names cannot have surrounding whitespace".into(),
                );
            }
            if !aliases.insert(&model.alias) {
                return Err("Copilot model aliases must be unique".into());
            }
        }
        Ok(())
    }

    pub(crate) fn model_for_alias(&self, alias: &str) -> Option<&CopilotModel> {
        self.models.iter().find(|model| model.alias == alias)
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    pub kind: RouteKind,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub physical_route: Option<String>,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    pub safe_concurrency: usize,
    pub context_limit: u32,
    pub adapter_version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_true")]
    pub streaming: bool,
    #[serde(default)]
    pub responses_done_tail: bool,
    #[serde(default)]
    pub credential_file: Option<String>,
    #[serde(default)]
    pub disable_thinking: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub enabled: bool,
}
fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    120
}
fn default_health_path() -> String {
    "/health".into()
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Local,
    Headroom,
    Embedding,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    #[serde(default)]
    pub passthrough_route: Option<String>,
    pub context_limit: u32,
    #[serde(default)]
    pub tokenizer: Option<TokenizerSpec>,
    #[serde(default)]
    pub endpoint: Endpoint,
    pub routes: Vec<AliasRoute>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_true")]
    pub streaming: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenizerSpec {
    pub file: String,
    pub sha256: String,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Endpoint {
    #[default]
    Chat,
    Embedding,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasRoute {
    pub route: String,
    pub priority: u32,
    #[serde(default)]
    pub fallback_only: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    pub id: String,
    pub class: String,
    pub source: MatcherSource,
    pub regex: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatcherSource {
    UserAgent,
    CredentialName,
}

pub struct LoadedCredential {
    pub config: Credential,
    pub key: Option<Vec<u8>>,
}
pub struct LoadedRoute {
    pub config: Route,
    pub key: Option<Vec<u8>>,
}
pub struct LoadedMatcher {
    pub config: Matcher,
    pub regex: Regex,
}
pub struct Policy {
    pub config: Config,
    pub credentials: Vec<LoadedCredential>,
    pub routes: Vec<LoadedRoute>,
    pub matchers: Vec<LoadedMatcher>,
    pub tokenizers: HashMap<String, Arc<Tokenizer>>,
    pub key_snapshot: Option<Arc<AuthSnapshot>>,
}

pub fn key_credentials_from_snapshot(
    config: &Config,
    keys: &AuthSnapshot,
) -> Result<Vec<Credential>, String> {
    let mut policies: HashMap<String, key_manager::KeyPolicy> = HashMap::new();
    for key in keys.values() {
        match policies.get(&key.policy.id) {
            Some(existing) if existing != &key.policy => {
                return Err("inconsistent key policy versions".into());
            }
            Some(_) => {}
            None => {
                policies.insert(key.policy.id.clone(), key.policy.clone());
            }
        }
    }
    let mut credentials: Vec<_> = policies
        .into_values()
        .map(|policy| Credential {
            name: policy.id,
            key_file: None,
            aliases: policy.aliases,
            allowed_routes: policy.allowed_routes,
            class: policy.class,
            cloud_allowed: policy.cloud_allowed,
            requests_per_minute: policy.requests_per_minute,
            burst: policy.burst,
            max_in_flight: policy.max_in_flight,
            operator: policy.operator,
            agent_affinity: policy.agent_affinity,
            expires_at: policy.expires_at,
        })
        .collect();
    credentials.sort_by(|a, b| a.name.cmp(&b.name));
    let mut class_limits = HashMap::new();
    for credential in &credentials {
        if credential.aliases.iter().any(|alias| {
            !config.aliases.contains_key(alias)
                && config.codex.alias.as_deref() != Some(alias.as_str())
                && !config
                    .copilot
                    .models
                    .iter()
                    .any(|model| model.alias == *alias)
        }) {
            return Err(format!("unknown authorized alias for {}", credential.name));
        }
        if !credential.allowed_routes.is_empty() {
            let allowed: HashSet<_> = credential.allowed_routes.iter().collect();
            if allowed.len() != credential.allowed_routes.len()
                || credential.allowed_routes.iter().any(|name| {
                    name != "codex"
                        && name != "copilot"
                        && !config.routes.iter().any(|route| &route.name == name)
                })
                || credential.aliases.iter().any(|name| {
                    if config.codex.alias.as_ref() == Some(name) {
                        return !allowed.contains(&"codex".to_string());
                    }
                    if config
                        .copilot
                        .models
                        .iter()
                        .any(|model| model.alias == *name)
                    {
                        return !allowed.contains(&"copilot".to_string());
                    }
                    !config.aliases[name].routes.iter().any(|entry| {
                        entry.enabled
                            && allowed.contains(&entry.route)
                            && config
                                .routes
                                .iter()
                                .any(|route| route.name == entry.route && route.enabled)
                    })
                })
            {
                return Err(format!("invalid allowed_routes for {}", credential.name));
            }
        }
        if let Some(limit) = class_limits.insert(&credential.class, credential.max_in_flight)
            && limit != credential.max_in_flight
        {
            return Err(format!(
                "inconsistent in-flight limit for class {}",
                credential.class
            ));
        }
    }
    Ok(credentials)
}

fn load_tokenizer(spec: &TokenizerSpec) -> Result<Arc<Tokenizer>, String> {
    let path = Path::new(&spec.file);
    if !path.is_absolute() {
        return Err("tokenizer file must be absolute".into());
    }
    let bytes = fs::read(path).map_err(|e| format!("tokenizer read: {e}"))?;
    if bytes.len() > 40 * 1024 * 1024 || bytes.is_empty() {
        return Err("tokenizer size invalid".into());
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if digest != spec.sha256.to_ascii_lowercase() {
        return Err("tokenizer checksum mismatch".into());
    }
    Tokenizer::from_bytes(bytes)
        .map(Arc::new)
        .map_err(|e| format!("tokenizer parse: {e}"))
}

fn read_key(path: &str) -> Result<Vec<u8>, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("credential file must be absolute".into());
    }
    let metadata = fs::metadata(p).map_err(|e| format!("cannot stat credential file: {e}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("credential file must be regular and private (mode 0600 or stricter)".into());
    }
    let key = fs::read(p).map_err(|e| format!("cannot read credential file: {e}"))?;
    let key = key.strip_suffix(b"\n").unwrap_or(&key).to_vec();
    if key.len() < 24 || key.len() > 4096 {
        return Err("credential length outside 24..4096".into());
    }
    Ok(key)
}

pub fn load(path: &Path) -> Result<Policy, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("config read: {e}"))?;
    let mut config: Config =
        serde_yaml::from_str(&text).map_err(|e| format!("config parse: {e}"))?;
    config.codex.validate()?;
    config.copilot.validate()?;
    if config
        .codex
        .alias
        .as_ref()
        .is_some_and(|name| config.aliases.contains_key(name))
        || config.copilot.models.iter().any(|model| {
            config.aliases.contains_key(&model.alias)
                || config.codex.alias.as_ref() == Some(&model.alias)
        })
        || (config.codex.enabled && config.routes.iter().any(|route| route.name == "codex"))
        || (config.copilot.enabled && config.routes.iter().any(|route| route.name == "copilot"))
    {
        return Err("Cloud provider alias and route identity must be exclusive".into());
    }
    let key_snapshot = if let Some(path) = &config.key_store {
        if !config.credentials.is_empty() || !Path::new(path).is_absolute() {
            return Err("key_store requires an absolute path and no legacy credentials".into());
        }
        let keys = key_manager::snapshot(Path::new(path))
            .map_err(|e| format!("key store unavailable: {e}"))?;
        if keys.is_empty() {
            return Err("key store has no active credentials".into());
        }
        config.credentials = key_credentials_from_snapshot(&config, &keys)?;
        Some(Arc::new(keys))
    } else {
        None
    };
    if config.retry_policy.max_attempts == 0
        || config.retry_policy.max_attempts > 16
        || config.retry_policy.total_seconds == 0
        || config.retry_policy.total_seconds > 1800
    {
        return Err("invalid retry policy (max_attempts 1..16; total_seconds 1..1800)".into());
    }
    for (class, budget) in &config.class_timeout_budgets {
        budget.validate()?;
        if !config
            .credentials
            .iter()
            .any(|credential| &credential.class == class)
        {
            return Err(format!(
                "timeout budget for unknown credential class {class}"
            ));
        }
    }
    if config.port == 4001
        || config.refresh_seconds < 30
        || config.refresh_seconds > 3600
        || config.affinity_lease_seconds == 0
        || config.affinity_lease_seconds > 3600
    {
        return Err("port 4001 is reserved for incumbent; refresh_seconds must be 30..3600 and affinity_lease_seconds 1..3600".into());
    }
    if config.bind.is_unspecified()
        || !matches!(config.bind, IpAddr::V4(v) if v.octets()[0] == 100 && v.octets()[1] >= 64 && v.octets()[1] <= 127)
    {
        return Err("bind must be an explicit Tailnet IPv4 address".into());
    }
    let route_name_re = Regex::new("^[a-z0-9][a-z0-9_-]{0,63}$").unwrap();
    let mut route_names = HashSet::new();
    for route in &config.routes {
        if !route_name_re.is_match(&route.name) {
            return Err("invalid route name".into());
        }
        if !route_names.insert(route.name.as_str()) {
            return Err(format!("duplicate route {}", route.name));
        }
        if route.safe_concurrency == 0
            || route.safe_concurrency > 64
            || route.context_limit == 0
            || route.timeout_seconds < 1
            || route.timeout_seconds > 1800
        {
            return Err(format!("invalid limits on route {}", route.name));
        }
        if route.adapter_version.trim().is_empty() {
            return Err(format!("missing adapter version on route {}", route.name));
        }
        if !route.health_path.starts_with('/')
            || route.health_path.starts_with("//")
            || route.health_path.contains('?')
            || route.health_path.contains('#')
            || route.health_path.len() > 100
        {
            return Err(format!("invalid health path on route {}", route.name));
        }
        let url = reqwest::Url::parse(&route.base_url)
            .map_err(|_| format!("invalid URL on route {}", route.name))?;
        if url.scheme() != "http"
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.path().ends_with("/v1")
        {
            return Err(format!("unsafe URL on route {}", route.name));
        }
        let host = url.host_str().ok_or("missing route host")?;
        let ip: IpAddr = host
            .parse()
            .map_err(|_| format!("route {} must use a fixed IP", route.name))?;
        if route.kind == RouteKind::Headroom && !ip.is_loopback() {
            return Err(format!("Headroom route {} must be loopback", route.name));
        }
        if route.kind != RouteKind::Headroom
            && !ip.is_loopback()
            && !matches!(ip, IpAddr::V4(v) if v.octets()[0] == 100 && v.octets()[1] >= 64 && v.octets()[1] <= 127)
        {
            return Err(format!("route {} must be loopback or Tailnet", route.name));
        }
        if !route.capabilities.iter().all(|c| {
            c == "text"
                || c == "image"
                || c == "tools"
                || c == "json_schema"
                || c == "json_object"
                || c == "responses"
                || c == "responses_json_schema"
                || c == "responses_json_object"
        }) || (route.kind == RouteKind::Embedding
            && route.capabilities.iter().any(|c| c != "text"))
            || (route.responses_done_tail
                && (!route.streaming || !route.capabilities.iter().any(|c| c == "responses")))
        {
            return Err(format!(
                "route {} has an unsupported capability",
                route.name
            ));
        }
    }
    for route in &config.routes {
        match (route.kind, route.physical_route.as_deref()) {
            (RouteKind::Headroom, Some(name)) => {
                if route.health_path != "/healthz" {
                    return Err(format!("Headroom route {} must use /healthz", route.name));
                }
                let physical = config
                    .routes
                    .iter()
                    .find(|candidate| candidate.name == name)
                    .ok_or_else(|| format!("unknown physical route for {}", route.name))?;
                if physical.kind != RouteKind::Local
                    || !physical.enabled
                    || physical.model != route.model
                    || physical.safe_concurrency != route.safe_concurrency
                    || physical.context_limit < route.context_limit
                    || (route.streaming && !physical.streaming)
                    || route.disable_thinking != physical.disable_thinking
                    || !route
                        .capabilities
                        .iter()
                        .all(|capability| physical.capabilities.contains(capability))
                {
                    return Err(format!("incompatible physical route for {}", route.name));
                }
                // Tools and structured output require serving fixtures. Image
                // forwarding requires separate adapter and physical-route evidence.
            }
            (RouteKind::Headroom, None) => {
                return Err(format!(
                    "Headroom route {} needs physical_route",
                    route.name
                ));
            }
            (_, Some(_)) => {
                return Err(format!(
                    "physical_route is only valid for Headroom route {}",
                    route.name
                ));
            }
            (_, None) => {}
        }
    }
    let alias_name_re = Regex::new("^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$").unwrap();
    for (name, alias) in &config.aliases {
        if !alias_name_re.is_match(name) {
            return Err("invalid alias name".into());
        }
        if alias.context_limit == 0
            || alias.routes.is_empty()
            || !alias.capabilities.iter().all(|c| {
                c == "text"
                    || c == "image"
                    || c == "tools"
                    || c == "json_schema"
                    || c == "json_object"
                    || c == "responses"
                    || c == "responses_json_schema"
                    || c == "responses_json_object"
            })
            || (alias.endpoint == Endpoint::Embedding
                && alias.capabilities.iter().any(|c| c != "text"))
        {
            return Err(format!("invalid alias {name}"));
        }
        if let Some(target) = &alias.passthrough_route {
            let matching: Vec<_> = alias.routes.iter().filter(|e| e.enabled).collect();
            let route = config
                .routes
                .iter()
                .find(|r| &r.name == target)
                .ok_or_else(|| format!("unknown passthrough route in alias {name}"))?;
            if matching.len() != 1
                || &matching[0].route != target
                || matching[0].fallback_only
                || !route.enabled
                || (alias.endpoint == Endpoint::Embedding) != (route.kind == RouteKind::Embedding)
            {
                return Err(format!(
                    "passthrough requires one enabled compatible pinned route in alias {name}"
                ));
            }
        }
        let mut seen = HashSet::new();
        let mut enabled = false;
        let mut full_capability_route = false;
        for entry in &alias.routes {
            if !seen.insert(&entry.route) {
                return Err(format!("duplicate route in alias {name}"));
            }
            let route = config
                .routes
                .iter()
                .find(|r| r.name == entry.route)
                .ok_or_else(|| format!("unknown route in alias {name}"))?;
            if entry.enabled && route.enabled {
                enabled = true;
                if alias
                    .capabilities
                    .iter()
                    .all(|capability| route.capabilities.contains(capability))
                {
                    full_capability_route = true;
                }
            }
            if alias.context_limit > route.context_limit
                || alias.streaming && !route.streaming
                || (alias.endpoint == Endpoint::Embedding) != (route.kind == RouteKind::Embedding)
            {
                return Err(format!("alias/route incompatible: {name}/{}", route.name));
            }
        }
        if !enabled || !full_capability_route {
            return Err(format!(
                "alias {name} has no enabled route covering its full capabilities"
            ));
        }
    }
    let mut tokenizers = HashMap::new();
    let mut tokenizer_cache: HashMap<String, Arc<Tokenizer>> = HashMap::new();
    for (name, alias) in &config.aliases {
        if let Some(spec) = &alias.tokenizer {
            if alias.endpoint != Endpoint::Chat {
                return Err(format!("tokenizer only supported for chat alias {name}"));
            }
            let key = format!("{}:{}", spec.file, spec.sha256);
            let tokenizer = if let Some(cached) = tokenizer_cache.get(&key) {
                cached.clone()
            } else {
                let loaded = load_tokenizer(spec)?;
                tokenizer_cache.insert(key, loaded.clone());
                loaded
            };
            tokenizers.insert(name.clone(), tokenizer);
        }
    }
    let mut credential_names = HashSet::new();
    let mut class_limits = HashMap::new();
    let mut credentials = Vec::new();
    for credential in &config.credentials {
        if !credential_names.insert(credential.name.as_str())
            || (config.key_store.is_none() && credential.key_file.is_none())
            || credential.class.is_empty()
            || credential.requests_per_minute == 0
            || credential.burst == 0
            || credential.max_in_flight == 0
        {
            return Err("invalid credential".into());
        }
        if credential.aliases.iter().any(|a| {
            !config.aliases.contains_key(a)
                && config.codex.alias.as_ref() != Some(a)
                && !config.copilot.models.iter().any(|model| &model.alias == a)
        }) {
            return Err(format!("unknown authorized alias for {}", credential.name));
        }
        if !credential.allowed_routes.is_empty() {
            let allowed: HashSet<_> = credential.allowed_routes.iter().collect();
            if allowed.len() != credential.allowed_routes.len()
                || credential.allowed_routes.iter().any(|name| {
                    name != "codex"
                        && name != "copilot"
                        && !config.routes.iter().any(|route| &route.name == name)
                })
                || credential.aliases.iter().any(|name| {
                    if config.codex.alias.as_ref() == Some(name) {
                        return !allowed.contains(&"codex".to_string());
                    }
                    if config
                        .copilot
                        .models
                        .iter()
                        .any(|model| model.alias == *name)
                    {
                        return !allowed.contains(&"copilot".to_string());
                    }
                    !config.aliases[name].routes.iter().any(|entry| {
                        entry.enabled
                            && allowed.contains(&entry.route)
                            && config
                                .routes
                                .iter()
                                .any(|route| route.name == entry.route && route.enabled)
                    })
                })
            {
                return Err(format!("invalid allowed_routes for {}", credential.name));
            }
        }
        if let Some(limit) = class_limits.insert(credential.class.clone(), credential.max_in_flight)
            && limit != credential.max_in_flight
        {
            return Err(format!(
                "inconsistent in-flight limit for class {}",
                credential.class
            ));
        }
        credentials.push(LoadedCredential {
            config: credential.clone(),
            key: credential.key_file.as_deref().map(read_key).transpose()?,
        });
    }
    if credentials.is_empty() {
        return Err("no credentials".into());
    }
    for i in 0..credentials.len() {
        for j in i + 1..credentials.len() {
            if credentials[i].key.is_some() && credentials[i].key == credentials[j].key {
                return Err("duplicate credential material".into());
            }
        }
    }
    let mut routes = Vec::new();
    for route in &config.routes {
        routes.push(LoadedRoute {
            config: route.clone(),
            key: route
                .credential_file
                .as_ref()
                .map(|p| read_key(p))
                .transpose()?,
        });
    }
    let mut matchers = Vec::new();
    for matcher in &config.metadata_matchers {
        if matcher.regex.len() > 256 || !config.credentials.iter().any(|c| c.class == matcher.class)
        {
            return Err(format!("invalid matcher {}", matcher.id));
        }
        matchers.push(LoadedMatcher {
            config: matcher.clone(),
            regex: Regex::new(&matcher.regex).map_err(|e| format!("regex: {e}"))?,
        });
    }
    Ok(Policy {
        config,
        credentials,
        routes,
        matchers,
        tokenizers,
        key_snapshot,
    })
}

#[cfg(test)]
mod copilot_config_tdd_tests {
    use super::*;

    #[test]
    fn copilot_auth_sources_are_disjoint_and_aliases_are_unique() {
        let managed_with_external = CopilotConfig {
            enabled: false,
            auth_source: CopilotAuthSource::Managed,
            oauth_token_file: Some("/synthetic/oauth-token".into()),
            managed_auth_file: None,
            models: Vec::new(),
        };
        assert!(managed_with_external.validate().is_err());

        let read_only_with_managed = CopilotConfig {
            enabled: false,
            auth_source: CopilotAuthSource::OAuthTokenFile,
            oauth_token_file: Some("/synthetic/oauth-token".into()),
            managed_auth_file: Some("/synthetic/managed-auth.json".into()),
            models: Vec::new(),
        };
        assert!(read_only_with_managed.validate().is_err());

        let duplicate_aliases = CopilotConfig {
            enabled: true,
            auth_source: CopilotAuthSource::Managed,
            oauth_token_file: None,
            managed_auth_file: None,
            models: vec![
                CopilotModel {
                    alias: "same-alias".into(),
                    model: "first-model".into(),
                    endpoint: CopilotEndpoint::ChatCompletions,
                },
                CopilotModel {
                    alias: "same-alias".into(),
                    model: "second-model".into(),
                    endpoint: CopilotEndpoint::Responses,
                },
            ],
        };
        assert!(duplicate_aliases.validate().is_err());
    }

    #[test]
    fn native_copilot_configuration_is_a_first_class_config_section() {
        let yaml = r#"
bind: 127.0.0.1
port: 8080
routes: []
aliases: {}
copilot:
  enabled: true
  auth_source: managed
  models:
    - alias: copilot-haiku
      model: claude-haiku-5.5
      endpoint: chat_completions
"#;
        let parsed = serde_yaml::from_str::<Config>(yaml);
        assert!(parsed.is_ok(), "Copilot config must parse");
    }
}
