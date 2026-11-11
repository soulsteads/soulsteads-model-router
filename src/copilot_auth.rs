use crate::config::{Config, CopilotAuthSource, CopilotConfig};
use reqwest::{Client, Url, redirect::Policy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const DEVICE_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const SESSION_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub(crate) const API_VERSION: &str = "2025-04-01";
pub(crate) const COPILOT_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
pub(crate) const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const AUTH_EDITOR_VERSION: &str = "vscode/1.85.1";
const AUTH_PLUGIN_VERSION: &str = "copilot/1.155.0";
const AUTH_USER_AGENT: &str = "GithubCopilot/1.155.0";
const MAX_AUTH_BYTES: usize = 16 * 1024;
const SESSION_EXPIRY_SKEW: u64 = 60;

#[derive(Clone)]
pub(crate) struct CopilotSession {
    pub(crate) bearer: String,
    pub(crate) expires_at: u64,
    pub(crate) api_endpoint: Url,
}

pub(crate) struct AuthenticatedSession {
    pub(crate) oauth_token: String,
    pub(crate) session: CopilotSession,
}

type SessionCell = Arc<Mutex<Option<CopilotSession>>>;
type SessionCache = Mutex<HashMap<String, SessionCell>>;
static SESSION_CACHE: OnceLock<SessionCache> = OnceLock::new();

fn session_cache() -> &'static SessionCache {
    SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn auth_client() -> Result<Client, &'static str> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "Copilot authentication transport is unavailable")
}

pub(crate) fn validate_provider_endpoint(value: &str) -> Result<Url, &'static str> {
    let mut url = Url::parse(value).map_err(|_| "Invalid Copilot API endpoint")?;
    let host = url.host_str().ok_or("Invalid Copilot API endpoint")?;
    let host_is_provider_api = host == "api.githubcopilot.com"
        || (host.starts_with("api.") && host.ends_with(".githubcopilot.com"));
    if url.scheme() != "https"
        || !host_is_provider_api
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("Invalid Copilot API endpoint");
    }
    url.set_path("/");
    Ok(url)
}

fn valid_secret(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_AUTH_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn managed_path(config: &CopilotConfig) -> Result<PathBuf, &'static str> {
    if let Some(path) = &config.managed_auth_file {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or("Copilot managed auth path is unavailable")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("slotline")
        .join("copilot-auth.json"))
}

fn load_oauth_token(config: &CopilotConfig) -> Result<String, &'static str> {
    let contents = match config.auth_source {
        CopilotAuthSource::Managed => {
            let path = managed_path(config)?;
            let store = crate::codex_auth::Store::open(&path)?;
            let value = store.read_json_value()?;
            value
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or("Copilot managed credential is unavailable")?
                .to_owned()
        }
        CopilotAuthSource::OAuthTokenFile => {
            let path = config
                .oauth_token_file
                .as_deref()
                .ok_or("Copilot OAuth token file is not configured")?;
            crate::codex_auth::Store::read_private_text(Path::new(path))?
        }
    };
    let token = contents.trim();
    if !valid_secret(token) {
        return Err("Copilot OAuth credential is invalid or unavailable");
    }
    Ok(token.to_owned())
}

fn unix_time() -> Result<u64, &'static str> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "System clock is before the Unix epoch")
}

fn cache_key(oauth_token: &str) -> String {
    let digest = Sha256::digest(oauth_token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn session_needs_refresh(expires_at: u64, now: u64) -> bool {
    expires_at <= now.saturating_add(SESSION_EXPIRY_SKEW)
}

fn parse_session(value: &Value, now: u64) -> Result<CopilotSession, &'static str> {
    let bearer = value
        .get("token")
        .and_then(Value::as_str)
        .ok_or("Copilot session response is invalid")?;
    if !valid_secret(bearer) {
        return Err("Copilot session response is invalid");
    }
    let expires_at = value
        .get("expires_at")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|number| *number >= 0.0)
                        .map(|n| n as u64)
                })
        })
        .ok_or("Copilot session response is invalid")?;
    if expires_at <= now {
        return Err("Copilot session is expired");
    }
    let endpoint = value
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(Value::as_str)
        .ok_or("Copilot session response is invalid")?;
    Ok(CopilotSession {
        bearer: bearer.to_owned(),
        expires_at,
        api_endpoint: validate_provider_endpoint(endpoint)?,
    })
}

fn github_auth_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("editor-version", AUTH_EDITOR_VERSION)
        .header("editor-plugin-version", AUTH_PLUGIN_VERSION)
        .header("user-agent", AUTH_USER_AGENT)
}

