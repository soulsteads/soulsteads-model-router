#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub prompt_tokens: Option<f64>,
    pub generated_tokens: Option<f64>,
    pub draft_tokens: Option<f64>,
    pub accepted_tokens: Option<f64>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Delta {
    pub prompt_tokens: Option<f64>,
    pub generated_tokens: Option<f64>,
    pub draft_tokens: Option<f64>,
    pub accepted_tokens: Option<f64>,
}
pub(crate) fn parse(text: &str) -> Snapshot {
    let mut out = Snapshot {
        prompt_tokens: None,
        generated_tokens: None,
        draft_tokens: None,
        accepted_tokens: None,
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(metric) = fields.next() else {
            continue;
        };
        if metric.starts_with('#') {
            continue;
        }
        let Some(raw) = fields.next() else { continue };
        let name = metric.split('{').next().unwrap_or(metric);
        let Ok(value) = raw.parse::<f64>() else {
            continue;
        };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        let slot = match name {
            "vllm:prompt_tokens_total" | "llamacpp:prompt_tokens_total" => &mut out.prompt_tokens,
            "vllm:generation_tokens_total" | "llamacpp:tokens_predicted_total" => {
                &mut out.generated_tokens
            }
            "vllm:spec_decode_num_draft_tokens_total" | "halogen:draft_tokens_total" => {
                &mut out.draft_tokens
            }
            "vllm:spec_decode_num_accepted_tokens_total"
            | "halogen:draft_tokens_accepted_total" => &mut out.accepted_tokens,
            _ => continue,
        };
        *slot = Some(slot.unwrap_or(0.0) + value);
    }
    out
}
pub(crate) fn delta(old: &Snapshot, new: &Snapshot) -> Delta {
    fn diff(a: Option<f64>, b: Option<f64>) -> Option<f64> {
        match (a, b) {
            (Some(a), Some(b)) if b >= a => Some(b - a),
            (Some(_), Some(_)) => None,
            _ => None,
        }
    }
    Delta {
        prompt_tokens: diff(old.prompt_tokens, new.prompt_tokens),
        generated_tokens: diff(old.generated_tokens, new.generated_tokens),
        draft_tokens: diff(old.draft_tokens, new.draft_tokens),
        accepted_tokens: diff(old.accepted_tokens, new.accepted_tokens),
    }
}
pub(crate) fn acceptance(d: &Delta) -> Option<f64> {
    match (d.accepted_tokens, d.draft_tokens) {
        (Some(a), Some(n)) if n > 0.0 && a <= n => Some(a / n),
        _ => None,
    }
}
// Select only the configured model; retain series identity and creation epochs.
type ScopedMetrics = (
    Snapshot,
    String,
    std::collections::BTreeMap<String, f64>,
    std::collections::BTreeMap<String, f64>,
);

