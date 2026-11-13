use crate::{AppState, authorize, json_error};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
    routing::post,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
};

pub(crate) const MAX_REQUEST_BYTES: usize = 256 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SECRET_BYTES: u64 = 4096;

/// Memory calls must not hold the model permits needed by Hindsight callbacks.
/// Keep their own class-bounded pools across policy reloads, without retaining
/// inactive classes or conflating changed class limits with active old limits.
#[derive(Default)]
pub(crate) struct Admission {
    pools: Mutex<BTreeMap<(String, usize), Weak<Semaphore>>>,
}

impl Admission {
    fn try_acquire(
        &self,
        class: &str,
        limit: usize,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        let semaphore = {
            let mut pools = self.pools.lock().unwrap();
            pools.retain(|_, pool| pool.strong_count() > 0);
            let key = (class.to_owned(), limit);
            match pools.get(&key).and_then(Weak::upgrade) {
                Some(pool) => pool,
                None => {
                    let pool = Arc::new(Semaphore::new(limit));
                    pools.insert(key, Arc::downgrade(&pool));
                    pool
                }
            }
        };
        semaphore.try_acquire_owned()
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub timeout_ms: u64,
    pub bindings: BTreeMap<String, Binding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            api_key_env: None,
            api_key_file: None,
            timeout_ms: 10000,
            bindings: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub bank_id: String,
    #[serde(default)]
    pub allow_retain: bool,
}

impl Config {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(100..=120000).contains(&self.timeout_ms) {
            return Err("Hindsight timeout_ms must be 100..120000".into());
        }
        if let Some(endpoint) = &self.endpoint {
            let url = reqwest::Url::parse(endpoint)
                .map_err(|_| "invalid Hindsight endpoint".to_string())?;
            let raw_path = endpoint
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'));
            if !matches!(url.scheme(), "http" | "https")
                || !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || !matches!(url.path(), "" | "/")
                || raw_path.is_some_and(|(_, path)| !path.is_empty())
                || endpoint
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\')
                || endpoint.contains('@')
            {
                return Err("invalid Hindsight endpoint: use an HTTP(S) origin without userinfo, path, query or fragment".into());
            }
        }
        if self.api_key_env.is_some() && self.api_key_file.is_some() {
            return Err("Hindsight requires at most one secret reference".into());
        }
        if self.api_key_env.as_ref().is_some_and(|name| {
            name.is_empty()
                || name.len() > 128
                || !name.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_alphabetic()
                        || (index > 0 && byte.is_ascii_digit())
                })
        }) {
            return Err("invalid Hindsight api_key_env".into());
        }
        if self.api_key_file.as_ref().is_some_and(|path| {
            !Path::new(path).is_absolute() || Path::new(path).file_name().is_none()
        }) {
            return Err("Hindsight api_key_file must be an absolute file path".into());
        }
        if self.enabled && self.endpoint.is_none() {
            return Err("enabled Hindsight requires endpoint".into());
        }
        if self.bindings.iter().any(|(credential, binding)| {
            credential.is_empty()
                || credential.len() > 128
                || binding.bank_id.is_empty()
                || binding.bank_id.len() > 64
                || !binding
                    .bank_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        }) {
            return Err("invalid Hindsight credential binding or bank ID".into());
        }
        Ok(())
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, Failure> {
        let origin = self.endpoint.as_deref().ok_or(Failure::Unavailable)?;
        let mut url = reqwest::Url::parse(origin).map_err(|_| Failure::Unavailable)?;
        url.set_path(path);
        Ok(url)
    }

    async fn secret(&self) -> Result<HeaderValue, Failure> {
        let value = if let Some(name) = &self.api_key_env {
            std::env::var(name).map_err(|_| Failure::Secret)?
        } else if let Some(path) = &self.api_key_file {
            let file = tokio::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .await
                .map_err(|_| Failure::Secret)?;
            let metadata = file.metadata().await.map_err(|_| Failure::Secret)?;
            if !metadata.is_file()
                || metadata.len() > MAX_SECRET_BYTES
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(Failure::Secret);
            }
            let mut bytes = Vec::new();
            file.take(MAX_SECRET_BYTES + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| Failure::Secret)?;
            if bytes.len() as u64 > MAX_SECRET_BYTES {
                return Err(Failure::Secret);
            }
            String::from_utf8(bytes).map_err(|_| Failure::Secret)?
        } else {
            return Err(Failure::Secret);
        };
        let value = value.trim();
        if value.is_empty()
            || value.len() as u64 > MAX_SECRET_BYTES
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Failure::Secret);
        }
        let mut header =
            HeaderValue::from_str(&format!("Bearer {value}")).map_err(|_| Failure::Secret)?;
        header.set_sensitive(true);
        Ok(header)
    }
}