async fn refresh_session_at(
    client: &Client,
    url: &str,
    oauth_token: &str,
) -> Result<CopilotSession, &'static str> {
    let response = github_auth_headers(client.get(url))
        .header("authorization", format!("token {oauth_token}"))
        .send()
        .await
        .map_err(|_| "Copilot session refresh failed")?;
    if !response.status().is_success() {
        return Err("Copilot session refresh was rejected");
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|_| "Copilot session response is invalid")?;
    parse_session(&value, unix_time()?)
}

async fn session_for_token(oauth_token: &str) -> Result<CopilotSession, &'static str> {
    let client = auth_client()?;
    session_for_token_with(oauth_token, SESSION_URL, &client).await
}

async fn session_for_token_with(
    oauth_token: &str,
    url: &str,
    client: &Client,
) -> Result<CopilotSession, &'static str> {
    let key = cache_key(oauth_token);
    let cell = {
        let mut cache = session_cache().lock().await;
        if cache.len() > 64 {
            cache.retain(|_, cell| Arc::strong_count(cell) > 1);
        }
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut cached = cell.lock().await;
    let now = unix_time()?;
    if let Some(session) = cached.as_ref()
        && !session_needs_refresh(session.expires_at, now)
    {
        return Ok(session.clone());
    }
    let session = refresh_session_at(client, url, oauth_token).await?;
    *cached = Some(session.clone());
    Ok(session)
}

pub(crate) async fn session_for_config(
    config: &CopilotConfig,
) -> Result<AuthenticatedSession, &'static str> {
    let oauth_token = load_oauth_token(config)?;
    let session = session_for_token(&oauth_token).await?;
    Ok(AuthenticatedSession {
        oauth_token,
        session,
    })
}

pub(crate) async fn invalidate_session(oauth_token: &str, bearer: &str) {
    let key = cache_key(oauth_token);
    let cell = session_cache().lock().await.get(&key).cloned();
    if let Some(cell) = cell {
        let mut cached = cell.lock().await;
        if cached
            .as_ref()
            .is_some_and(|session| session.bearer == bearer)
        {
            *cached = None;
        }
    }
}

#[derive(serde::Deserialize)]
struct DeviceCodeReply {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

fn valid_verification_uri(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/login/device"
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn poll_error(value: &Value) -> Option<&str> {
    value.get("error").and_then(Value::as_str)
}

struct DeviceEndpoints {
    device_code_url: String,
    access_token_url: String,
}

impl DeviceEndpoints {
    fn github() -> Self {
        Self {
            device_code_url: DEVICE_CODE_URL.into(),
            access_token_url: ACCESS_TOKEN_URL.into(),
        }
    }
}

fn next_poll_interval(interval: u64, error: &str) -> Result<u64, &'static str> {
    match error {
        "authorization_pending" => Ok(interval),
        "slow_down" => Ok(interval.saturating_add(5)),
        _ => Err("GitHub device authorization polling failed"),
    }
}

async fn acquire_device_token_with(
    client: &Client,
    endpoints: &DeviceEndpoints,
    display: impl FnOnce(&str, &str),
) -> Result<String, &'static str> {
    let device_response = github_auth_headers(client.post(&endpoints.device_code_url))
        .json(&json!({"client_id": DEVICE_CLIENT_ID, "scope": "read:user"}))
        .send()
        .await
        .map_err(|_| "GitHub device authorization request failed")?;
    if !device_response.status().is_success() {
        return Err("GitHub device authorization was rejected");
    }
    let device: DeviceCodeReply = device_response
        .json()
        .await
        .map_err(|_| "GitHub device authorization response is invalid")?;
    if device.device_code.is_empty()
        || device.user_code.is_empty()
        || !valid_verification_uri(&device.verification_uri)
        || device.expires_in == 0
        || device.expires_in > 3600
    {
        return Err("GitHub device authorization response is invalid");
    }
    let mut interval = device.interval.unwrap_or(5).max(1);
    if interval > 120 {
        return Err("GitHub device authorization interval is invalid");
    }
    display(&device.verification_uri, &device.user_code);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("GitHub device authorization expired");
        }
        tokio::time::sleep(Duration::from_secs(
            interval.min(remaining.as_secs().max(1)),
        ))
        .await;
        if tokio::time::Instant::now() >= deadline {
            return Err("GitHub device authorization expired");
        }
        let response = github_auth_headers(client.post(&endpoints.access_token_url))
            .json(&json!({
                "client_id": DEVICE_CLIENT_ID,
                "device_code": device.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
            }))
            .send()
            .await
            .map_err(|_| "GitHub device authorization polling failed")?;
        if !response.status().is_success() {
            return Err("GitHub device authorization polling was rejected");
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| "GitHub device authorization response is invalid")?;
        if let Some(token) = value.get("access_token").and_then(Value::as_str) {
            if !valid_secret(token) {
                return Err("GitHub OAuth token response is invalid");
            }
            return Ok(token.to_owned());
        }
        match poll_error(&value) {
            Some("authorization_pending") => {
                interval = next_poll_interval(interval, "authorization_pending")?;
            }
            Some("slow_down") => {
                interval = next_poll_interval(interval, "slow_down")?;
            }
            Some("access_denied") => return Err("GitHub device authorization was denied"),
            Some("expired_token") => return Err("GitHub device authorization expired"),
            _ => return Err("GitHub device authorization polling failed"),
        }
    }
}