fn scoped(text: &str, model: &str) -> Result<ScopedMetrics, &'static str> {
    let line_re =
        regex::Regex::new(r#"^([^\s{}]+)(\{(?:[^"\\]|"(?:\\.|[^"\\])*")*\})?\s+(\S+)(?:\s+\d+)?$"#)
            .unwrap();
    let label_re = regex::Regex::new(r#"([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\.|[^"\\])*)""#).unwrap();
    let mut filtered = String::new();
    let mut counters = std::collections::BTreeMap::new();
    let mut epochs = std::collections::BTreeMap::new();
    let mut gauges = std::collections::BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|x| !x.is_empty() && !x.starts_with('#'))
    {
        let Some(c) = line_re.captures(line) else {
            return Err("invalid_metrics");
        };
        let name = &c[1];
        let recognized = matches!(
            name,
            "vllm:prompt_tokens_total"
                | "vllm:generation_tokens_total"
                | "vllm:spec_decode_num_draft_tokens_total"
                | "vllm:spec_decode_num_accepted_tokens_total"
                | "llamacpp:prompt_tokens_total"
                | "llamacpp:tokens_predicted_total"
                | "halogen:draft_tokens_total"
                | "halogen:draft_tokens_accepted_total"
        );
        let epoch = matches!(
            name,
            "process_start_time_seconds"
                | "vllm:prompt_tokens_created"
                | "vllm:generation_tokens_created"
                | "vllm:spec_decode_num_draft_tokens_created"
                | "vllm:spec_decode_num_accepted_tokens_created"
        );
        let gauge_field = match name {
            "llamacpp:predicted_tokens_seconds" => Some("reported_generation_tokens_per_second"),
            "llamacpp:prompt_tokens_seconds" => Some("reported_prompt_tokens_per_second"),
            "llamacpp:requests_processing" | "vllm:num_requests_running" => {
                Some("requests_running")
            }
            "llamacpp:requests_deferred" | "vllm:num_requests_waiting" => Some("requests_waiting"),
            "llamacpp:kv_cache_tokens" => Some("kv_cache_tokens"),
            "llamacpp:kv_cache_usage_ratio" | "vllm:kv_cache_usage_perc" => {
                Some("kv_cache_usage_ratio")
            }
            _ => None,
        };
        if !recognized && !epoch && gauge_field.is_none() {
            continue;
        }
        let labels = c.get(2).map_or("", |x| x.as_str());
        let mut canonical = std::collections::BTreeMap::new();
        let inner = labels.trim_start_matches('{').trim_end_matches('}');
        let mut cursor = 0;
        for l in label_re.captures_iter(inner) {
            let m = l.get(0).unwrap();
            let gap = inner[cursor..m.start()].trim();
            if (cursor == 0 && !gap.is_empty()) || (cursor > 0 && gap != ",") {
                return Err("invalid_labels");
            }
            let value: String =
                serde_json::from_str(&format!("\"{}\"", &l[2])).map_err(|_| "invalid_labels")?;
            if canonical.insert(l[1].to_string(), value).is_some() {
                return Err("duplicate_labels");
            }
            cursor = m.end();
        }
        if !inner[cursor..].trim().is_empty() {
            return Err("invalid_labels");
        }
        if canonical.get("model_name").is_some_and(|x| x != model) {
            continue;
        }
        let value = c[3].parse::<f64>().map_err(|_| "invalid_metrics")?;
        if !value.is_finite() || value < 0.0 {
            return Err("invalid_metrics");
        }
        let key = format!("{name}:{}", serde_json::to_string(&canonical).unwrap());
        if let Some(field) = gauge_field {
            if field == "kv_cache_usage_ratio" && (value > 1.0 || gauges.contains_key(field)) {
                return Err("ambiguous_kv_usage");
            }
            *gauges.entry(field.to_string()).or_insert(0.0) += value;
        } else if epoch {
            if epochs.insert(key, value).is_some() {
                return Err("duplicate_series");
            }
        } else {
            if counters.insert(key, value).is_some() {
                return Err("duplicate_series");
            }
            filtered.push_str(&format!("{name} {value}\n"));
        }
    }
    let mut snapshot = parse(&filtered);
    let matching_drafts = [
        (
            "vllm:spec_decode_num_draft_tokens_total:",
            "vllm:spec_decode_num_accepted_tokens_total:",
        ),
        (
            "halogen:draft_tokens_total:",
            "halogen:draft_tokens_accepted_total:",
        ),
    ]
    .into_iter()
    .filter(|(draft, _)| counters.keys().any(|k| k.starts_with(draft)))
    .collect::<Vec<_>>();
    let matched = matching_drafts.len() == 1
        && matching_drafts.iter().all(|(draft, accepted)| {
            let proposed = counters
                .keys()
                .filter_map(|key| key.strip_prefix(draft))
                .collect::<std::collections::BTreeSet<_>>();
            let accepted = counters
                .keys()
                .filter_map(|key| key.strip_prefix(accepted))
                .collect::<std::collections::BTreeSet<_>>();
            !proposed.is_empty() && proposed == accepted
        });
    if !matched {
        snapshot.draft_tokens = None;
        snapshot.accepted_tokens = None;
    }
    if snapshot.generated_tokens.is_none() && snapshot.prompt_tokens.is_none() {
        return Err("unsupported_metrics");
    }
    let identity =
        serde_json::to_string(&(model, counters.keys().collect::<Vec<_>>(), epochs)).unwrap();
    Ok((snapshot, identity, counters, gauges))
}