pub(crate) fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(2)
        .build()
        .map_err(|_| "Hindsight transport initialization failed".into())
}

#[derive(Clone, Copy)]
enum Operation {
    Retain,
    Recall,
    Reflect,
}

impl Operation {
    fn suffix(self) -> &'static str {
        match self {
            Self::Retain => "memories",
            Self::Recall => "memories/recall",
            Self::Reflect => "reflect",
        }
    }
}

#[derive(Clone, Copy)]
enum Failure {
    Unavailable,
    Secret,
    Timeout,
    Transport,
    Redirect,
    Denied,
    Upstream,
    Response,
    TooLarge,
    Request,
}

impl Failure {
    fn name(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Secret => "secret_unavailable",
            Self::Timeout => "timeout",
            Self::Transport => "unreachable",
            Self::Redirect => "redirect_rejected",
            Self::Denied => "unauthorized",
            Self::Upstream => "upstream_error",
            Self::Response => "invalid_response",
            Self::TooLarge => "request_too_large",
            Self::Request => "invalid_request",
        }
    }
    fn response(self) -> Response {
        let status = match self {
            Self::Unavailable | Self::Secret => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Request => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        json_error(
            status,
            self.name(),
            "Hindsight operation failed; no completion guarantee. Check operator diagnostics.",
        )
    }
}

fn transport(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::Timeout
    } else {
        Failure::Transport
    }
}

async fn json_response(response: reqwest::Response) -> Result<(StatusCode, Value), Failure> {
    let status = response.status();
    if status.is_redirection() {
        return Err(Failure::Redirect);
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(Failure::Denied);
    }
    if !status.is_success() {
        return Err(Failure::Upstream);
    }
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        })
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(Failure::Response);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
            return Err(Failure::Response);
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Response)?;
    if !value.is_object() {
        return Err(Failure::Response);
    }
    Ok((status, value))
}

fn shape(value: &Value, kind: &str) -> bool {
    if value.is_null() {
        return true;
    }
    match kind {
        "text" => value.is_string(),
        "bool" => value.is_boolean(),
        "tokens" => value
            .as_u64()
            .is_some_and(|count| count > 0 && count <= 1_000_000),
        "object" => value.is_object(),
        "rerank" => value.as_object().is_some_and(|options| {
            options.iter().all(|(key, value)| {
                matches!(key.as_str(), "prune" | "dedup") && (value.is_null() || value.is_boolean())
            })
        }),
        "scopes" => {
            value.is_string()
                || value.as_array().is_some_and(|scopes| {
                    scopes.iter().all(|scope| {
                        scope
                            .as_array()
                            .is_some_and(|tags| tags.iter().all(Value::is_string))
                    })
                })
        }
        "strings" => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        "objects" => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_object)),
        _ => false,
    }
}

