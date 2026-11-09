use crate::config::CodexConfig;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_ERROR: &str = "Codex credentials unavailable; run codex login locally";
const STORAGE_ERROR: &str = "Unsafe or unavailable Codex private account storage";
const MAX_AUTH: usize = 128 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Account {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) id_token: String,
    pub(crate) account_id: String,
    pub(crate) expires_at: u64,
    #[serde(default)]
    pub(crate) refresh_failed: bool,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn safe_secret(secret: &str) -> bool {
    !secret.is_empty()
        && secret.len() <= 32768
        && secret.bytes().all(|byte| byte.is_ascii_graphic())
}

fn claims(token: &str) -> Result<Value, &'static str> {
    let encoded = token.split('.').nth(1).ok_or(AUTH_ERROR)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| AUTH_ERROR)?;
    serde_json::from_slice(&bytes).map_err(|_| AUTH_ERROR)
}

fn account_claim(token: &str) -> Result<String, &'static str> {
    let claims = claims(token)?;
    let account = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|value| safe_secret(value) && value.len() <= 256)
        .ok_or(AUTH_ERROR)?;
    Ok(account.to_owned())
}

impl Account {
    fn validate(&self) -> Result<(), &'static str> {
        if !safe_secret(&self.access_token)
            || !safe_secret(&self.refresh_token)
            || !safe_secret(&self.id_token)
            || self.account_id != account_claim(&self.id_token)?
            || self.refresh_failed
            || self.expires_at == 0
        {
            return Err(AUTH_ERROR);
        }
        Ok(())
    }

    fn from_token(value: &Value, previous: Option<&Self>) -> Result<Self, &'static str> {
        let access_token = value["access_token"].as_str().ok_or(AUTH_ERROR)?.to_owned();
        let refresh_token = match value.get("refresh_token") {
            Some(value) => value.as_str().ok_or(AUTH_ERROR)?,
            None => previous
                .map(|old| old.refresh_token.as_str())
                .ok_or(AUTH_ERROR)?,
        }
        .to_owned();
        let id_token = match value.get("id_token") {
            Some(value) => value.as_str().ok_or(AUTH_ERROR)?,
            None => previous
                .map(|old| old.id_token.as_str())
                .ok_or(AUTH_ERROR)?,
        }
        .to_owned();
        let account_id = account_claim(&id_token)?;
        if previous.is_some_and(|old| old.account_id != account_id) {
            return Err(AUTH_ERROR);
        }
        let expires_at = match value.get("expires_in") {
            Some(value) => value
                .as_u64()
                .filter(|seconds| *seconds > 0 && *seconds <= 604800)
                .and_then(|seconds| now().checked_add(seconds)),
            None => claims(&access_token)
                .ok()
                .and_then(|claim| claim["exp"].as_u64()),
        }
        .filter(|expiry| *expiry > now())
        .ok_or(AUTH_ERROR)?;
        let result = Self {
            access_token,
            refresh_token,
            id_token,
            account_id,
            expires_at,
            refresh_failed: false,
        };
        result.validate()?;
        Ok(result)
    }
}

fn cstring(value: &std::ffi::OsStr) -> Result<CString, &'static str> {
    CString::new(value.as_bytes()).map_err(|_| STORAGE_ERROR)
}

fn open_at(directory: &File, name: &CString, flags: i32, mode: u32) -> Result<File, &'static str> {
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(STORAGE_ERROR);
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