async fn device_login(
    config: &CopilotConfig,
    display: impl FnOnce(&str, &str),
) -> Result<(), &'static str> {
    if config.auth_source != CopilotAuthSource::Managed {
        return Err("Copilot login requires managed auth_source; OAuth token files are read-only");
    }
    let path = managed_path(config)?;
    let store = crate::codex_auth::Store::open(&path)?;
    let token =
        acquire_device_token_with(&auth_client()?, &DeviceEndpoints::github(), display).await?;
    store
        .write_json_value(&json!({"access_token": token}))
        .await?;
    Ok(())
}

fn auth_present(config: &CopilotConfig) -> bool {
    load_oauth_token(config).is_ok()
}

pub(crate) async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [command, config_path] = args else {
        return Err("usage: slotline copilot login|status|logout CONFIG.yaml".into());
    };
    if !matches!(command.as_str(), "login" | "status" | "logout") {
        return Err("usage: slotline copilot login|status|logout CONFIG.yaml".into());
    }
    let bytes = std::fs::read(config_path)?;
    let config: Config = serde_yaml::from_slice(&bytes)?;
    config.copilot.validate().map_err(std::io::Error::other)?;
    match command.as_str() {
        "login" => {
            device_login(&config.copilot, |url, code| {
                eprintln!("Open {url} and enter {code}. This code is only for this sign-in.");
            })
            .await
            .map_err(std::io::Error::other)?;
            println!("GitHub Copilot managed authentication saved.");
        }
        "status" => {
            println!(
                "provider: github-copilot\nsource: {}\nstatus: {}",
                match config.copilot.auth_source {
                    CopilotAuthSource::Managed => "managed",
                    CopilotAuthSource::OAuthTokenFile => "read-only OAuth token file",
                },
                if auth_present(&config.copilot) {
                    "credential available"
                } else {
                    "credential unavailable"
                }
            );
        }
        "logout" => {
            if config.copilot.auth_source != CopilotAuthSource::Managed {
                return Err(
                    "Copilot OAuth token-file auth is read-only; remove it at its source".into(),
                );
            }
            let store = crate::codex_auth::Store::open(&managed_path(&config.copilot)?)?;
            store.remove().await.map_err(std::io::Error::other)?;
            println!("GitHub Copilot managed authentication removed.");
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_endpoint_accepts_only_github_copilot_api_hosts() {
        assert_eq!(
            validate_provider_endpoint("https://api.githubcopilot.com")
                .unwrap()
                .host_str(),
            Some("api.githubcopilot.com")
        );
        assert_eq!(
            validate_provider_endpoint("https://api.enterprise.githubcopilot.com/")
                .unwrap()
                .host_str(),
            Some("api.enterprise.githubcopilot.com")
        );
        for url in [
            "http://api.githubcopilot.com",
            "https://api.githubcopilot.com.evil.example",
            "https://user@api.githubcopilot.com",
            "https://api.githubcopilot.com:8443",
            "https://api.githubcopilot.com/path",
            "https://api.githubcopilot.com/?next=https://evil.example",
        ] {
            assert!(validate_provider_endpoint(url).is_err());
        }
    }

    #[test]
    fn session_expiry_refreshes_before_expiration() {
        assert!(!session_needs_refresh(1_200, 1_139));
        assert!(session_needs_refresh(1_200, 1_140));
        assert!(session_needs_refresh(1_200, 1_200));
    }

    #[test]
    fn device_polling_observes_pending_and_slow_down_intervals() {
        assert_eq!(next_poll_interval(5, "authorization_pending"), Ok(5));
        assert_eq!(next_poll_interval(5, "slow_down"), Ok(10));
        assert!(next_poll_interval(5, "access_denied").is_err());
    }

    #[tokio::test]
    async fn session_refresh_is_singleflight_rotates_by_oauth_source_and_invalidates() {
        use axum::{Json, Router, routing::get};
        use futures_util::future::join_all;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let call_counter = calls.clone();
        let expires_at = unix_time().unwrap() + 3_600;
        let app = Router::new().route(
            "/session",
            get(move |headers: axum::http::HeaderMap| {
                assert!(
                    headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.starts_with("token synthetic-oauth"))
                );
                assert_eq!(headers.get("editor-version").unwrap(), "vscode/1.85.1");
                assert_eq!(
                    headers.get("editor-plugin-version").unwrap(),
                    "copilot/1.155.0"
                );
                assert_eq!(headers.get("user-agent").unwrap(), "GithubCopilot/1.155.0");
                assert_eq!(headers.get("accept").unwrap(), "application/json");
                assert_eq!(headers.get("content-type").unwrap(), "application/json");
                let counter = call_counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Json(json!({
                        "token": "synthetic-session-token",
                        "expires_at": expires_at,
                        "endpoints": {"api": "https://api.githubcopilot.com"}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .unwrap();
        let url = format!("http://{address}/session");
        let sessions = join_all(
            (0..8).map(|_| session_for_token_with("synthetic-oauth-singleflight", &url, &client)),
        )
        .await;
        assert!(sessions.iter().all(Result::is_ok));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        session_for_token_with("synthetic-oauth-singleflight", &url, &client)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        session_for_token_with("synthetic-oauth-rotated", &url, &client)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        invalidate_session("synthetic-oauth-singleflight", "synthetic-session-token").await;
        session_for_token_with("synthetic-oauth-singleflight", &url, &client)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        server.abort();
    }

    #[tokio::test]
    async fn device_flow_uses_loopback_mock_and_preserves_reference_grant() {
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let polls = Arc::new(AtomicUsize::new(0));
        let poll_counter = polls.clone();
        let app = Router::new()
            .route(
                "/device",
                post(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers.get("editor-version").unwrap(), "vscode/1.85.1");
                    assert_eq!(
                        headers.get("editor-plugin-version").unwrap(),
                        "copilot/1.155.0"
                    );
                    assert_eq!(headers.get("user-agent").unwrap(), "GithubCopilot/1.155.0");
                    Json(json!({
                        "device_code": "synthetic-device-code",
                        "user_code": "ABCD-EFGH",
                        "verification_uri": "https://github.com/login/device",
                        "expires_in": 30,
                        "interval": 1
                    }))
                }),
            )
            .route(
                "/token",
                post(
                    move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                        assert_eq!(headers.get("editor-version").unwrap(), "vscode/1.85.1");
                        assert_eq!(
                            headers.get("editor-plugin-version").unwrap(),
                            "copilot/1.155.0"
                        );
                        assert_eq!(headers.get("user-agent").unwrap(), "GithubCopilot/1.155.0");
                        assert!(headers.get("authorization").is_none());
                        assert_eq!(body["client_id"], DEVICE_CLIENT_ID);
                        assert_eq!(body["device_code"], "synthetic-device-code");
                        assert_eq!(
                            body["grant_type"],
                            "urn:ietf:params:oauth:grant-type:device_code"
                        );
                        let attempt = poll_counter.fetch_add(1, Ordering::Relaxed);
                        async move {
                            if attempt == 0 {
                                Json(json!({"error": "authorization_pending"}))
                            } else {
                                Json(json!({"access_token": "synthetic-github-oauth-token"}))
                            }
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .unwrap();
        let endpoints = DeviceEndpoints {
            device_code_url: format!("http://{address}/device"),
            access_token_url: format!("http://{address}/token"),
        };
        let display = std::sync::Arc::new(std::sync::Mutex::new(None));
        let display_copy = display.clone();
        let token = acquire_device_token_with(&client, &endpoints, move |url, code| {
            *display_copy.lock().unwrap() = Some((url.to_owned(), code.to_owned()));
        })
        .await
        .unwrap();
        server.abort();
        assert_eq!(token, "synthetic-github-oauth-token");
        assert_eq!(polls.load(Ordering::Relaxed), 2);
        assert_eq!(
            *display.lock().unwrap(),
            Some((
                "https://github.com/login/device".to_owned(),
                "ABCD-EFGH".to_owned()
            ))
        );
    }

    #[test]
    fn session_parser_requires_a_future_expiry_and_valid_discovered_endpoint() {
        let session = parse_session(
            &json!({
                "token": "synthetic-session-token",
                "expires_at": 1_200,
                "endpoints": {"api": "https://api.enterprise.githubcopilot.com"}
            }),
            1_000,
        )
        .unwrap();
        assert_eq!(session.expires_at, 1_200);
        assert_eq!(
            session.api_endpoint.host_str(),
            Some("api.enterprise.githubcopilot.com")
        );

        assert!(
            parse_session(
                &json!({
                    "token": "synthetic-session-token",
                    "expires_at": 1_000,
                    "endpoints": {"api": "https://api.githubcopilot.com"}
                }),
                1_000,
            )
            .is_err()
        );
    }
}