fn validate_request(operation: Operation, value: &Value) -> Result<(), Failure> {
    let object = value.as_object().ok_or(Failure::Request)?;
    if matches!(operation, Operation::Retain) {
        for (key, value) in object {
            let valid = match key.as_str() {
                "items" => value
                    .as_array()
                    .is_some_and(|items| !items.is_empty() && items.iter().all(valid_item)),
                "async" => value.is_boolean(),
                "document_tags" => shape(value, "strings"),
                "operation_id" => shape(value, "text"),
                _ => false,
            };
            if !valid {
                return Err(Failure::Request);
            }
        }
        return if object.contains_key("items") {
            Ok(())
        } else {
            Err(Failure::Request)
        };
    }
    if !object
        .get("query")
        .and_then(Value::as_str)
        .is_some_and(|query| !query.trim().is_empty())
    {
        return Err(Failure::Request);
    }
    for (key, value) in object {
        let kind = match key.as_str() {
            "query" => "text",
            "budget" | "tags_match" => "text",
            "tags" => "strings",
            "max_tokens" => "tokens",
            "include" => "object",
            "tag_groups" => "objects",
            "types" if matches!(operation, Operation::Recall) => "strings",
            "rerank" if matches!(operation, Operation::Recall) => "rerank",
            "prefer_observations" | "trace" if matches!(operation, Operation::Recall) => "bool",
            "query_timestamp" if matches!(operation, Operation::Recall) => "text",
            "min_scores" | "temporal_window" if matches!(operation, Operation::Recall) => "object",
            "context" if matches!(operation, Operation::Reflect) => "text",
            "response_schema" if matches!(operation, Operation::Reflect) => "object",
            "fact_types" | "exclude_mental_model_ids"
                if matches!(operation, Operation::Reflect) =>
            {
                "strings"
            }
            "apply_all_directives"
            | "exclude_mental_models"
            | "reflect_search_observations_include_entities"
                if matches!(operation, Operation::Reflect) =>
            {
                "bool"
            }
            "reflect_search_observations_max_tokens" if matches!(operation, Operation::Reflect) => {
                "tokens"
            }
            _ => return Err(Failure::Request),
        };
        if !shape(value, kind) {
            return Err(Failure::Request);
        }
    }
    for (key, allowed) in [
        ("budget", &["low", "mid", "high"][..]),
        (
            "tags_match",
            &["any", "all", "any_strict", "all_strict", "exact"][..],
        ),
    ] {
        if object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|text| !allowed.contains(&text))
        {
            return Err(Failure::Request);
        }
    }
    Ok(())
}

fn valid_item(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !object
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
    {
        return false;
    }
    object.iter().all(|(key, value)| match key.as_str() {
        "content" | "timestamp" | "context" | "document_id" | "strategy" | "update_mode" => {
            shape(value, "text")
        }
        "tags" => shape(value, "strings"),
        "entities" => shape(value, "objects"),
        "resolve_entities" => shape(value, "bool"),
        "observation_scopes" => shape(value, "scopes"),
        "metadata" => {
            value.is_null()
                || value
                    .as_object()
                    .is_some_and(|metadata| metadata.values().all(Value::is_string))
        }
        _ => false,
    })
}

fn valid_output(operation: Operation, value: &Value, bank: &str) -> bool {
    if value.get("error").is_some() || value.get("detail").is_some() {
        return false;
    }
    match operation {
        Operation::Recall => {
            value
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results
                        .iter()
                        .all(|result| result.get("text").is_some_and(Value::is_string))
                })
        }
        Operation::Reflect => value.get("text").is_some_and(Value::is_string),
        Operation::Retain => {
            value.get("success") == Some(&Value::Bool(true))
                && value.get("bank_id").and_then(Value::as_str) == Some(bank)
                && value.get("items_count").and_then(Value::as_u64).is_some()
                && value.get("async").is_some_and(Value::is_boolean)
        }
    }
}

fn sanitize_output(operation: Operation, value: &mut Value) {
    if matches!(operation, Operation::Reflect)
        && value
            .get("structured_output_error")
            .is_some_and(|error| !error.is_null())
    {
        value["structured_output_error"] = json!("Structured output unavailable");
    }
}

