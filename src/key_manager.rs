//! Local administrator key control. No handler on the inference listener calls
//! this module's database writers. The store contains Argon2id verifiers only.

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyPolicy {
    pub id: String,
    pub aliases: Vec<String>,
    pub class: String,
    pub requests_per_minute: u32,
    pub burst: u32,
    pub max_in_flight: usize,
    #[serde(default)]
    pub operator: bool,
    #[serde(default)]
    pub agent_affinity: bool,
    #[serde(default)]
    pub allowed_routes: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

pub struct SnapshotKey {
    pub verifier: String,
    pub policy: KeyPolicy,
    pub expires_at: Option<i64>,
}
pub type AuthSnapshot = HashMap<String, SnapshotKey>;

pub fn control_socket_path(store: &Path) -> PathBuf {
    let mut path = store.as_os_str().to_os_string();
    path.push(".control.sock");
    PathBuf::from(path)
}

fn notify_reload(store: &Path) -> Result<()> {
    let socket_path = control_socket_path(store);
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // A missing socket is safe only if no store-backed listener owns
            // the store. A running process holds this advisory lock.
            let file = fs::File::open(store)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(invalid("running listener control socket is missing").into());
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.write_all(b"reload\n")?;
    let mut reply = [0u8; 3];
    stream.read_exact(&mut reply)?;
    if reply != *b"ok\n" {
        return Err(invalid("running listener rejected key store reload").into());
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn now() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

fn check_private(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file()
        || meta.permissions().mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return Err(invalid("key store must be a private regular file (0600)").into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("key store needs a parent"))?;
    let parent_meta = fs::symlink_metadata(parent)?;
    if !parent_meta.file_type().is_dir() || parent_meta.permissions().mode() & 0o022 != 0 {
        return Err(invalid("key store parent must not be group/world writable").into());
    }
    Ok(())
}

fn open(path: &Path) -> Result<Connection> {
    check_private(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA temp_store=MEMORY;",
    )?;
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(invalid("key store integrity check failed").into());
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 1 {
        return Err(invalid("unsupported key store schema").into());
    }
    Ok(connection)
}

pub fn init(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("key store needs a parent"))?;
    let parent_meta = fs::symlink_metadata(parent)?;
    if !parent_meta.file_type().is_dir() || parent_meta.permissions().mode() & 0o022 != 0 {
        return Err(invalid("key store parent must not be group/world writable").into());
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA foreign_keys=ON;
         PRAGMA journal_mode=DELETE;
         PRAGMA user_version=1;
         CREATE TABLE credentials (
             id TEXT PRIMARY KEY,
             policy_json TEXT NOT NULL,
             enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
             created_at INTEGER NOT NULL,
             revoked_at INTEGER
         );
         CREATE TABLE key_versions (
             prefix TEXT PRIMARY KEY,
             credential_id TEXT NOT NULL REFERENCES credentials(id),
             verifier TEXT NOT NULL,
             issued_at INTEGER NOT NULL,
             expires_at INTEGER,
             revoked_at INTEGER
         );
         CREATE INDEX key_versions_credential ON key_versions(credential_id);
         CREATE TABLE audit (
             sequence INTEGER PRIMARY KEY AUTOINCREMENT,
             credential_id TEXT NOT NULL,
             actor TEXT NOT NULL,
             action TEXT NOT NULL,
             occurred_at INTEGER NOT NULL,
             result TEXT NOT NULL
         );",
    )?;
    check_private(path)
}

fn validate_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b":._-".contains(&byte)
        })
}

fn validate_policy(policy: &KeyPolicy) -> Result<()> {
    if !validate_label(&policy.id)
        || !validate_label(&policy.class)
        || policy.aliases.is_empty()
        || policy.aliases.len() > 64
        || policy.aliases.iter().any(|alias| !validate_label(alias))
        || policy.allowed_routes.len() > 64
        || policy
            .allowed_routes
            .iter()
            .any(|route| !validate_label(route))
        || policy.requests_per_minute == 0
        || policy.burst == 0
        || policy.max_in_flight == 0
        || policy.max_in_flight > 1024
        || policy.expires_at.is_some_and(|expiry| expiry <= 0)
    {
        return Err(invalid("invalid key policy").into());
    }
    Ok(())
}