#[derive(Default)]
pub(crate) struct Track {
    pub(crate) payload: Option<serde_json::Value>,
    previous: Option<(
        Snapshot,
        std::time::Instant,
        String,
        std::collections::BTreeMap<String, f64>,
    )>,
    checked: Option<std::time::Instant>,
}
impl Track {
    pub(crate) fn observe_slots(&mut self, text: &str, now: std::time::Instant) {
        self.fail("metrics_disabled", now);
        let Ok(slots) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
            return;
        };
        if slots
            .iter()
            .any(|s| !s["id"].is_u64() || !s["is_processing"].is_boolean())
        {
            return;
        }
        self.payload = Some(serde_json::json!({
            "status":"partial", "source":"llamacpp_slots", "reason":"metrics_disabled",
            "slots_total":slots.len(), "requests_running":slots.iter().filter(|s| s["is_processing"] == true).count(),
            "generation_tokens_per_second":null, "prompt_tokens_per_second":null,
            "acceptance_ratio":null, "acceptance_supported":false
        }));
    }
    pub(crate) fn fail(&mut self, reason: &str, now: std::time::Instant) {
        self.previous = None;
        self.checked = Some(now);
        self.payload = Some(serde_json::json!({"status":"unavailable", "reason":reason,
            "generation_tokens_per_second":null, "prompt_tokens_per_second":null,
            "acceptance_ratio":null, "acceptance_supported":false}));
    }
    pub(crate) fn observe(&mut self, text: &str, model: &str, now: std::time::Instant) {
        let (sample, identity, counters, gauges) = match scoped(text, model) {
            Ok(x) => x,
            Err(reason) => {
                self.fail(reason, now);
                return;
            }
        };
        let source = if counters.keys().any(|x| x.starts_with("halogen:")) {
            "halogen"
        } else if counters.keys().any(|x| x.starts_with("vllm:")) {
            "vllm"
        } else {
            "llamacpp"
        };
        let mut window = None;
        let mut generation = None;
        let mut prompt = None;
        let mut ratio = None;
        if let Some((old, at, old_identity, old_counters)) = &self.previous {
            let elapsed = now.saturating_duration_since(*at).as_secs_f64();
            if old_identity == &identity
                && elapsed > 0.0
                && counters
                    .iter()
                    .all(|(key, value)| old_counters.get(key).is_some_and(|old| value >= old))
            {
                let change = delta(old, &sample);
                window = Some(elapsed);
                generation = change
                    .generated_tokens
                    .map(|n| n / elapsed)
                    .filter(|n| n.is_finite());
                prompt = change
                    .prompt_tokens
                    .map(|n| n / elapsed)
                    .filter(|n| n.is_finite());
                ratio = acceptance(&change);
            }
        }
        self.payload = Some(serde_json::json!({
            "status":"available", "source":source, "model":model, "rate_source":"native_counter_delta",
            "window_seconds":window, "generation_tokens_per_second":generation,
            "prompt_tokens_per_second":prompt, "acceptance_ratio":ratio,
            "generation_tokens_total":sample.generated_tokens, "prompt_tokens_total":sample.prompt_tokens,
            "draft_tokens_total":sample.draft_tokens, "accepted_draft_tokens_total":sample.accepted_tokens,
            "acceptance_supported":sample.draft_tokens.is_some() && sample.accepted_tokens.is_some(),
            "acceptance_source":if source == "halogen" {"draft_head_and_prompt_lookup"} else {"speculative_draft_tokens"}
        }));
        for (field, value) in gauges {
            self.payload.as_mut().unwrap()[field] = serde_json::json!(value);
        }
        self.previous = Some((sample, now, identity, counters));
        self.checked = Some(now);
    }
    #[cfg(test)]
    pub(crate) fn view(&self) -> serde_json::Value {
        self.view_at(
            std::time::Instant::now(),
            std::time::Duration::from_secs(180),
        )
    }
    pub(crate) fn view_at(
        &self,
        now: std::time::Instant,
        max_age: std::time::Duration,
    ) -> serde_json::Value {
        let mut value = self.payload.clone().unwrap_or_else(|| {
            serde_json::json!({
                "status":"unavailable", "reason":"not_sampled",
                "generation_tokens_per_second":null, "prompt_tokens_per_second":null,
                "acceptance_ratio":null, "acceptance_supported":false
            })
        });
        let age = self.checked.map(|at| now.saturating_duration_since(at));
        value["age_seconds"] = serde_json::json!(age.map(|x| x.as_secs()));
        value["stale_after_seconds"] = serde_json::json!(max_age.as_secs());
        if age.is_some_and(|age| age > max_age)
            && matches!(value["status"].as_str(), Some("available" | "partial"))
        {
            value["status"] = serde_json::json!("stale");
            for key in [
                "generation_tokens_per_second",
                "prompt_tokens_per_second",
                "acceptance_ratio",
                "reported_generation_tokens_per_second",
                "reported_prompt_tokens_per_second",
                "requests_running",
                "requests_waiting",
                "kv_cache_usage_ratio",
                "kv_cache_tokens",
            ] {
                value[key] = serde_json::Value::Null;
            }
        }
        value
    }
}

