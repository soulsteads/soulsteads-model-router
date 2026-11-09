use crate::config::{CodexConfig, Credential};

fn credential(cloud_allowed: Option<bool>) -> Credential {
    let mut value = serde_json::json!({
        "name": "fixture-workload",
        "aliases": ["local-model", "codex-model"],
        "class": "test"
    });
    if let Some(cloud_allowed) = cloud_allowed {
        value["cloud_allowed"] = serde_json::Value::Bool(cloud_allowed);
    }
    serde_json::from_value(value).expect("fixture credential parses")
}

#[test]
fn codex_cloud_requires_explicit_backend_and_workload_opt_in() {
    let backend_disabled: CodexConfig =
        serde_json::from_str("{}").expect("default Codex config parses");
    let backend_enabled = CodexConfig {
        enabled: true,
        ..CodexConfig::default()
    };
    let legacy_workload = credential(None);
    let denied_workload = credential(Some(false));
    let opted_in_workload = credential(Some(true));

    assert!(!backend_disabled.enabled);
    assert!(!legacy_workload.cloud_allowed);
    assert!(!legacy_workload.can_use_codex(&backend_enabled));
    assert!(!denied_workload.can_use_codex(&backend_enabled));
    assert!(!opted_in_workload.can_use_codex(&backend_disabled));
    assert!(opted_in_workload.can_use_codex(&backend_enabled));
}

#[test]
fn codex_permission_defaults_to_deny_in_workload_key_policy() {
    let base = serde_json::json!({
        "id": "fixture-key",
        "aliases": ["local-model", "codex-model"],
        "class": "test",
        "requests_per_minute": 30,
        "burst": 5,
        "max_in_flight": 2
    });
    let legacy: crate::key_manager::KeyPolicy = serde_json::from_value(base.clone()).unwrap();
    assert!(!legacy.cloud_allowed);

    let mut opted_in = base;
    opted_in["cloud_allowed"] = serde_json::json!(true);
    let opted_in: crate::key_manager::KeyPolicy = serde_json::from_value(opted_in).unwrap();
    assert!(opted_in.cloud_allowed);
}

#[test]
fn codex_backend_requires_explicit_alias_model_and_private_state_path() {
    let incomplete = CodexConfig {
        enabled: true,
        ..CodexConfig::default()
    };
    assert!(incomplete.validate().is_err());
}

#[test]
fn codex_endpoint_policy_allows_loopback_fixtures_but_not_remote_http() {
    let fixture = CodexConfig {
        enabled: true,
        alias: Some("codex-test".into()),
        model: Some("gpt-test".into()),
        account_file: Some("/var/lib/slotline-codex-fixture/account.json".into()),
        auth_base_url: Some("http://127.0.0.1:40123".into()),
        responses_url: Some("http://[::1]:40123/backend-api/codex/responses".into()),
    };
    assert!(fixture.validate().is_ok(), "{:?}", fixture.validate());

    let mut unsafe_remote = fixture;
    unsafe_remote.auth_base_url = Some("http://auth.openai.com".into());
    assert!(unsafe_remote.validate().is_err());
}

#[test]
fn codex_explicit_provider_alias_loads_without_a_local_route() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("workload.key");
    std::fs::write(&key, "synthetic-codex-workload-0123456789").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let path = dir.path().join("policy.json");
    let value = serde_json::json!({
        "bind":"100.127.255.254", "port":14022,
        "codex":{"enabled":true,"alias":"cloud-test","model":"gpt-fixture","account_file":"/var/lib/slotline-codex-fixture/account.json"},
        "credentials":[{"name":"fixture","key_file":key,"class":"test","aliases":["cloud-test"],"allowed_routes":["codex"],"cloud_allowed":true}],
        "routes":[], "aliases":{}
    });
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(crate::config::load(&path).is_ok());
}

#[test]
fn codex_endpoint_credentials_queries_ports_and_nonproduction_tls_origins_are_rejected() {
    for endpoint in [
        "http://auth.openai.com",
        "http://localhost:12345",
        "https://auth.openai.com.evil.invalid",
        "https://auth.openai.com:8443",
        "https://synthetic:secret@auth.openai.com",
        "https://auth.openai.com?api_key=synthetic",
        "https://auth.openai.com#synthetic",
    ] {
        let config = CodexConfig {
            auth_base_url: Some(endpoint.into()),
            ..CodexConfig::default()
        };
        assert!(config.validate().is_err(), "{endpoint}");
    }
    for endpoint in [
        "http://chatgpt.com/backend-api/codex/responses",
        "https://chatgpt.com:8443/backend-api/codex/responses",
        "https://chatgpt.com/backend-api/codex/responses?token=synthetic",
        "https://example.invalid/backend-api/codex/responses",
    ] {
        let config = CodexConfig {
            responses_url: Some(endpoint.into()),
            ..CodexConfig::default()
        };
        assert!(config.validate().is_err(), "{endpoint}");
    }
}
