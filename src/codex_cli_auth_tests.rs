use crate::{
    codex_auth::{self, Account},
    config::{CodexAuthSource, CodexConfig},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    auth_dir: PathBuf,
    auth_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "slotline-codex-cli-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let auth_dir = root.join(".codex");
        fs::create_dir(&auth_dir).unwrap();
        fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let auth_path = auth_dir.join("auth.json");
        Self {
            root,
            auth_dir,
            auth_path,
        }
    }

    fn write_bytes(&self, path: &std::path::Path, bytes: &[u8], mode: u32) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write_auth(&self, token: &str, account: Option<&str>, mode: &str) {
        self.write_bytes(&self.auth_path, &auth_document(token, account, mode), 0o600);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn token(exp: u64, account: Option<&str>) -> String {
    let mut claims = json!({"exp": exp});
    if let Some(account) = account {
        claims["https://api.openai.com/auth"] = json!({"chatgpt_account_id": account});
    }
    format!(
        "e30.{}.synthetic",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}

fn auth_document(token: &str, account: Option<&str>, mode: &str) -> Vec<u8> {
    let mut tokens = json!({
        "id_token": "synthetic-cli-id-token-not-used",
        "access_token": token,
        "refresh_token": "synthetic-cli-refresh-token-never-used"
    });
    if let Some(account) = account {
        tokens["account_id"] = json!(account);
    }
    serde_json::to_vec(&json!({
        "auth_mode": mode,
        "OPENAI_API_KEY": null,
        "tokens": tokens,
        "last_refresh": "2026-01-01T00:00:00Z"
    }))
    .unwrap()
}

fn cli_config(path: &std::path::Path) -> CodexConfig {
    CodexConfig {
        enabled: true,
        alias: Some("fixture-codex".into()),
        model: Some("fixture-model".into()),
        auth_source: CodexAuthSource::CodexCli,
        cli_auth_file: Some(path.to_string_lossy().into_owned()),
        ..CodexConfig::default()
    }
}

#[test]
fn config_accepts_read_only_codex_cli_auth_source() {
    let parsed = serde_yaml::from_str::<CodexConfig>(
        "auth_source: codex_cli\ncli_auth_file: /fixture/codex/auth.json\n",
    );
    assert!(parsed.is_ok(), "Codex CLI auth config must be accepted");
}

#[test]
fn auth_source_precedence_and_path_resolution_are_deterministic() {
    let auto = CodexConfig::default();
    assert_eq!(
        auto.resolved_auth_source().unwrap(),
        CodexAuthSource::CodexCli
    );
    assert_eq!(
        codex_auth::resolve_cli_auth_path(
            &auto,
            Some(OsStr::new("/fixture/codex-home")),
            Some(OsStr::new("/fixture/home")),
        )
        .unwrap(),
        PathBuf::from("/fixture/codex-home/auth.json")
    );
    assert_eq!(
        codex_auth::resolve_cli_auth_path(&auto, None, Some(OsStr::new("/fixture/home"))).unwrap(),
        PathBuf::from("/fixture/home/.codex/auth.json")
    );

    let mut legacy = auto.clone();
    legacy.account_file = Some("/fixture/managed/account.json".into());
    assert_eq!(
        legacy.resolved_auth_source().unwrap(),
        CodexAuthSource::Managed
    );

    let mut override_config = auto.clone();
    override_config.cli_auth_file = Some("/fixture/override/auth.json".into());
    assert_eq!(
        override_config.resolved_auth_source().unwrap(),
        CodexAuthSource::CodexCli
    );
    assert_eq!(
        codex_auth::resolve_cli_auth_path(
            &override_config,
            Some(OsStr::new("/ignored/codex-home")),
            Some(OsStr::new("/ignored/home")),
        )
        .unwrap(),
        PathBuf::from("/fixture/override/auth.json")
    );

    legacy.cli_auth_file = Some("/fixture/ambiguous/auth.json".into());
    assert!(legacy.resolved_auth_source().is_err());
    let mut explicit_cli = auto.clone();
    explicit_cli.auth_source = CodexAuthSource::CodexCli;
    explicit_cli.account_file = Some("/fixture/managed/account.json".into());
    assert!(explicit_cli.resolved_auth_source().is_err());
    let mut explicit_managed = auto;
    explicit_managed.auth_source = CodexAuthSource::Managed;
    assert!(explicit_managed.resolved_auth_source().is_err());
}

#[test]
fn reads_chatgpt_auth_from_private_file_under_normal_0755_codex_directory() {
    let fixture = Fixture::new();
    assert_eq!(
        fs::metadata(&fixture.auth_dir).unwrap().mode() & 0o777,
        0o755
    );
    let access = token(codex_auth::now() + 3600, Some("fixture-account"));
    fixture.write_auth(&access, Some("fixture-account"), "chatgpt");

    let account = codex_auth::read_cli_auth_file(&fixture.auth_path).unwrap();
    assert!(account.cli_auth);
    assert_eq!(account.account_id, "fixture-account");
    assert_eq!(account.access_token, access);
    assert!(account.refresh_token.is_empty());
    assert!(account.id_token.is_empty());
}

#[test]
fn derives_account_claim_only_when_file_id_is_missing_and_rejects_mismatches() {
    let fixture = Fixture::new();
    let access = token(codex_auth::now() + 3600, Some("claim-account"));
    fixture.write_auth(&access, None, "chatgpt");
    assert_eq!(
        codex_auth::read_cli_auth_file(&fixture.auth_path)
            .unwrap()
            .account_id,
        "claim-account"
    );

    fixture.write_auth(&access, Some("different-account"), "chatgpt");
    assert!(codex_auth::read_cli_auth_file(&fixture.auth_path).is_err());
}

#[test]
fn rejects_expired_malformed_oversized_and_non_chatgpt_inputs_without_leaking_content() {
    let fixture = Fixture::new();
    let secret_marker = "synthetic-token-marker";
    fixture.write_auth(
        &token(codex_auth::now().saturating_sub(1), Some("fixture-account")),
        Some("fixture-account"),
        "chatgpt",
    );
    assert_eq!(
        codex_auth::read_cli_auth_file(&fixture.auth_path)
            .err()
            .unwrap(),
        "Codex CLI access token expired; run codex login locally"
    );

    fixture.write_bytes(&fixture.auth_path, b"{malformed synthetic input", 0o600);
    let error = codex_auth::read_cli_auth_file(&fixture.auth_path)
        .err()
        .unwrap();
    assert!(!error.contains("malformed"));
    assert!(!error.contains(fixture.auth_path.to_string_lossy().as_ref()));

    fixture.write_bytes(&fixture.auth_path, &vec![b' '; 128 * 1024 + 1], 0o600);
    assert!(codex_auth::read_cli_auth_file(&fixture.auth_path).is_err());

    let api_key = auth_document(secret_marker, Some("fixture-account"), "apikey");
    fixture.write_bytes(&fixture.auth_path, &api_key, 0o600);
    let error = codex_auth::read_cli_auth_file(&fixture.auth_path)
        .err()
        .unwrap();
    assert!(!error.contains(secret_marker));
}

#[test]
fn rejects_symlinks_and_insecure_file_or_directory_permissions() {
    let fixture = Fixture::new();
    let access = token(codex_auth::now() + 3600, Some("fixture-account"));
    fixture.write_auth(&access, Some("fixture-account"), "chatgpt");

    let symlink_path = fixture.root.join("auth-link.json");
    symlink(&fixture.auth_path, &symlink_path).unwrap();
    assert!(codex_auth::read_cli_auth_file(&symlink_path).is_err());

    fs::set_permissions(&fixture.auth_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(codex_auth::read_cli_auth_file(&fixture.auth_path).is_err());

    let unsafe_dir = fixture.root.join("group-writable");
    fs::create_dir(&unsafe_dir).unwrap();
    fs::set_permissions(&unsafe_dir, fs::Permissions::from_mode(0o775)).unwrap();
    let unsafe_file = unsafe_dir.join("auth.json");
    fixture.write_bytes(
        &unsafe_file,
        &auth_document(&access, Some("fixture-account"), "chatgpt"),
        0o600,
    );
    assert!(codex_auth::read_cli_auth_file(&unsafe_file).is_err());
}

#[test]
fn rereads_atomic_cli_file_replacement_and_status_contains_no_identity_or_path() {
    let fixture = Fixture::new();
    let first = token(codex_auth::now() + 3600, Some("first-account"));
    fixture.write_auth(&first, Some("first-account"), "chatgpt");
    assert_eq!(
        codex_auth::read_cli_auth_file(&fixture.auth_path)
            .unwrap()
            .access_token,
        first
    );

    let second = token(codex_auth::now() + 7200, Some("second-account"));
    let replacement = fixture.auth_dir.join("auth.next");
    fixture.write_bytes(
        &replacement,
        &auth_document(&second, Some("second-account"), "chatgpt"),
        0o600,
    );
    fs::rename(&replacement, &fixture.auth_path).unwrap();
    let current = codex_auth::read_cli_auth_file(&fixture.auth_path).unwrap();
    assert_eq!(current.access_token, second);
    assert_eq!(current.account_id, "second-account");

    let status = codex_auth::status(&cli_config(&fixture.auth_path)).to_string();
    assert!(status.contains("codex_cli"));
    assert!(status.contains("ready"));
    for private in [
        fixture.auth_path.to_string_lossy().as_ref(),
        "second-account",
        second.as_str(),
    ] {
        assert!(!status.contains(private));
    }
}

#[test]
fn disabled_default_status_does_not_read_home_codex_auth() {
    let status = codex_auth::status(&CodexConfig::default()).to_string();
    assert!(status.contains("disabled"));
    assert!(status.contains("codex_cli"));
}

#[tokio::test]
async fn legacy_managed_selection_does_not_fall_back_to_available_cli_file() {
    let fixture = Fixture::new();
    let access = token(codex_auth::now() + 3600, Some("fixture-account"));
    fixture.write_auth(&access, Some("fixture-account"), "chatgpt");
    let config = CodexConfig {
        enabled: true,
        alias: Some("fixture-codex".into()),
        model: Some("fixture-model".into()),
        account_file: Some(
            fixture
                .auth_dir
                .join("missing-managed-account.json")
                .to_string_lossy()
                .into_owned(),
        ),
        ..CodexConfig::default()
    };
    assert_eq!(
        config.resolved_auth_source().unwrap(),
        CodexAuthSource::Managed
    );
    assert!(codex_auth::access(&config).await.is_err());
}

#[tokio::test]
async fn cli_rejection_and_native_login_do_not_write_or_refresh_the_cli_store() {
    let fixture = Fixture::new();
    let access = token(codex_auth::now() + 3600, Some("fixture-account"));
    fixture.write_auth(&access, Some("fixture-account"), "chatgpt");
    let account: Account = codex_auth::read_cli_auth_file(&fixture.auth_path).unwrap();
    let before = fs::read(&fixture.auth_path).unwrap();
    let entry_count = fs::read_dir(&fixture.auth_dir).unwrap().count();
    let config = cli_config(&fixture.auth_path);

    codex_auth::reject(&config, &account).await.unwrap();
    assert!(
        codex_auth::login_for(&config, |_, _| {}, Duration::from_millis(1))
            .await
            .is_err()
    );
    assert_eq!(fs::read(&fixture.auth_path).unwrap(), before);
    assert_eq!(
        fs::read_dir(&fixture.auth_dir).unwrap().count(),
        entry_count
    );
}

#[tokio::test]
async fn expired_cli_token_never_calls_oauth_refresh_endpoint() {
    let fixture = Fixture::new();
    fixture.write_auth(
        &token(codex_auth::now().saturating_sub(1), Some("fixture-account")),
        Some("fixture-account"),
        "chatgpt",
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut config = cli_config(&fixture.auth_path);
    config.auth_base_url = Some(url);

    let result = codex_auth::access(&config).await;
    assert_eq!(
        result.err(),
        Some("Codex CLI access token expired; run codex login locally")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}
