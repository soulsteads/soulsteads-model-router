use crate::{auth_control::policy_control_path, config};
use serde_json::Value;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

fn object<'a>(value: &'a Value, key: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("invalid status response: missing {key}"))
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("invalid status response: missing {key}"))
}
fn number(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("invalid status response: missing {key}"))
}
fn percentage(value: &Value, key: &str) -> Result<f64, String> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && (0.0..=100.0).contains(x))
        .ok_or_else(|| format!("invalid status response: missing {key}"))
}

fn selector_names(value: &Value) -> Result<String, String> {
    let names = value
        .as_array()
        .ok_or("invalid status response: invalid selectors")?
        .iter()
        .map(|name| {
            name.as_str()
                .ok_or("invalid status response: invalid selector")
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    })
}

pub(crate) fn render(value: &Value) -> Result<String, String> {
    let routes = object(value, "routes")?;
    let credentials = object(value, "credentials")?;
    let leases = value
        .get("leases")
        .and_then(Value::as_array)
        .ok_or("invalid status response: missing leases")?;
    let mut out = String::new();
    writeln!(out, "Slotline — live status").unwrap();
    writeln!(
        out,
        "Requests: {}  Rejected: {}  Upstream errors: {}",
        number(value, "requests_total")?,
        number(value, "rejects_total")?,
        number(value, "upstream_errors_total")?
    )
    .unwrap();
    if let Some(semantic) = value.get("semantic_routing") {
        let enabled = semantic["enabled"]
            .as_bool()
            .ok_or("invalid status response: missing semantic enabled")?;
        let admission = &semantic["decision_admission"];
        let closed = admission["closed"]
            .as_bool()
            .ok_or("invalid status response: missing decision admission state")?;
        writeln!(
            out,
            "\nSemantic routing: {}",
            if enabled { "enabled" } else { "disabled" }
        )
        .unwrap();
        writeln!(
            out,
            "  Configured selectors: {}",
            selector_names(&semantic["selectors"])?
        )
        .unwrap();
        writeln!(
            out,
            "  Decision admission: {}/{} in flight, {} available, {}",
            number(admission, "in_flight")?,
            number(admission, "limit")?,
            number(admission, "available")?,
            if closed { "closed" } else { "open" }
        )
        .unwrap();
        if let Some(profiles) = semantic.get("profiles").and_then(Value::as_object) {
            for (name, profile) in profiles {
                writeln!(
                    out,
                    "  Profile {name}: mode={}, version={}, preflight={}",
                    text(profile, "mode")?,
                    profile["policy_version"].as_str().unwrap_or("(none)"),
                    profile["compatibility_preflight"]
                        .as_bool()
                        .unwrap_or(false)
                )
                .unwrap();
            }
        }
        if let Some(digest) = semantic.get("policy_sha256").and_then(Value::as_str) {
            writeln!(out, "  Semantic policy SHA256: {digest}").unwrap();
        }
    }
    writeln!(
        out,
        "\nEndpoints (traffic = upstream attempts; 5m share / process-lifetime share):"
    )
    .unwrap();
    let mut names: Vec<_> = routes.keys().collect();
    names.sort();
    for name in names {
        let r = &routes[name];
        let state = text(r, "state")?;
        let health = if !r
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or("invalid status response: missing enabled")?
        {
            "DISABLED"
        } else if state == "serving" {
            "UP"
        } else {
            "NOT SERVING"
        };
        let checked = r
            .get("last_checked_age_seconds")
            .and_then(Value::as_u64)
            .map_or("never".to_string(), |x| format!("{x}s ago"));
        writeln!(out, "  {name}  {health} ({state})  {}  model {}  {}/{} active  checked {checked}  traffic {:.1}% / {:.1}% ({} / {} attempts)",
            text(r,"base_url")?, text(r,"model")?, number(r,"active")?, number(r,"limit")?,
            percentage(r,"traffic_percent_5m")?, percentage(r,"traffic_percent_total")?, number(r,"attempts_5m")?, number(r,"attempts_total")?).unwrap();
        if let Some(probe) = r.get("last_probe").filter(|v| !v.is_null()) {
            // Probe health remains independent of display-only server statistics.
            let status = probe
                .get("http_status")
                .and_then(Value::as_u64)
                .map_or("-".to_string(), |code| code.to_string());
            writeln!(
                out,
                "    latest probe: {} {}  HTTP {status}  {}ms  {}s ago",
                text(probe, "stage")?,
                text(probe, "outcome")?,
                number(probe, "latency_ms")?,
                number(probe, "age_seconds")?
            )
            .unwrap();
        }
        if let Some(m) = r.get("server_metrics") {
            let rate = |field: &str| {
                m[field]
                    .as_f64()
                    .map_or("unavailable".into(), |x| format!("{x:.2} tok/s"))
            };
            let acceptance = m["acceptance_ratio"]
                .as_f64()
                .map_or("unavailable".into(), |x| format!("{:.1}%", x * 100.0));
            writeln!(out, "    server metrics: {} ({})  aggregate generation {}  prompt {}  acceptance {}  sample age {}s  window {}s",
                m["status"].as_str().unwrap_or("unavailable"),
                m["source"].as_str().or_else(|| m["reason"].as_str()).unwrap_or("unknown"),
                rate("generation_tokens_per_second"), rate("prompt_tokens_per_second"), acceptance,
                m["age_seconds"].as_u64().map_or("-".into(), |x| x.to_string()),
                m["window_seconds"].as_f64().map_or("-".into(), |x| format!("{x:.1}"))).unwrap();
            if m["reported_generation_tokens_per_second"].is_number()
                || m["reported_prompt_tokens_per_second"].is_number()
            {
                writeln!(out, "    server-reported generation {}  prompt {} (server-defined throughput; not per-request speed)",
                    rate("reported_generation_tokens_per_second"), rate("reported_prompt_tokens_per_second")).unwrap();
            }
        }
        if let Some(probe) = r.get("last_failed_probe").filter(|v| !v.is_null()) {
            let status = probe
                .get("http_status")
                .and_then(Value::as_u64)
                .map_or("-".to_string(), |code| code.to_string());
            writeln!(
                out,
                "    last failure: {} {}  HTTP {status}  {}ms  {}s ago",
                text(probe, "stage")?,
                text(probe, "outcome")?,
                number(probe, "latency_ms")?,
                number(probe, "age_seconds")?
            )
            .unwrap();
        }
    }
    writeln!(out, "\nAPI-key routes (active credential policies; eligible configured routes, not live placement):").unwrap();
    let mut ids: Vec<_> = credentials.keys().collect();
    ids.sort();
    for id in ids {
        let c = &credentials[id];
        let affinity = c
            .get("agent_affinity")
            .and_then(Value::as_bool)
            .ok_or("invalid status response: missing agent_affinity")?;
        writeln!(
            out,
            "  {id}  class={}  affinity={affinity}",
            text(c, "class")?
        )
        .unwrap();
        if let Some(selectors) = c.get("semantic_selectors") {
            writeln!(out, "    Selector grants: {}", selector_names(selectors)?).unwrap();
        }
        let aliases = object(c, "aliases")?;
        let mut alias_names: Vec<_> = aliases.keys().collect();
        alias_names.sort();
        for alias in alias_names {
            let entries = aliases[alias]
                .as_array()
                .ok_or("invalid status response: invalid alias routes")?;
            let mut items = Vec::new();
            for entry in entries {
                let fallback = entry
                    .get("fallback_only")
                    .and_then(Value::as_bool)
                    .ok_or("invalid status response: invalid fallback")?;
                items.push(format!(
                    "{}{}",
                    text(entry, "route")?,
                    if fallback { " [fallback]" } else { "" }
                ));
            }
            writeln!(
                out,
                "    {alias} -> {}",
                if items.is_empty() {
                    "(no enabled routes)".to_string()
                } else {
                    items.join(", ")
                }
            )
            .unwrap();
        }
    }
    writeln!(
        out,
        "\nAdvisory leases (TTL {}s; age / idle / remaining; no model unloading):",
        number(value, "lease_ttl_seconds")?
    )
    .unwrap();
    if leases.is_empty() {
        writeln!(out, "  (none)").unwrap();
    }
    for lease in leases {
        writeln!(
            out,
            "  {}  {}s / {}s / {}s",
            text(lease, "route")?,
            number(lease, "age_seconds")?,
            number(lease, "idle_seconds")?,
            number(lease, "remaining_seconds")?
        )
        .unwrap();
    }
    Ok(out)
}

pub(crate) fn default_policy_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SLOTLINE_CONFIG") {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("slotline/config.yaml");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/slotline/config.yaml")
}

pub(crate) async fn run(
    policy_path: &Path,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = config::load(policy_path)?;
    let socket = policy_control_path(
        policy_path,
        policy.config.key_store.as_deref().map(Path::new),
    );
    let value = tokio::time::timeout(Duration::from_secs(8), async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(b"status\n").await?;
        let mut bytes = Vec::new();
        stream
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("status response exceeds size limit".into());
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        Ok::<Value, Box<dyn std::error::Error>>(value)
    })
    .await??;
    let formatted = render(&value)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{formatted}");
    }
    Ok(())
}
