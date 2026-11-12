mod adaptive;
mod audit;
mod audit_identity;
mod audit_sink;
mod auth_control;
mod codex;
mod codex_auth;
#[cfg(test)]
mod codex_cli_auth_tests;
mod config;
mod copilot;
mod copilot_auth;
mod key_manager;
mod status;
mod status_cli;

mod embedding_context;
mod handlers;
mod passthrough;
mod request_logging;
mod request_validation;
mod upstream;
mod validation;
use request_validation::*;
use upstream::*;
use validation::*;
mod sse;

use auth_control::*;
use embedding_context::*;
use handlers::*;
use sse::*;
use status::*;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use config::{Endpoint, MatcherSource, Policy, RouteKind};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io,
    net::SocketAddr,
    ops::Deref,
    os::fd::AsRawFd,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokenizers::Tokenizer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

mod backend_telemetry;
mod state;
use state::*;

async fn reload_policy(app: &AppState) -> Result<(), String> {
    // Lock before reading the file, so a slower earlier reload cannot publish
    // over a later accepted update. No file or verifier I/O runs on requests.
    let _guard = app.reload_lock.lock().await;
    let path = app.path.clone();
    let candidate = tokio::task::spawn_blocking(move || config::load(&path))
        .await
        .map_err(|e| format!("policy reload task: {e}"))??;
    let current = app.current();
    validate_reload(&current.policy, &candidate)?;
    let next = make_runtime(candidate, Some(&current))?;
    info!(build_source_sha256=audit_identity::BUILD, old_policy_sha256=%current.policy_digest, policy_sha256=%next.policy_digest, old_policy_generation=current.policy_generation, policy_generation=next.policy_generation, "policy activated");
    *app.current.write().unwrap() = next;
    Ok(())
}

async fn policy_control_loop(app: Arc<AppState>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let mut command = [0u8; 7];
                let accepted =
                    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut command))
                        .await;
                match accepted {
                    Ok(Ok(_)) if command == *b"status\n" => {
                        let payload = status_payload(&app.current()).to_string();
                        let _ = tokio::time::timeout(
                            Duration::from_secs(3),
                            stream.write_all(payload.as_bytes()),
                        )
                        .await;
                    }
                    Ok(Ok(_)) if command == *b"reload\n" => {
                        let success = match reload_policy(&app).await {
                            Ok(()) => true,
                            Err(error) => {
                                warn!(error=%error, "policy reload rejected");
                                false
                            }
                        };
                        let _ = stream
                            .write_all(if success { b"ok\n" } else { b"no\n" })
                            .await;
                    }
                    _ => {
                        let _ = stream.write_all(b"no\n").await;
                    }
                }
            }
            Err(error) => warn!(error=%error, "policy control accept failed"),
        }
    }
}

const MAX_AFFINITY_LEASES: usize = 4096;

// Reload deliberately supports catalog/selection edits only. Exact route order
// and configuration keep every indexed observation, counter, lease and shared
// physical semaphore meaningful while older generations drain.
fn validate_reload(current: &Policy, candidate: &Policy) -> Result<(), String> {
    if current.config.routing_policies != candidate.config.routing_policies {
        return Err("routing policy authority cannot change on reload; restart required".into());
    }
    if current.config.codex != candidate.config.codex {
        return Err("Codex authority cannot change on reload; restart required".into());
    }
    if current.config.copilot != candidate.config.copilot {
        return Err("Copilot authority cannot change on reload; restart required".into());
    }
    if current.config.bind != candidate.config.bind || current.config.port != candidate.config.port
    {
        return Err("listener authority cannot change on reload".into());
    }
    if current.config.key_store != candidate.config.key_store
        || (current.config.key_store.is_none()
            && (current.config.credentials != candidate.config.credentials
                || current
                    .credentials
                    .iter()
                    .zip(&candidate.credentials)
                    .any(|(old, new)| old.key != new.key)))
    {
        return Err("credential authority cannot change on reload".into());
    }
    if current.config.routes != candidate.config.routes {
        return Err("route identity, order and capacity cannot change on reload".into());
    }
    if current
        .routes
        .iter()
        .zip(&candidate.routes)
        .any(|(old, new)| old.key != new.key)
    {
        return Err("route credential material cannot change on reload".into());
    }
    if candidate.config.routes.iter().any(|route| {
        let adapter = route.adapter_version.to_ascii_lowercase();
        !route.model.starts_with("qwen")
            || route.name.contains("codex")
            || route.name.contains("copilot")
            || route.name.contains("copilot")
            || adapter.contains("codex")
            || adapter.contains("copilot")
            || adapter.contains("copilot")
    }) || candidate.config.aliases.keys().any(|name| {
        let name = name.to_ascii_lowercase();
        name.contains("codex")
            || name.contains("copilot")
            || name.contains("copilot")
            || name.starts_with("gpt")
            || name.starts_with("claude")
            || name.starts_with("gemini")
            || name.starts_with("openai")
    }) {
        return Err("reload accepts only reviewed local Qwen routes".into());
    }
    Ok(())
}