async fn operation_request(
    client: &reqwest::Client,
    config: &Config,
    binding: &Binding,
    operation: Operation,
    value: &Value,
) -> Result<reqwest::Request, Failure> {
    let secret = config.secret().await?;
    let url = config.url(&format!(
        "/v1/default/banks/{}/{}",
        binding.bank_id,
        operation.suffix()
    ))?;
    client
        .post(url)
        .header(header::AUTHORIZATION, secret)
        .header(header::ACCEPT, "application/json")
        .json(value)
        .build()
        .map_err(transport)
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/memory/retain", post(retain))
        .route("/v1/memory/recall", post(recall))
        .route("/v1/memory/reflect", post(reflect))
}

async fn retain(State(app): State<Arc<AppState>>, request: Request) -> Response {
    forward(app, request, Operation::Retain).await
}
async fn recall(State(app): State<Arc<AppState>>, request: Request) -> Response {
    forward(app, request, Operation::Recall).await
}
async fn reflect(State(app): State<Arc<AppState>>, request: Request) -> Response {
    forward(app, request, Operation::Reflect).await
}

async fn forward(app: Arc<AppState>, request: Request, operation: Operation) -> Response {
    let runtime = app.current();
    let config = &runtime.policy.config.hindsight;
    if !config.enabled {
        return json_error(
            StatusCode::NOT_FOUND,
            "memory_disabled",
            "Memory integration is disabled",
        );
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(config.timeout_ms);
    let result = tokio::time::timeout_at(deadline, async {
        let credential = authorize(&runtime, request.headers()).await?;
        let binding = config.bindings.get(&credential.name).ok_or_else(|| {
            json_error(
                StatusCode::FORBIDDEN,
                "memory_not_allowed",
                "Memory is not authorized",
            )
        })?;
        if matches!(operation, Operation::Retain) && !binding.allow_retain {
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "retain_not_allowed",
                "Retention is not authorized",
            ));
        }
        let _permit = app
            .hindsight_admission
            .try_acquire(&credential.class, credential.max_in_flight)
            .map_err(|_| {
                json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "memory_capacity",
                    "Memory capacity exceeded",
                )
            })?;
        if request.uri().query().is_some() {
            return Err(Failure::Request.response());
        }
        if request
            .headers()
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
        {
            return Err(Failure::Request.response());
        }
        if !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            })
        {
            return Err(json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "json_required",
                "application/json required; streaming is unsupported",
            ));
        }
        let bytes = to_bytes(request.into_body(), MAX_REQUEST_BYTES)
            .await
            .map_err(|_| Failure::TooLarge.response())?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| Failure::Request.response())?;
        validate_request(operation, &value).map_err(Failure::response)?;
        let client = runtime
            .hindsight_client
            .as_ref()
            .ok_or_else(|| Failure::Unavailable.response())?;
        let outgoing = operation_request(client, config, binding, operation, &value)
            .await
            .map_err(Failure::response)?;
        let response = client
            .execute(outgoing)
            .await
            .map_err(|error| transport(error).response())?;
        let (status, mut output) = json_response(response).await.map_err(Failure::response)?;
        if !valid_output(operation, &output, &binding.bank_id) {
            return Err(Failure::Response.response());
        }
        sanitize_output(operation, &mut output);
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(output.to_string()))
            .expect("static response headers"))
    })
    .await;
    match result {
        Ok(Ok(response)) => response,
        Ok(Err(response)) => response,
        Err(_) => Failure::Timeout.response(),
    }
}

pub(crate) fn status(config: &Config) -> Value {
    json!({"state":if config.enabled {"enabled"} else {"disabled"},
        "endpoint_configured":config.endpoint.is_some(),
        "secret_reference_configured":config.api_key_env.is_some() || config.api_key_file.is_some(),
        "timeout_ms":config.timeout_ms,"bindings_count":config.bindings.len(),
        "health":"not_checked","banks":{},"ok":!config.enabled})
}