pub(super) async fn fetch(
    runtime: &crate::Runtime,
    idx: usize,
    path: &str,
) -> Result<String, String> {
    let route = &runtime.policy.routes[idx];
    let base = route
        .config
        .base_url
        .strip_suffix("/v1")
        .unwrap_or(&route.config.base_url);
    let mut request = runtime
        .probes
        .get(format!("{base}/{path}"))
        .timeout(std::time::Duration::from_secs(5));
    if let Some(key) = &route.key {
        request = request.bearer_auth(String::from_utf8_lossy(key).as_ref());
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| "transport_error".to_string())?;
    if !response.status().is_success() {
        return Err(format!("http_{}", response.status().as_u16()));
    }
    if response.content_length().is_some_and(|n| n > 1_048_576) {
        return Err("response_too_large".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "body_error".to_string())?
    {
        if body.len().saturating_add(chunk.len()) > 1_048_576 {
            return Err("response_too_large".into());
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| "invalid_utf8".into())
}

pub(crate) async fn refresh(runtime: std::sync::Arc<crate::Runtime>, idx: usize) {
    let route = &runtime.policy.routes[idx];
    if !route.config.enabled
        || route.config.kind != crate::RouteKind::Local
        || route.config.physical_route.is_some()
    {
        return;
    }
    let result = fetch(&runtime, idx, "metrics").await;
    let slots = if result.as_ref().is_err_and(|e| e == "http_501") {
        Some(fetch(&runtime, idx, "slots").await)
    } else {
        None
    };
    let now = std::time::Instant::now();
    let mut telemetry = runtime.backend_telemetry.lock().unwrap();
    match (result, slots) {
        (Ok(text), _) => telemetry[idx].observe(&text, &route.config.model, now),
        (_, Some(Ok(text))) => telemetry[idx].observe_slots(&text, now),
        (Err(reason), _) => telemetry[idx].fail(&reason, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derives_acceptance_from_counter_deltas_not_lifetime_totals() {
        let a = parse(
            "vllm:spec_decode_num_drafts_total 99\nvllm:spec_decode_num_draft_tokens_total 90\nvllm:spec_decode_num_accepted_tokens_total 45\n",
        );
        let b = parse(
            "vllm:spec_decode_num_drafts_total 105\nvllm:spec_decode_num_draft_tokens_total 100\nvllm:spec_decode_num_accepted_tokens_total 50\n",
        );
        assert_eq!(acceptance(&delta(&a, &b)), Some(0.5));
    }
    #[test]
    fn tracking_exposes_window_rates_and_native_acceptance() {
        let now = std::time::Instant::now();
        let mut track = Track::default();
        track.observe("llamacpp:tokens_predicted_total 10\nllamacpp:prompt_tokens_total 100\nhalogen:draft_tokens_total 20\nhalogen:draft_tokens_accepted_total 10\n", "qwen", now);
        assert!(track.view()["generation_tokens_per_second"].is_null());
        track.observe("llamacpp:tokens_predicted_total 30\nllamacpp:prompt_tokens_total 200\nhalogen:draft_tokens_total 40\nhalogen:draft_tokens_accepted_total 25\n", "qwen", now + std::time::Duration::from_secs(10));
        let v = track.view();
        assert_eq!(v["status"], "available");
        assert_eq!(v["generation_tokens_per_second"], 2.0);
        assert_eq!(v["prompt_tokens_per_second"], 10.0);
        assert_eq!(v["acceptance_ratio"], 0.75);
        assert_eq!(v["source"], "halogen");
    }
    #[test]
    fn reset_invalidates_window() {
        let old = parse("halogen:draft_tokens_total 100\nhalogen:draft_tokens_accepted_total 40\n");
        let new = parse("halogen:draft_tokens_total 110\nhalogen:draft_tokens_accepted_total 1\n");
        assert_eq!(acceptance(&delta(&old, &new)), None);
        assert_eq!(
            parse("foreign:prompt_tokens_total 99\n").prompt_tokens,
            None
        );
    }
    #[test]
    fn tracking_scopes_models_and_invalidates_changed_epoch() {
        let now = std::time::Instant::now();
        let mut t = Track::default();
        t.observe("vllm:generation_tokens_total{model_name=\"other\",engine=\"0\"} 999\nvllm:generation_tokens_total{model_name=\"qwen\",engine=\"0\"} 10\nvllm:generation_tokens_created{model_name=\"qwen\",engine=\"0\"} 1\n", "qwen", now);
        assert_eq!(t.view()["generation_tokens_total"], 10.0);
        t.observe("vllm:generation_tokens_total{model_name=\"qwen\",engine=\"0\"} 100\nvllm:generation_tokens_created{model_name=\"qwen\",engine=\"0\"} 2\n", "qwen", now + std::time::Duration::from_secs(10));
        assert!(t.view()["generation_tokens_per_second"].is_null());
        t.observe("<html>not metrics</html>", "qwen", now);
        assert_eq!(t.view()["status"], "unavailable");
    }
    #[test]
    fn native_rates_are_separate_from_wall_clock_rates() {
        let mut t = Track::default();
        t.observe("llamacpp:tokens_predicted_total 10\nllamacpp:predicted_tokens_seconds 80\nllamacpp:prompt_tokens_seconds 1200\nllamacpp:requests_processing 2\nllamacpp:requests_deferred 1\nllamacpp:kv_cache_usage_ratio 0.4\n", "qwen", std::time::Instant::now());
        let v = t.view();
        assert_eq!(v["reported_generation_tokens_per_second"], 80.0);
        assert!(v["generation_tokens_per_second"].is_null());
        assert_eq!(v["reported_prompt_tokens_per_second"], 1200.0);
        assert_eq!(v["requests_running"], 2.0);
        assert_eq!(v["requests_waiting"], 1.0);
        assert_eq!(v["kv_cache_usage_ratio"], 0.4);
    }
    #[test]
    fn stale_data_suppresses_rates_and_recovery_rebaselines() {
        let now = std::time::Instant::now();
        let mut t = Track::default();
        t.observe("llamacpp:tokens_predicted_total 10\n", "qwen", now);
        t.observe(
            "llamacpp:tokens_predicted_total 30\n",
            "qwen",
            now + std::time::Duration::from_secs(10),
        );
        let v = t.view_at(
            now + std::time::Duration::from_secs(200),
            std::time::Duration::from_secs(180),
        );
        assert_eq!(v["status"], "stale");
        assert!(v["generation_tokens_per_second"].is_null());
        t.fail("http_503", now + std::time::Duration::from_secs(201));
        t.observe(
            "llamacpp:tokens_predicted_total 50\n",
            "qwen",
            now + std::time::Duration::from_secs(202),
        );
        assert!(t.view()["generation_tokens_per_second"].is_null());
    }
    #[test]
    fn slots_only_exposes_activity_without_inventing_acceptance_or_rates() {
        let mut t = Track::default();
        t.observe_slots(r#"[{"id":0,"is_processing":true,"n_prompt_tokens_processed":50,"next_token":[{"n_decoded":10}],"params":{"prompt":"PRIVATE"},"speculative":true},{"id":1,"is_processing":false}]"#, std::time::Instant::now());
        let v = t.view();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["slots_total"], 2);
        assert_eq!(v["requests_running"], 1);
        assert!(v["generation_tokens_per_second"].is_null());
        assert_eq!(v["acceptance_supported"], false);
        assert!(!v.to_string().contains("PRIVATE"));
        let v = t.view_at(
            std::time::Instant::now() + std::time::Duration::from_secs(200),
            std::time::Duration::from_secs(180),
        );
        assert_eq!(v["status"], "stale");
        assert!(v["requests_running"].is_null());
    }
    #[test]
    fn acceptance_requires_matching_engine_series() {
        let mut t = Track::default();
        let now = std::time::Instant::now();
        let text = "vllm:generation_tokens_total{model_name=\"qwen\",engine=\"0\"} 10\nvllm:spec_decode_num_draft_tokens_total{model_name=\"qwen\",engine=\"0\"} 10\nvllm:spec_decode_num_accepted_tokens_total{model_name=\"qwen\",engine=\"1\"} 5\n";
        t.observe(text, "qwen", now);
        t.observe(
            &text.replace(" 10", " 20").replace(" 5", " 9"),
            "qwen",
            now + std::time::Duration::from_secs(10),
        );
        assert!(t.view()["acceptance_ratio"].is_null());
        assert_eq!(t.view()["acceptance_supported"], false);
    }
    #[test]
    fn ignores_nonfinite_and_zero_denominator() {
        let a = parse("halogen:draft_tokens_total 4\nhalogen:draft_tokens_accepted_total 2\n");
        let b = parse("halogen:draft_tokens_total NaN\nhalogen:draft_tokens_accepted_total 3\n");
        assert_eq!(acceptance(&delta(&a, &b)), None);
        assert_eq!(parse("foo_prompt_tokens_total inf\n").prompt_tokens, None);
    }
}