fn validate_actor(actor: &str) -> Result<()> {
    if !validate_label(actor) {
        return Err(invalid("invalid administrator identity").into());
    }
    Ok(())
}

fn audit_actor() -> String {
    // The CLI is the administrator boundary. Record the kernel's effective
    // UID rather than a caller-supplied label or environment variable.
    format!("uid:{}", unsafe { libc::geteuid() })
}

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn create_key() -> Result<(String, String, String)> {
    let mut prefix_bytes = [0u8; 12];
    let mut secret_bytes = [0u8; 32];
    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut prefix_bytes).map_err(|_| invalid("random source unavailable"))?;
    getrandom::fill(&mut secret_bytes).map_err(|_| invalid("random source unavailable"))?;
    getrandom::fill(&mut salt_bytes).map_err(|_| invalid("random source unavailable"))?;
    let prefix = hex(&prefix_bytes);
    let secret = hex(&secret_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| invalid("salt encoding failed"))?;
    let verifier = Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|_| invalid("key hashing failed"))?
        .to_string();
    Ok((prefix.clone(), format!("slk_{prefix}_{secret}"), verifier))
}

pub fn issue(path: &Path, policy: &KeyPolicy, actor: &str) -> Result<String> {
    validate_actor(actor)?;
    validate_policy(policy)?;
    if policy
        .expires_at
        .is_some_and(|expiry| expiry <= now().unwrap_or(i64::MAX))
    {
        return Err(invalid("credential expiry is in the past").into());
    }
    let (prefix, raw, verifier) = create_key()?;
    let mut connection = open(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let timestamp = now()?;
    transaction.execute(
        "INSERT INTO credentials(id,policy_json,enabled,created_at) VALUES (?1,?2,1,?3)",
        params![policy.id, serde_json::to_string(policy)?, timestamp],
    )?;
    transaction.execute(
        "INSERT INTO key_versions(prefix,credential_id,verifier,issued_at) VALUES (?1,?2,?3,?4)",
        params![prefix, policy.id, verifier, timestamp],
    )?;
    transaction.execute(
        "INSERT INTO audit(credential_id,actor,action,occurred_at,result) VALUES (?1,?2,'issue',?3,'ok')",
        params![policy.id, actor, timestamp],
    )?;
    transaction.commit()?;
    Ok(raw)
}

pub fn rotate(path: &Path, id: &str, overlap_seconds: u64, actor: &str) -> Result<String> {
    validate_actor(actor)?;
    if !validate_label(id) || overlap_seconds > 3600 {
        return Err(invalid("invalid rotation arguments").into());
    }
    let (prefix, raw, verifier) = create_key()?;
    let mut connection = open(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let timestamp = now()?;
    let policy_json: Option<String> = transaction
        .query_row(
            "SELECT policy_json FROM credentials WHERE id=?1 AND enabled=1 AND revoked_at IS NULL",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let policy: KeyPolicy =
        serde_json::from_str(&policy_json.ok_or_else(|| invalid("credential unavailable"))?)?;
    if policy.expires_at.is_some_and(|expiry| expiry <= timestamp) {
        return Err(invalid("credential expired").into());
    }
    let overlap_end = timestamp + overlap_seconds as i64;
    transaction.execute(
        "UPDATE key_versions SET expires_at=MIN(COALESCE(expires_at,?2),?2)
         WHERE credential_id=?1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>?3)",
        params![id, overlap_end, timestamp],
    )?;
    transaction.execute(
        "INSERT INTO key_versions(prefix,credential_id,verifier,issued_at) VALUES (?1,?2,?3,?4)",
        params![prefix, id, verifier, timestamp],
    )?;
    transaction.execute(
        "INSERT INTO audit(credential_id,actor,action,occurred_at,result) VALUES (?1,?2,'rotate',?3,'ok')",
        params![id, actor, timestamp],
    )?;
    transaction.commit()?;
    Ok(raw)
}

pub fn revoke_key(path: &Path, prefix: &str, actor: &str) -> Result<()> {
    validate_actor(actor)?;
    if prefix.len() != 24 || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid key prefix").into());
    }
    let mut connection = open(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id: String = transaction
        .query_row(
            "SELECT credential_id FROM key_versions WHERE prefix=?1 AND revoked_at IS NULL",
            [prefix],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| invalid("key unavailable"))?;
    let timestamp = now()?;
    transaction.execute(
        "UPDATE key_versions SET revoked_at=?2 WHERE prefix=?1",
        params![prefix, timestamp],
    )?;
    transaction.execute(
        "INSERT INTO audit(credential_id,actor,action,occurred_at,result) VALUES (?1,?2,'revoke_key',?3,'ok')",
        params![id, actor, timestamp],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn revoke_credential(path: &Path, id: &str, actor: &str) -> Result<()> {
    validate_actor(actor)?;
    if !validate_label(id) {
        return Err(invalid("invalid credential ID").into());
    }
    let mut connection = open(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let timestamp = now()?;
    let count = transaction.execute(
        "UPDATE credentials SET enabled=0,revoked_at=?2 WHERE id=?1 AND enabled=1 AND revoked_at IS NULL",
        params![id, timestamp],
    )?;
    if count != 1 {
        return Err(invalid("credential unavailable").into());
    }
    transaction.execute(
        "UPDATE key_versions SET revoked_at=?2 WHERE credential_id=?1 AND revoked_at IS NULL",
        params![id, timestamp],
    )?;
    transaction.execute(
        "INSERT INTO audit(credential_id,actor,action,occurred_at,result) VALUES (?1,?2,'revoke_credential',?3,'ok')",
        params![id, actor, timestamp],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn snapshot(path: &Path) -> Result<AuthSnapshot> {
    let connection = open(path)?;
    let timestamp = now()?;
    let mut statement = connection.prepare(
        "SELECT k.prefix,k.verifier,c.policy_json,k.expires_at FROM key_versions k
         JOIN credentials c ON c.id=k.credential_id
         WHERE c.enabled=1 AND c.revoked_at IS NULL AND k.revoked_at IS NULL
           AND (k.expires_at IS NULL OR k.expires_at>?1)",
    )?;
    let rows = statement.query_map([timestamp], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut result = HashMap::new();
    for row in rows {
        let (prefix, verifier, policy_json, expires_at) = row?;
        if result.len() >= 4096
            || prefix.len() != 24
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
            || verifier.len() > 256
        {
            return Err(invalid("key store entry is outside bounds").into());
        }
        let policy: KeyPolicy = serde_json::from_str(&policy_json)?;
        validate_policy(&policy)?;
        if policy.expires_at.is_none_or(|expiry| expiry > timestamp) {
            let hash = PasswordHash::new(&verifier).map_err(|_| invalid("invalid key verifier"))?;
            let params = argon2::Params::try_from(&hash)
                .map_err(|_| invalid("invalid key verifier parameters"))?;
            if hash.algorithm.as_str() != "argon2id"
                || hash.version != Some(19)
                || hash.salt.is_none()
                || hash.hash.is_none()
                || params.m_cost() != argon2::Params::DEFAULT_M_COST
                || params.t_cost() != argon2::Params::DEFAULT_T_COST
                || params.p_cost() != argon2::Params::DEFAULT_P_COST
            {
                return Err(invalid("unsupported key verifier").into());
            }
            result.insert(
                prefix.clone(),
                SnapshotKey {
                    verifier,
                    policy,
                    expires_at,
                },
            );
        }
    }
    Ok(result)
}

pub fn verify<'a>(snapshot: &'a AuthSnapshot, raw: &str) -> Option<&'a KeyPolicy> {
    let mut parts = raw.split('_');
    if parts.next()? != "slk" {
        return None;
    }
    let prefix = parts.next()?;
    let secret = parts.next()?;
    if parts.next().is_some()
        || prefix.len() != 24
        || secret.len() != 64
        || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !secret.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let entry = snapshot.get(prefix)?;
    let current = now().ok()?;
    if entry.expires_at.is_some_and(|expiry| expiry <= current)
        || entry
            .policy
            .expires_at
            .is_some_and(|expiry| expiry <= current)
    {
        return None;
    }
    let hash = PasswordHash::new(&entry.verifier).ok()?;
    Argon2::default()
        .verify_password(secret.as_bytes(), &hash)
        .ok()
        .map(|()| &entry.policy)
}

pub fn list(path: &Path) -> Result<serde_json::Value> {
    let connection = open(path)?;
    let mut statement = connection.prepare(
        "SELECT c.id,c.policy_json,c.enabled,c.created_at,c.revoked_at,k.prefix,k.issued_at,k.expires_at,k.revoked_at
         FROM credentials c LEFT JOIN key_versions k ON k.credential_id=c.id
         ORDER BY c.id,k.issued_at",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    })?;
    let mut output = Vec::new();
    for row in rows {
        let (
            id,
            policy_json,
            enabled,
            created_at,
            revoked_at,
            prefix,
            issued_at,
            key_expires_at,
            key_revoked_at,
        ) = row?;
        let policy: KeyPolicy = serde_json::from_str(&policy_json)?;
        validate_policy(&policy)?;
        output.push(serde_json::json!({
            "id":id,
            "policy":policy,
            "enabled":enabled == 1,
            "created_at":created_at,
            "revoked_at":revoked_at,
            "prefix":prefix,
            "issued_at":issued_at,
            "key_expires_at":key_expires_at,
            "key_revoked_at":key_revoked_at,
        }));
    }
    Ok(serde_json::Value::Array(output))
}

fn copy_store(source: &Path, destination: &Path) -> Result<()> {
    let source_connection = open(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("destination needs a parent"))?;
    let parent_meta = fs::symlink_metadata(parent)?;
    if !parent_meta.file_type().is_dir() || parent_meta.permissions().mode() & 0o022 != 0 {
        return Err(invalid("destination parent must not be group/world writable").into());
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    let copied = (|| -> Result<()> {
        let mut destination_connection = Connection::open(destination)?;
        let backup = Backup::new(&source_connection, &mut destination_connection)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
        drop(backup);
        drop(destination_connection);
        let restored = open(destination)?;
        let source_count: i64 =
            source_connection
                .query_row("SELECT COUNT(*) FROM key_versions", [], |row| row.get(0))?;
        let dest_count: i64 =
            restored.query_row("SELECT COUNT(*) FROM key_versions", [], |row| row.get(0))?;
        if source_count != dest_count {
            return Err(invalid("key store copy validation failed").into());
        }
        Ok(())
    })();
    if copied.is_err() {
        let _ = fs::remove_file(destination);
    }
    copied
}

pub fn backup(path: &Path, destination: &Path) -> Result<()> {
    copy_store(path, destination)
}

pub fn restore(backup_path: &Path, destination: &Path) -> Result<()> {
    // Restore only into a new path. Replacing a live store requires an
    // administrator to stop the service and deliberately swap paths.
    copy_store(backup_path, destination)
}

pub fn run(args: &[String]) -> Result<()> {
    match args {
        [command, path] if command == "init" => init(Path::new(path)),
        [command, path, policy_path] if command == "issue" => {
            let policy: KeyPolicy = serde_json::from_slice(&fs::read(policy_path)?)?;
            let raw = issue(Path::new(path), &policy, &audit_actor())?;
            println!("{raw}");
            notify_reload(Path::new(path))
        }
        [command, path, id, overlap] if command == "rotate" => {
            let seconds: u64 = overlap.parse()?;
            let raw = rotate(Path::new(path), id, seconds, &audit_actor())?;
            println!("{raw}");
            notify_reload(Path::new(path))
        }
        [command, path, prefix] if command == "revoke-key" => {
            revoke_key(Path::new(path), prefix, &audit_actor())?;
            notify_reload(Path::new(path))
        }
        [command, path, id] if command == "revoke-credential" => {
            revoke_credential(Path::new(path), id, &audit_actor())?;
            notify_reload(Path::new(path))
        }
        [command, path] if command == "list" => {
            println!("{}", list(Path::new(path))?);
            Ok(())
        }
        [command, path] if command == "verify" => {
            let mut raw = String::new();
            io::stdin().take(256).read_to_string(&mut raw)?;
            let keys = snapshot(Path::new(path))?;
            let id = verify(&keys, raw.trim_end_matches(['\r', '\n']))
                .map(|policy| policy.id.as_str());
            println!("{}", serde_json::json!({"valid":id.is_some(),"id":id}));
            Ok(())
        }
        [command, path, destination] if command == "backup" => {
            backup(Path::new(path), Path::new(destination))
        }
        [command, backup_path, destination] if command == "restore" => {
            restore(Path::new(backup_path), Path::new(destination))
        }
        _ => Err(invalid("usage: slotline keys {init STORE | issue STORE POLICY.json | rotate STORE ID OVERLAP_SECONDS | revoke-key STORE PREFIX | revoke-credential STORE ID | list STORE | verify STORE < client.key | backup STORE NEW_FILE | restore BACKUP NEW_STORE}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> KeyPolicy {
        KeyPolicy {
            id: "example-client".into(),
            aliases: vec!["qwen38flash".into()],
            class: "interactive".into(),
            requests_per_minute: 120,
            burst: 12,
            max_in_flight: 8,
            operator: false,
            agent_affinity: true,
            allowed_routes: vec![],
            expires_at: None,
        }
    }

    #[test]
    fn issue_rotate_revoke_and_store_only_verifiers() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("keys.sqlite3");
        init(&path).unwrap();
        let raw = issue(&path, &policy(), "operator").unwrap();
        let first = snapshot(&path).unwrap();
        assert_eq!(verify(&first, &raw).unwrap().id, policy().id);
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(raw.len())
                .any(|bytes| bytes == raw.as_bytes())
        );
        assert!(!list(&path).unwrap().to_string().contains(&raw));
        let rotated = rotate(&path, &policy().id, 30, "operator").unwrap();
        let second = snapshot(&path).unwrap();
        assert!(verify(&second, &raw).is_some());
        assert!(verify(&second, &rotated).is_some());
        let backup_path = directory.path().join("backup.sqlite3");
        let restored_path = directory.path().join("restored.sqlite3");
        backup(&path, &backup_path).unwrap();
        restore(&backup_path, &restored_path).unwrap();
        let restored = snapshot(&restored_path).unwrap();
        assert!(verify(&restored, &raw).is_some());
        assert!(verify(&restored, &rotated).is_some());
        assert!(
            !fs::read(&backup_path)
                .unwrap()
                .windows(raw.len())
                .any(|bytes| bytes == raw.as_bytes())
        );
        let prefix = raw.split('_').nth(1).unwrap();
        revoke_key(&path, prefix, "operator").unwrap();
        let third = snapshot(&path).unwrap();
        assert!(verify(&third, &raw).is_none());
        assert!(verify(&third, &rotated).is_some());
        revoke_credential(&path, &policy().id, "operator").unwrap();
        assert!(snapshot(&path).unwrap().is_empty());
    }

    #[test]
    fn expiry_and_store_failure_deny_keys() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("keys.sqlite3");
        init(&path).unwrap();
        let mut expired = policy();
        expired.expires_at = Some(now().unwrap() - 1);
        assert!(issue(&path, &expired, "operator").is_err());
        let raw = issue(&path, &policy(), "operator").unwrap();
        assert!(verify(&snapshot(&path).unwrap(), &raw).is_some());
        let replacement = rotate(&path, &policy().id, 0, "operator").unwrap();
        let after = snapshot(&path).unwrap();
        assert!(verify(&after, &raw).is_none());
        assert!(verify(&after, &replacement).is_some());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(snapshot(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"not a SQLite key store").unwrap();
        assert!(snapshot(&path).is_err());
    }
}