pub(crate) struct Store {
    directory: File,
    name: CString,
    lock_name: CString,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self, &'static str> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(STORAGE_ERROR);
        }
        let parent = path.parent().ok_or(STORAGE_ERROR)?;
        let mut directory = File::open("/").map_err(|_| STORAGE_ERROR)?;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let name = cstring(name)?;
            directory = open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let metadata = directory.metadata().map_err(|_| STORAGE_ERROR)?;
            let uid = unsafe { libc::geteuid() };
            if (metadata.uid() != uid && metadata.uid() != 0)
                || (metadata.mode() & 0o022 != 0
                    && !(metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0))
            {
                return Err(STORAGE_ERROR);
            }
            let git = CString::new(".git").unwrap();
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    git.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                return Err(STORAGE_ERROR);
            }
        }
        let metadata = directory.metadata().map_err(|_| STORAGE_ERROR)?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(STORAGE_ERROR);
        }
        let filename = path.file_name().ok_or(STORAGE_ERROR)?;
        let name = cstring(filename)?;
        let mut lock = filename.to_os_string();
        lock.push(".lock");
        Ok(Self {
            directory,
            name,
            lock_name: cstring(&lock)?,
        })
    }

    fn private_file(file: &File) -> Result<(), &'static str> {
        let metadata = file.metadata().map_err(|_| STORAGE_ERROR)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(STORAGE_ERROR);
        }
        Ok(())
    }

    async fn lock(&self) -> Result<File, &'static str> {
        let file = open_at(
            &self.directory,
            &self.lock_name,
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )?;
        Self::private_file(&file)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(file);
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EWOULDBLOCK)
                || tokio::time::Instant::now() >= deadline
            {
                return Err(AUTH_ERROR);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(crate) fn read(&self) -> Result<Account, &'static str> {
        let file = open_at(&self.directory, &self.name, libc::O_RDONLY, 0)?;
        Self::private_file(&file)?;
        let mut bytes = Vec::new();
        file.take((MAX_AUTH + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| STORAGE_ERROR)?;
        if bytes.len() > MAX_AUTH {
            return Err(STORAGE_ERROR);
        }
        let account: Account = serde_json::from_slice(&bytes).map_err(|_| AUTH_ERROR)?;
        account.validate()?;
        Ok(account)
    }

    pub(crate) fn write(&self, account: &Account) -> Result<(), &'static str> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            let file = open_at(&self.directory, &self.name, libc::O_RDONLY, 0)?;
            Self::private_file(&file)?;
        } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
            return Err(STORAGE_ERROR);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| STORAGE_ERROR)?;
        let temporary = CString::new(format!(".codex-{}", URL_SAFE_NO_PAD.encode(random))).unwrap();
        let result = (|| {
            let mut file = open_at(
                &self.directory,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            let bytes = serde_json::to_vec(account).map_err(|_| STORAGE_ERROR)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| STORAGE_ERROR)?;
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    self.name.as_ptr(),
                )
            } != 0
            {
                return Err(STORAGE_ERROR);
            }
            self.directory.sync_all().map_err(|_| STORAGE_ERROR)
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    }
}

pub(crate) fn client() -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| AUTH_ERROR)
}

async fn bounded_json(response: reqwest::Response) -> Result<Value, &'static str> {
    if !response.status().is_success() {
        return Err(AUTH_ERROR);
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| AUTH_ERROR)?;
        if bytes.len() + chunk.len() > MAX_AUTH {
            return Err(AUTH_ERROR);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| AUTH_ERROR)
}

pub(crate) async fn access(config: &CodexConfig) -> Result<Account, &'static str> {
    config.validate().map_err(|_| AUTH_ERROR)?;
    let store = Store::open(Path::new(config.account_file.as_deref().ok_or(AUTH_ERROR)?))?;
    let _lock = store.lock().await?;
    let mut account = store.read()?;
    if account.expires_at > now().saturating_add(60) {
        return Ok(account);
    }
    account.refresh_failed = true;
    store.write(&account)?;
    let response = client()?.post(format!("{}/oauth/token", config.auth_base_url().trim_end_matches('/')))
        .json(&json!({"client_id":CLIENT_ID,"grant_type":"refresh_token","refresh_token":account.refresh_token,"scope":"openid profile email"}))
        .send().await.map_err(|_| AUTH_ERROR)?;
    let fresh = Account::from_token(&bounded_json(response).await?, Some(&account))?;
    store.write(&fresh)?;
    Ok(fresh)
}

pub(crate) async fn reject(config: &CodexConfig, rejected: &Account) -> Result<(), &'static str> {
    let store = Store::open(Path::new(config.account_file.as_deref().ok_or(AUTH_ERROR)?))?;
    let _lock = store.lock().await?;
    let mut current = store.read()?;
    if current.access_token == rejected.access_token {
        current.refresh_failed = true;
        store.write(&current)?;
    }
    Ok(())
}

pub(crate) async fn login(
    config: &CodexConfig,
    announce: impl Fn(&str, &str),
) -> Result<(), &'static str> {
    login_for(config, announce, Duration::from_secs(900)).await
}