fn physical_route_index(policy: &Policy, idx: usize) -> Option<usize> {
    let name = policy.routes[idx].config.physical_route.as_deref()?;
    policy
        .routes
        .iter()
        .position(|route| route.config.name == name)
}

fn physical_route_ready(runtime: &Runtime, idx: usize, observations: &[RouteObservation]) -> bool {
    let Some(physical_idx) = physical_route_index(&runtime.policy, idx) else {
        return true;
    };
    let physical = &observations[physical_idx];
    physical.status == RouteStatus::Serving
        && physical.checked.is_some_and(|time| {
            time.elapsed() <= Duration::from_secs(runtime.policy.config.refresh_seconds * 3)
        })
        && physical
            .qualified
            .is_some_and(|time| time.elapsed() <= Duration::from_secs(900))
}

fn effective_route_status(
    runtime: &Runtime,
    idx: usize,
    observations: &[RouteObservation],
) -> RouteStatus {
    if !physical_route_ready(runtime, idx, observations)
        && observations[idx].status == RouteStatus::Serving
    {
        RouteStatus::Stale
    } else {
        observations[idx].status
    }
}

// Keep the direct Axum Response error contract at this handler boundary.
#[allow(clippy::result_large_err)]
fn affinity_key(
    headers: &HeaderMap,
    credential: &config::Credential,
    alias: &str,
) -> Result<Option<[u8; 32]>, Response> {
    let mut hasher = Sha256::new();
    hasher.update(credential.name.as_bytes());
    hasher.update([0]);
    hasher.update(alias.as_bytes());
    let mut values = headers.get_all("x-slotline-session").iter();
    if let Some(value) = values.next() {
        if values.next().is_some() {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_session",
                "Exactly one X-Slotline-Session value is allowed",
            ));
        }
        let bytes = value.as_bytes();
        if bytes.is_empty() || bytes.len() > 128 || !bytes.iter().all(u8::is_ascii_graphic) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_session",
                "X-Slotline-Session must be 1..128 printable ASCII bytes",
            ));
        }
        // Validate caller-controlled metadata, but do not let it split an
        // authenticated profile's lease into separately placed conversations.
        if !credential.agent_affinity {
            hasher.update(b"session");
            hasher.update(bytes);
        }
    } else if !credential.agent_affinity {
        return Ok(None);
    }
    if credential.agent_affinity {
        hasher.update(b"agent");
    }
    Ok(Some(hasher.finalize().into()))
}

fn record_attempt(runtime: &Runtime, idx: usize) {
    runtime.route_attempts[idx].fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let mut recent = runtime.recent_routes.lock().unwrap();
    recent.push_back((now, idx));
    while recent
        .front()
        .is_some_and(|(at, _)| at.elapsed() > Duration::from_secs(300))
        || recent.len() > 10000
    {
        recent.pop_front();
    }
}
fn record_skip(runtime: &Runtime, _alias: &str, idx: usize, reason: &'static str) {
    *runtime
        .route_skips
        .lock()
        .unwrap()
        .entry((idx, reason))
        .or_default() += 1;
}
fn traffic_counts(runtime: &Runtime) -> (Vec<u64>, Vec<u64>) {
    let totals = runtime
        .route_attempts
        .iter()
        .map(|x| x.load(Ordering::Relaxed))
        .collect();
    let mut recent = vec![0u64; runtime.policy.routes.len()];
    for (at, idx) in runtime.recent_routes.lock().unwrap().iter() {
        if at.elapsed() <= Duration::from_secs(300) {
            recent[*idx] += 1;
        }
    }
    (totals, recent)
}
fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}
fn set_status(runtime: &Runtime, idx: usize, status: RouteStatus, reason: &'static str) {
    let old = {
        let mut observations = runtime.observations.lock().unwrap();
        let old = observations[idx].status;
        observations[idx].status = status;
        old
    };
    if old != status {
        info!(route=%runtime.policy.routes[idx].config.name,from=old.name(),to=status.name(),reason=reason,"route health changed");
    }
}

fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    let mut response = Response::new(Body::from(
        json!({"error":{"message":message,"type":"invalid_request_error","code":code}}).to_string(),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    }
    response
}
fn unauthorized() -> Response {
    json_error(
        StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "Invalid API key",
    )
}

// Axum Response is deliberately returned directly to preserve handler error contracts.
#[allow(clippy::result_large_err)]
async fn authenticate(
    runtime: &Runtime,
    headers: &HeaderMap,
) -> Result<Option<Arc<CredentialState>>, Response> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let Some(value) = value
        .to_str()
        .ok()
        .and_then(|header| header.strip_prefix("Bearer "))
    else {
        return Ok(None);
    };
    let value = value.as_bytes();
    if value.len() > 4096 {
        return Ok(None);
    }
    let auth = runtime.auth.read().unwrap().clone();
    if runtime.policy.config.key_store.is_some() {
        let Some(snapshot) = &auth.snapshot else {
            return Ok(None);
        };
        let snapshot = snapshot.clone();
        let permit = runtime
            .auth_limiter
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "auth_capacity",
                    "Authentication capacity exceeded",
                )
            })?;
        let raw = String::from_utf8_lossy(value).into_owned();
        let id = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            key_manager::verify(&snapshot, &raw).map(|policy| policy.id.clone())
        })
        .await
        .ok()
        .flatten();
        return Ok(id.and_then(|id| auth.credentials.get(&id).cloned()));
    }
    let mut found = None;
    for item in &runtime.policy.credentials {
        if item
            .key
            .as_ref()
            .is_some_and(|key| value.len() == key.len() && bool::from(value.ct_eq(key)))
        {
            found = auth.credentials.get(&item.config.name).cloned();
        }
    }
    Ok(found)
}
fn rate_check(credential: &CredentialState) -> bool {
    let mut rate = credential.rate.lock().unwrap();
    let now = Instant::now();
    rate.tokens = (rate.tokens
        + now.duration_since(rate.last).as_secs_f64() * credential.requests_per_minute as f64
            / 60.0)
        .min(credential.burst as f64);
    rate.last = now;
    if rate.tokens < 1.0 {
        false
    } else {
        rate.tokens -= 1.0;
        true
    }
}
fn matcher_id(
    runtime: &Runtime,
    credential: &config::Credential,
    headers: &HeaderMap,
) -> Option<String> {
    for matcher in &runtime.policy.matchers {
        if matcher.config.class != credential.class {
            continue;
        }
        let input = match matcher.config.source {
            MatcherSource::CredentialName => Some(credential.name.as_str()),
            MatcherSource::UserAgent => headers
                .get(header::USER_AGENT)
                .and_then(|h| h.to_str().ok()),
        };
        if let Some(input) = input
            && input.len() <= 128
            && matcher.regex.is_match(input)
        {
            return Some(matcher.config.id.clone());
        }
    }
    None
}
// Axum Response is deliberately returned directly to preserve handler error contracts.
#[allow(clippy::result_large_err)]
async fn authorize(
    runtime: &Runtime,
    headers: &HeaderMap,
) -> Result<Arc<CredentialState>, Response> {
    let cfg = authenticate(runtime, headers)
        .await?
        .ok_or_else(unauthorized)?;
    if cfg.expires_at.is_some_and(|expiry| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|duration| duration.as_secs() as i64 >= expiry)
    }) {
        return Err(unauthorized());
    }
    if !rate_check(&cfg) {
        return Err(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            "Rate limit exceeded",
        ));
    }
    Ok(cfg)
}

