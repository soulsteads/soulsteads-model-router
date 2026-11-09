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