pub(crate) async fn login_for(
    config: &CodexConfig,
    announce: impl Fn(&str, &str),
    timeout: Duration,
) -> Result<(), &'static str> {
    config.validate().map_err(|_| AUTH_ERROR)?;
    if !config.enabled {
        return Err("Codex is disabled");
    }
    let store = Store::open(Path::new(config.account_file.as_deref().ok_or(AUTH_ERROR)?))?;
    let _lock = store.lock().await?;
    let client = client()?;
    let base = config.auth_base_url().trim_end_matches('/');
    let code = bounded_json(
        client
            .post(format!("{base}/api/accounts/deviceauth/usercode"))
            .json(&json!({"client_id":CLIENT_ID}))
            .send()
            .await
            .map_err(|_| AUTH_ERROR)?,
    )
    .await?;
    let device = code["device_auth_id"]
        .as_str()
        .filter(|value| safe_secret(value))
        .ok_or(AUTH_ERROR)?;
    let user_code = code["user_code"]
        .as_str()
        .filter(|value| safe_secret(value))
        .ok_or(AUTH_ERROR)?;
    let interval = code["interval"]
        .as_u64()
        .or_else(|| {
            code["interval"]
                .as_str()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(5)
        .clamp(5, 60);
    announce(&format!("{base}/codex/device"), user_code);
    tokio::time::timeout(timeout, async {
        loop {
            let response = client
                .post(format!("{base}/api/accounts/deviceauth/token"))
                .json(&json!({"device_auth_id":device,"user_code":user_code}))
                .send()
                .await
                .map_err(|_| AUTH_ERROR)?;
            if matches!(response.status().as_u16(), 403 | 404) {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
            let authorization = bounded_json(response).await?;
            let code = authorization["authorization_code"]
                .as_str()
                .filter(|value| safe_secret(value))
                .ok_or(AUTH_ERROR)?;
            let verifier = authorization["code_verifier"]
                .as_str()
                .filter(|value| safe_secret(value))
                .ok_or(AUTH_ERROR)?;
            authorization["code_challenge"]
                .as_str()
                .filter(|value| safe_secret(value))
                .ok_or(AUTH_ERROR)?;
            let response = client
                .post(format!("{base}/oauth/token"))
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    (
                        "redirect_uri",
                        "https://auth.openai.com/deviceauth/callback",
                    ),
                    ("client_id", CLIENT_ID),
                    ("code_verifier", verifier),
                ])
                .send()
                .await
                .map_err(|_| AUTH_ERROR)?;
            let account = Account::from_token(&bounded_json(response).await?, None)?;
            store.write(&account)?;
            return Ok(());
        }
    })
    .await
    .map_err(|_| "Codex device sign-in expired")?
}

pub(crate) fn status(config: &CodexConfig) -> Value {
    let state = config
        .account_file
        .as_deref()
        .and_then(|path| Store::open(Path::new(path)).ok())
        .and_then(|store| store.read().ok());
    match state {
        Some(account) => {
            json!({"enabled":config.enabled,"state":if account.expires_at > now() {"ready"} else {"expired"},"expires_at":account.expires_at})
        }
        None => json!({"enabled":config.enabled,"state":"reauthentication_required"}),
    }
}

pub(crate) async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [command, path] = args else {
        return Err("usage: slotline codex login|status CONFIG.yaml".into());
    };
    if command != "login" && command != "status" {
        return Err("usage: slotline codex login|status CONFIG.yaml".into());
    }
    let contents = std::fs::read(path)?;
    let config: crate::config::Config = serde_yaml::from_slice(&contents)?;
    config.codex.validate()?;
    if command == "login" {
        login(&config.codex, |url, code| {
            eprintln!("Open {url} and enter {code}. This code is only for this sign-in.")
        })
        .await?;
    }
    println!("{}", status(&config.codex));
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn codex_auth_claims_rotation_missing_fields_and_expiry_are_fail_closed() {
        let token = format!(
            "e30.{}.synthetic",
            URL_SAFE_NO_PAD.encode(
                json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture"}}).to_string()
            )
        );
        let value = json!({"access_token":"synthetic-access","refresh_token":"synthetic-refresh","id_token":token,"expires_in":3600});
        let original = Account::from_token(&value, None).unwrap();
        assert_eq!(original.account_id, "fixture");
        let fresh = Account::from_token(&json!({"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}),Some(&original)).unwrap();
        assert_eq!(fresh.refresh_token, "rotated-refresh");
        assert_eq!(fresh.id_token, original.id_token);
        assert_eq!(
            Account::from_token(
                &json!({"access_token":"new-access","expires_in":3600}),
                Some(&original)
            )
            .unwrap()
            .refresh_token,
            "synthetic-refresh"
        );
        for field in ["access_token", "refresh_token", "id_token", "expires_in"] {
            let mut incomplete = value.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(Account::from_token(&incomplete, None).is_err());
        }
        for expiry in [json!(0), json!(-1), json!("invalid")] {
            let mut expired = value.clone();
            expired["expires_in"] = expiry;
            assert!(Account::from_token(&expired, None).is_err());
        }
        let mut invalid = original.clone();
        invalid.account_id = "other".into();
        assert!(invalid.validate().is_err());
        invalid = original.clone();
        invalid.refresh_failed = true;
        assert!(invalid.validate().is_err());
        assert!(account_claim("e30.e30.synthetic").is_err());
        assert!(account_claim("not-a-jwt").is_err());
        for field in ["refresh_token", "id_token"] {
            let mut malformed = value.clone();
            malformed[field] = Value::Null;
            assert!(Account::from_token(&malformed, Some(&original)).is_err());
        }
        assert!(Account::from_token(&json!({"access_token":"header\r\ninjection","refresh_token":"synthetic","id_token":token,"expires_in":3600}),None).is_err());
    }

    #[test]
    fn codex_auth_status_never_contains_account_material_or_storage_errors() {
        let config = CodexConfig {
            enabled: true,
            account_file: Some("/var/lib/slotline-nonexistent-fixture/account.json".into()),
            ..CodexConfig::default()
        };
        assert_eq!(
            status(&config),
            json!({"enabled":true,"state":"reauthentication_required"})
        );
    }
}