async fn models(State(runtime): State<Arc<Runtime>>, headers: HeaderMap) -> Response {
    let credential = match authorize(&runtime, &headers).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let mut models = Vec::new();
    for (name, alias) in &runtime.policy.config.aliases {
        if credential.aliases.contains(name) {
            let mode = if alias.endpoint == Endpoint::Embedding {
                "embedding"
            } else {
                "chat"
            };
            models.push(json!({"id":name,"object":"model","created":0,"owned_by":"slotline","context_length":alias.context_limit,"model_info":{"mode":mode,"max_input_tokens":alias.context_limit,"max_total_tokens":alias.context_limit}}));
        }
    }
    for (name, policy) in &runtime.policy.config.routing_policies {
        let target_is_usable = |target: &str| {
            [
                config::RoutingEndpoint::Chat,
                config::RoutingEndpoint::Responses,
            ]
            .into_iter()
            .any(|endpoint| {
                model_routing::target_usable(&runtime, &credential.config, target, endpoint)
            })
        };
        let rule_is_usable = |rule: &config::RoutingRule| {
            (rule.predicate.credential_classes.is_empty()
                || rule
                    .predicate
                    .credential_classes
                    .contains(&credential.config.class))
                && (if rule.predicate.endpoints.is_empty() {
                    target_is_usable(&rule.target)
                } else {
                    rule.predicate.endpoints.iter().copied().any(|endpoint| {
                        model_routing::target_usable(
                            &runtime,
                            &credential.config,
                            &rule.target,
                            endpoint,
                        )
                    })
                })
        };
        let unmatched_is_usable = if let Some(judge) = &policy.decision_model {
            model_routing::target_usable(
                &runtime,
                &credential.config,
                judge,
                config::RoutingEndpoint::Chat,
            ) && policy
                .decision_targets
                .iter()
                .any(|target| target_is_usable(target))
        } else {
            policy
                .default_target
                .as_deref()
                .is_some_and(target_is_usable)
        };
        if credential.aliases.contains(name)
            && (policy.rules.iter().any(rule_is_usable) || unmatched_is_usable)
        {
            models.push(json!({"id":name,"object":"model","created":0,"owned_by":"slotline-routing","model_info":{"mode":"routing"}}));
        }
    }
    if copilot::eligible(&credential, &runtime.policy.config.copilot) {
        for model in &runtime.policy.config.copilot.models {
            if credential.aliases.contains(&model.alias) {
                let mode = match model.endpoint {
                    config::CopilotEndpoint::ChatCompletions => "chat",
                    config::CopilotEndpoint::Responses => "responses",
                };
                models.push(json!({
                    "id": model.alias,
                    "object": "model",
                    "created": 0,
                    "owned_by": "github-copilot",
                    "model_info": {"mode": mode}
                }));
            }
        }
    }
    if codex::eligible(&credential, &runtime.policy.config.codex) {
        models.push(json!({"id":runtime.policy.config.codex.alias,"object":"model","created":0,"owned_by":"slotline-codex","model_info":{"mode":"chat"}}));
    }
    models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let mut response = Response::new(Body::from(
        json!({"object":"list","data":models}).to_string(),
    ));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

mod model_routing;
use model_routing::policy_target;
mod routing;
use routing::*;

async fn current_models(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    models(State(app.current()), headers).await
}
async fn current_chat(State(app): State<Arc<AppState>>, req: axum::extract::Request) -> Response {
    passthrough::dispatch(app.current(), req, "/chat/completions").await
}
async fn current_responses(
    State(app): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    passthrough::dispatch(app.current(), req, "/responses").await
}
async fn unsupported_responses_websocket() -> Response {
    json_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_endpoint",
        "Responses WebSocket transport is not supported",
    )
}
async fn current_embeddings(
    State(app): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    passthrough::dispatch(app.current(), req, "/embeddings").await
}
async fn current_health(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    health(State(app.current()), headers).await
}
async fn current_status(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    status(State(app.current()), headers).await
}
async fn current_metrics(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    metrics(State(app.current()), headers).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "routing") {
        adaptive::run_cli(&args[1..])?;
        return Ok(());
    }
    tracing_subscriber::fmt()
        .json()
        .event_format(audit_sink::formatter())
        .with_writer(audit_sink::Sink::start(std::io::stdout())?)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "slotline=info".into()),
        )
        .init();
    if args.first().is_some_and(|arg| arg == "codex") {
        return codex_auth::run(&args[1..]).await;
    }
    if args.first().is_some_and(|arg| arg == "copilot") {
        return copilot_auth::run(&args[1..]).await;
    }
    if args.first().is_some_and(|arg| arg == "keys") {
        return key_manager::run(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "status") {
        let default = status_cli::default_policy_path();
        let (policy, json) = match &args[1..] {
            [] => (default.as_path(), false),
            [flag] if flag == "--json" => (default.as_path(), true),
            [path] => (Path::new(path), false),
            [path, flag] if flag == "--json" => (Path::new(path), true),
            _ => return Err("usage: slotline status [CONFIG.yaml] [--json]".into()),
        };
        return status_cli::run(policy, json).await;
    }
    if let [command, config_path] = args.as_slice()
        && command == "reload"
    {
        let policy = config::load(Path::new(config_path))?;
        let socket = policy_control_path(
            Path::new(config_path),
            policy.config.key_store.as_deref().map(Path::new),
        );
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        use std::io::{Read, Write};
        stream.write_all(b"reload\n")?;
        let mut result = [0u8; 3];
        stream.read_exact(&mut result)?;
        if &result != b"ok\n" {
            return Err("policy reload rejected; running generation unchanged".into());
        }
        println!("slotline policy reloaded");
        return Ok(());
    }
    let (check, path) = match args.as_slice() {
        [path] => (false, PathBuf::from(path)),
        [flag, path] if flag == "--check-config" => (true, PathBuf::from(path)),
        _ => return Err("usage: slotline [--check-config|reload] CONFIG.yaml | status [CONFIG.yaml] [--json] | keys ...".into()),
    };
    let policy = config::load(&path)?;
    if check {
        println!("slotline configuration valid");
        return Ok(());
    }
    let policy_socket_store = policy.config.key_store.clone();
    let app_state = Arc::new(AppState::new(path.clone(), policy)?);
    let runtime = app_state.current();
    info!(build_source_sha256=audit_identity::BUILD, build_source_revision=env!("SLOTLINE_SOURCE_REVISION"), build_source_state=env!("SLOTLINE_SOURCE_STATE"), policy_sha256=%runtime.policy_digest, policy_generation=runtime.policy_generation, "policy activated");
    let address = SocketAddr::new(runtime.policy.config.bind, runtime.policy.config.port);
    let listener = tokio::net::TcpListener::bind(address).await?;
    let mut control_guard = None;
    let mut store_lock = None;
    if let Some(path) = &runtime.policy.config.key_store {
        let store = PathBuf::from(path);
        store_lock = Some(lock_auth_store(&store)?);
        let (control, socket) = bind_auth_control(&store)?;
        control_guard = Some(ControlSocketGuard::new(socket)?);
        tokio::spawn(auth_reload_loop(app_state.clone(), store, control));
    }
    let (policy_control, policy_socket) =
        bind_policy_control(&path, policy_socket_store.as_deref().map(Path::new))?;
    let policy_control_guard = ControlSocketGuard::new(policy_socket)?;
    tokio::spawn(policy_control_loop(app_state.clone(), policy_control));
    tokio::spawn(refresh_loop(app_state.clone()));
    let app = Router::new()
        .route("/v1/models", get(current_models))
        .route("/v1/chat/completions", post(current_chat))
        .route(
            "/v1/responses",
            post(current_responses).get(unsupported_responses_websocket),
        )
        .route("/v1/embeddings", post(current_embeddings))
        .route("/healthz", get(current_health))
        .route("/status", get(current_status))
        .route("/metrics", get(current_metrics))
        .with_state(app_state)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn(request_logging::correlate));
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    info!(bind=%address,start_epoch=epoch,"slotline listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    drop(control_guard);
    drop(policy_control_guard);
    drop(store_lock);
    Ok(())
}

#[cfg(test)]
mod adaptive_tests;
#[cfg(test)]
mod codex_integration_tests;
#[cfg(test)]
mod codex_tests;
#[cfg(test)]
mod routing_policy_tests;

#[cfg(test)]
mod tests;