async fn probe(
    client: &reqwest::Client,
    config: &Config,
    path: &str,
    authenticated: bool,
) -> Result<(), Failure> {
    tokio::time::timeout(Duration::from_millis(config.timeout_ms), async {
        let mut request = client
            .get(config.url(path)?)
            .header(header::ACCEPT, "application/json");
        if authenticated {
            request = request.header(header::AUTHORIZATION, config.secret().await?);
        }
        json_response(request.send().await.map_err(transport)?).await?;
        Ok(())
    })
    .await
    .map_err(|_| Failure::Timeout)?
}

pub(crate) async fn check(config: &Config) -> Value {
    let mut report = status(config);
    if !config.enabled {
        return report;
    }
    if config.validate().is_err() {
        report["health"] = json!("invalid_config");
        return report;
    }
    let Ok(client) = client() else {
        report["health"] = json!("unavailable");
        return report;
    };
    let health = probe(&client, config, "/health", false).await;
    report["health"] = json!(health.map_or_else(Failure::name, |()| "reachable"));
    let mut banks = Map::new();
    let unique: BTreeSet<_> = config
        .bindings
        .values()
        .map(|binding| &binding.bank_id)
        .collect();
    let mut ok = health.is_ok() && !unique.is_empty();
    for bank in unique {
        let result = probe(
            &client,
            config,
            &format!("/v1/default/banks/{bank}/stats"),
            true,
        )
        .await;
        ok &= result.is_ok();
        banks.insert(
            bank.clone(),
            json!(result.map_or_else(Failure::name, |()| "authorized")),
        );
    }
    report["banks"] = Value::Object(banks);
    report["ok"] = json!(ok);
    report
}

pub(crate) async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (command, path) = match args {
        [command, path] if matches!(command.as_str(), "status" | "check") => (command, path),
        _ => return Err("usage: slotline hindsight <status|check> CONFIG.yaml".into()),
    };
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|_| "Hindsight configuration unavailable")?;
    let config: crate::config::Config =
        serde_yaml::from_str(&text).map_err(|_| "invalid Hindsight configuration")?;
    config.hindsight.validate()?;
    let report = if command == "check" {
        check(&config.hindsight).await
    } else {
        status(&config.hindsight)
    };
    println!("{report}");
    if command == "check" && report["ok"] != true {
        return Err("Hindsight check failed; health is not bank authorization".into());
    }
    Ok(())
}

#[cfg(test)]
mod offline_tests {
    use super::*;

    #[test]
    fn hindsight_offline_missing_secret_reference_is_not_startup_dependency() {
        let config: Config =
            serde_json::from_value(json!({"enabled":true,"endpoint":"http://127.0.0.1:9"}))
                .unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn hindsight_offline_success_envelopes_are_not_error_passthrough() {
        assert!(!valid_output(
            Operation::Recall,
            &json!({"error":"private"}),
            "agent"
        ));
        assert!(!valid_output(
            Operation::Reflect,
            &json!({"error":"private"}),
            "agent"
        ));
        assert!(!valid_output(
            Operation::Retain,
            &json!({"success":false,"error":"private"}),
            "agent"
        ));
        let mut output = json!({"text":"authorized memory","structured_output":null,"structured_output_error":"private-error-sentinel"});
        sanitize_output(Operation::Reflect, &mut output);
        assert_eq!(output["text"], "authorized memory");
        assert_eq!(
            output["structured_output_error"],
            "Structured output unavailable"
        );
        assert!(!output.to_string().contains("sentinel"));
        assert!(valid_output(
            Operation::Recall,
            &json!({"results":[{"text":"owned memory"}]}),
            "agent"
        ));
        assert!(valid_output(
            Operation::Reflect,
            &json!({"text":"answer","structured_output":{"value":true}}),
            "agent"
        ));
        assert!(valid_output(
            Operation::Retain,
            &json!({"success":true,"bank_id":"agent","items_count":1,"async":true,"operation_id":"pending"}),
            "agent"
        ));
        assert!(!valid_output(
            Operation::Retain,
            &json!({"success":true,"bank_id":"other","items_count":1,"async":true}),
            "agent"
        ));
    }

    #[tokio::test]
    async fn hindsight_offline_outgoing_contract_uses_only_operator_authority() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
        std::fs::write(&path, "synthetic-hindsight-token").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = Config {
            enabled: true,
            endpoint: Some("http://127.0.0.1:9180/".into()),
            api_key_file: Some(path.to_str().unwrap().into()),
            ..Default::default()
        };
        let binding = Binding {
            bank_id: "agent-owned".into(),
            allow_retain: true,
        };
        let client = client().unwrap();
        for (operation, suffix, value) in [
            (
                Operation::Retain,
                "memories",
                json!({"items":[{"content":"explicit"}],"async":true}),
            ),
            (
                Operation::Recall,
                "memories/recall",
                json!({"query":"explicit","rerank":{"prune":false,"dedup":true}}),
            ),
            (
                Operation::Reflect,
                "reflect",
                json!({"query":"explicit","response_schema":{"type":"object"}}),
            ),
        ] {
            let request = operation_request(&client, &config, &binding, operation, &value)
                .await
                .ok()
                .unwrap();
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(
                request.url().as_str(),
                format!("http://127.0.0.1:9180/v1/default/banks/agent-owned/{suffix}")
            );
            assert_eq!(
                request.headers()[header::AUTHORIZATION],
                "Bearer synthetic-hindsight-token"
            );
            assert!(request.headers()[header::AUTHORIZATION].is_sensitive());
            assert_eq!(request.headers().len(), 3);
            assert_eq!(
                serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap())
                    .unwrap(),
                value
            );
        }
    }

    #[tokio::test]
    async fn hindsight_offline_secret_files_are_bounded_private_and_lazy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
        let mut config = Config {
            api_key_file: Some(path.to_str().unwrap().into()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        assert!(matches!(config.secret().await, Err(Failure::Secret)));
        std::fs::write(&path, "test-only-upstream-key\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let header = config.secret().await.ok().unwrap();
        assert_eq!(header, "Bearer test-only-upstream-key");
        assert!(header.is_sensitive());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(config.secret().await, Err(Failure::Secret)));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        for value in [
            "".into(),
            "key\nheader-injection".into(),
            "x".repeat(MAX_SECRET_BYTES as usize + 1),
        ] {
            std::fs::write(&path, value).unwrap();
            assert!(matches!(config.secret().await, Err(Failure::Secret)));
        }
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        config.api_key_file = Some(link.to_str().unwrap().into());
        assert!(matches!(config.secret().await, Err(Failure::Secret)));
        config.api_key_file = None;
        config.api_key_env = Some("SLOTLINE_HINDSIGHT_TEST_MISSING_51885".into());
        assert!(matches!(config.secret().await, Err(Failure::Secret)));
    }

    #[tokio::test]
    async fn hindsight_offline_response_matrix_and_chunked_cap() {
        for (status, content_type, text, expected) in [
            (200, "application/json", "{\"results\":[]}", None),
            (
                401,
                "application/json",
                "private-sentinel",
                Some("unauthorized"),
            ),
            (
                403,
                "application/json",
                "private-sentinel",
                Some("unauthorized"),
            ),
            (
                429,
                "application/json",
                "private-sentinel",
                Some("upstream_error"),
            ),
            (
                500,
                "application/json",
                "private-sentinel",
                Some("upstream_error"),
            ),
            (
                307,
                "application/json",
                "private-sentinel",
                Some("redirect_rejected"),
            ),
            (
                200,
                "text/event-stream",
                "private-sentinel",
                Some("invalid_response"),
            ),
            (
                200,
                "text/html",
                "private-sentinel",
                Some("invalid_response"),
            ),
            (
                200,
                "application/json",
                "private-sentinel",
                Some("invalid_response"),
            ),
            (200, "application/json", "[]", Some("invalid_response")),
        ] {
            let response = axum::http::Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .body(reqwest::Body::from(text))
                .unwrap();
            let result = json_response(response.into()).await;
            match (result, expected) {
                (Ok((status, output)), None) => {
                    assert_eq!(status, StatusCode::OK);
                    assert!(output["results"].is_array());
                }
                (Err(failure), Some(expected)) => {
                    assert_eq!(failure.name(), expected);
                    let body = to_bytes(failure.response().into_body(), MAX_RESPONSE_BYTES)
                        .await
                        .unwrap();
                    assert!(!String::from_utf8_lossy(&body).contains("sentinel"));
                }
                _ => panic!("response classification mismatch"),
            }
        }
        let chunks =
            futures_util::stream::iter((0..33).map(|_| Ok::<_, std::io::Error>(vec![b' '; 65536])));
        let response = axum::http::Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(reqwest::Body::wrap_stream(chunks))
            .unwrap();
        assert!(matches!(
            json_response(response.into()).await,
            Err(Failure::Response)
        ));
    }

    #[test]
    fn hindsight_offline_complete_request_fields_and_type_validation() {
        assert!(validate_request(Operation::Recall, &json!({"query":"q","types":["world"],"prefer_observations":true,
            "rerank":{"prune":false,"dedup":true},"budget":"mid","max_tokens":128,"trace":false,"query_timestamp":"2026-11-12T00:00:00Z",
            "include":{"entities":{}},"tags":["owned"],"tags_match":"all_strict","tag_groups":[{"tags":["owned"]}],
            "min_scores":{},"temporal_window":{}})).is_ok());
        assert!(validate_request(Operation::Reflect, &json!({"query":"q","context":"c","budget":"high","max_tokens":128,
            "include":{"facts":{}},"response_schema":{"type":"object"},"tags":["owned"],"tags_match":"any",
            "tag_groups":[],"fact_types":["world"],"exclude_mental_models":true,"exclude_mental_model_ids":[],
            "apply_all_directives":false,"reflect_search_observations_max_tokens":128,"reflect_search_observations_include_entities":false})).is_ok());
        assert!(validate_request(Operation::Retain, &json!({"items":[{"content":"text","context":"c","timestamp":"2026-11-12T00:00:00Z",
            "metadata":{"source":"test"},"document_id":"d","entities":[{"text":"Alice"}],"tags":["owned"],
            "observation_scopes":[["owned"]],"strategy":"default","update_mode":"append","resolve_entities":false}],"async":true,
            "document_tags":["owned"],"operation_id":"test"})).is_ok());
        for input in [
            json!({"query":"q","budget":"invalid"}),
            json!({"query":"q","tags_match":"invalid"}),
            json!({"query":"q","trace":"true"}),
            json!({"query":"q","tag_groups":[3]}),
        ] {
            assert!(validate_request(Operation::Recall, &input).is_err());
        }
        for rerank in [
            json!(false),
            json!({"prune":"false"}),
            json!({"bank_id":"other"}),
        ] {
            assert!(
                validate_request(Operation::Recall, &json!({"query":"q","rerank":rerank})).is_err()
            );
        }
        for scopes in [
            json!("combined"),
            json!("per_tag"),
            json!([["project-a"], ["project-b"]]),
        ] {
            assert!(
                validate_request(
                    Operation::Retain,
                    &json!({"items":[{"content":"text","observation_scopes":scopes}]})
                )
                .is_ok()
            );
        }
        assert!(
            validate_request(
                Operation::Retain,
                &json!({"items":[{"content":"text","observation_scopes":{}}]})
            )
            .is_err()
        );
    }
}
