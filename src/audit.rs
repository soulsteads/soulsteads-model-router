use crate::{Runtime, audit_identity, audit_sink};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

pub(crate) const BUCKETS: [u64; 17] = [
    1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000, 120000, 300000, 900000,
];
const MAX_GROUPS: usize = 1024;
const OUTCOMES: [&str; 8] = [
    "success_first_attempt",
    "recovered_after_retry",
    "failed",
    "failed_after_retry",
    "overload",
    "rejected",
    "cancelled",
    "passthrough_complete",
];

#[derive(Clone, Default)]
struct Histogram {
    counts: [u64; 18],
    sum: u64,
}
impl Histogram {
    fn observe(&mut self, elapsed: u64) {
        self.counts[BUCKETS.partition_point(|bound| *bound < elapsed)] += 1;
        self.sum = self.sum.saturating_add(elapsed);
    }
    fn render(&self, name: &str, labels: &str, output: &mut String) {
        let mut count = 0;
        for (idx, bound) in BUCKETS.iter().enumerate() {
            count += self.counts[idx];
            output.push_str(&format!(
                "slotline_audit_{name}_milliseconds_bucket{{{labels},le=\"{bound}\"}} {count}\n"
            ));
        }
        count += self.counts[BUCKETS.len()];
        output.push_str(&format!("slotline_audit_{name}_milliseconds_bucket{{{labels},le=\"+Inf\"}} {count}\nslotline_audit_{name}_milliseconds_sum{{{labels}}} {}\nslotline_audit_{name}_milliseconds_count{{{labels}}} {count}\n",self.sum));
    }
}
#[derive(Clone, Default)]
struct Group {
    duration: Histogram,
    headers: Histogram,
    first_output: Histogram,
    attempts: Histogram,
    outcomes: [u64; 8],
    backend_complete: u64,
    backend_failed: u64,
    backend_incomplete: u64,
    backend_unknown: u64,
    delivery_complete: u64,
    usage_samples: [u64; 3],
    usage_tokens: [u64; 3],
}
#[derive(Default)]
pub(crate) struct Metrics {
    groups: Mutex<BTreeMap<(String, String, &'static str), Group>>,
    pub(crate) dropped: AtomicU64,
}
impl Metrics {
    fn update(
        &self,
        route: &str,
        class: &str,
        endpoint: &'static str,
        update: impl FnOnce(&mut Group),
    ) {
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let key = (label(route).to_owned(), label(class).to_owned(), endpoint);
        if !groups.contains_key(&key) && groups.len() >= MAX_GROUPS {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        update(groups.entry(key).or_default());
    }
    pub(crate) fn snapshot(&self) -> Value {
        json!({"groups":self.groups.lock().unwrap_or_else(|error| error.into_inner()).len(),"max_groups":MAX_GROUPS,"observations_dropped":self.dropped.load(Ordering::Relaxed)})
    }
    pub(crate) fn render(&self) -> String {
        let mut output = format!(
            "slotline_audit_observations_dropped_total {}\n",
            self.dropped.load(Ordering::Relaxed)
        );
        for name in ["duration", "headers", "first_output", "attempt"] {
            output.push_str(&format!(
                "# TYPE slotline_audit_{name}_milliseconds histogram\n"
            ));
        }
        for (name, counter) in [
            ("queue_full", &audit_sink::COUNTERS.full),
            ("format_dropped", &audit_sink::COUNTERS.oversize),
            ("sink_errors", &audit_sink::COUNTERS.errors),
            ("disconnected", &audit_sink::COUNTERS.disconnected),
            ("written", &audit_sink::COUNTERS.written),
            ("abandoned", &audit_sink::COUNTERS.abandoned),
        ] {
            output.push_str(&format!(
                "slotline_audit_log_{name}_total {}\n",
                counter.load(Ordering::Relaxed)
            ));
        }
        let groups = self
            .groups
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for ((route, class, endpoint), group) in &groups {
            let labels = format!(
                "route={},class={},endpoint=\"{endpoint}\"",
                json!(route),
                json!(class)
            );
            for (name, histogram) in [
                ("duration", &group.duration),
                ("headers", &group.headers),
                ("first_output", &group.first_output),
                ("attempt", &group.attempts),
            ] {
                histogram.render(name, &labels, &mut output);
            }
            for (idx, outcome) in OUTCOMES.iter().enumerate() {
                output.push_str(&format!(
                    "slotline_audit_outcomes_total{{{labels},outcome=\"{outcome}\"}} {}\n",
                    group.outcomes[idx]
                ));
            }
            output.push_str(&format!("slotline_audit_backend_total{{{labels},outcome=\"complete\"}} {}\nslotline_audit_backend_total{{{labels},outcome=\"failed\"}} {}\n",group.backend_complete,group.backend_failed));
            output.push_str(&format!("slotline_audit_backend_total{{{labels},outcome=\"incomplete\"}} {}\nslotline_audit_backend_total{{{labels},outcome=\"unknown\"}} {}\nslotline_audit_delivery_completed_total{{{labels}}} {}\n",group.backend_incomplete,group.backend_unknown,group.delivery_complete));
            for (idx, name) in ["input", "output", "total"].iter().enumerate() {
                output.push_str(&format!("slotline_audit_usage_{name}_tokens_samples_total{{{labels}}} {}\nslotline_audit_usage_{name}_tokens_total{{{labels}}} {}\n",group.usage_samples[idx],group.usage_tokens[idx]));
            }
        }
        output
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) input: Option<u64>,
    pub(crate) output: Option<u64>,
    pub(crate) total: Option<u64>,
}
impl Usage {
    pub(crate) fn read(value: &Value) -> Self {
        let usage = value.get("usage").or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("usage"))
        });
        let get = |names: &[&str]| {
            usage.and_then(|usage| {
                names.iter().find_map(|name| {
                    usage
                        .get(*name)
                        .and_then(Value::as_u64)
                        .filter(|count| *count <= 1_000_000_000)
                })
            })
        };
        Self {
            input: get(&["input_tokens", "prompt_tokens"]),
            output: get(&["output_tokens", "completion_tokens"]),
            total: get(&["total_tokens"]),
        }
    }
    fn merge(&mut self, next: Self) {
        if next.input.is_some() {
            self.input = next.input;
        }
        if next.output.is_some() {
            self.output = next.output;
        }
        if next.total.is_some() {
            self.total = next.total;
        }
    }
}

pub(crate) struct RequestAudit {
    pub(crate) started: Instant,
    runtime: Option<Arc<Runtime>>,
    endpoint: &'static str,
    route: String,
    class: String,
    attempts: u64,
    failed_attempts: u64,
    last_attempt_outcome: &'static str,
    last_attempt_status: Option<u16>,
    passthrough_target: Option<(Arc<Runtime>, usize)>,
    first_output: Option<u64>,
    usage: Usage,
    backend: &'static str,
    passthrough: bool,
}
pub(crate) type Context = Arc<Mutex<RequestAudit>>;
tokio::task_local! { pub(crate) static CURRENT: Context; }
pub(crate) fn context(started: Instant, endpoint: &'static str) -> Context {
    Arc::new(Mutex::new(RequestAudit {
        started,
        runtime: None,
        endpoint,
        route: "unselected".into(),
        class: "unknown".into(),
        attempts: 0,
        failed_attempts: 0,
        last_attempt_outcome: "unavailable",
        last_attempt_status: None,
        passthrough_target: None,
        first_output: None,
        usage: Usage::default(),
        backend: "unavailable",
        passthrough: false,
    }))
}
fn current(update: impl FnOnce(&mut RequestAudit)) {
    let _ = CURRENT
        .try_with(|context| update(&mut context.lock().unwrap_or_else(|error| error.into_inner())));
}
pub(crate) fn bind(runtime: &Arc<Runtime>) {
    current(|request| {
        if request.runtime.is_none() {
            request.runtime = Some(runtime.clone());
            tracing::info!(build_source_sha256=audit_identity::BUILD,policy_sha256=%runtime.policy_digest,policy_generation=runtime.policy_generation,"request policy");
        }
    });
}
pub(crate) fn category(class: &str) {
    current(|request| {
        if request.runtime.as_ref().is_some_and(|runtime| {
            runtime
                .policy
                .config
                .credentials
                .iter()
                .any(|credential| credential.class == class)
        }) {
            request.class = label(class).to_owned();
        }
    });
}
pub(crate) fn label(value: &str) -> &str {
    if !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        value
    } else {
        "unknown"
    }
}
pub(crate) fn passthrough(runtime: &Arc<Runtime>, idx: usize) {
    current(|request| {
        request.passthrough = true;
        request.passthrough_target = Some((runtime.clone(), idx));
    });
}
pub(crate) fn passthrough_target() -> Option<(Arc<Runtime>, usize)> {
    CURRENT
        .try_with(|context| {
            context
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .passthrough_target
                .clone()
        })
        .ok()
        .flatten()
}
pub(crate) struct Attempt<'runtime> {
    runtime: &'runtime Runtime,
    idx: usize,
    started: Instant,
    context: Option<Context>,
    number: u64,
    finished: bool,
}
impl<'runtime> Attempt<'runtime> {
    pub(crate) fn new(runtime: &'runtime Runtime, idx: usize) -> Self {
        let context = CURRENT.try_with(Clone::clone).ok();
        let number = if let Some(context) = &context {
            let mut request = context.lock().unwrap_or_else(|error| error.into_inner());
            request.attempts = request.attempts.saturating_add(1);
            request.route = label(&runtime.policy.routes[idx].config.name).to_owned();
            tracing::info!(
                attempt = request.attempts,
                route = label(&runtime.policy.routes[idx].config.name),
                route_status = runtime.observations.lock().unwrap()[idx].status.name(),
                "upstream attempt started"
            );
            request.attempts
        } else {
            0
        };
        Self {
            runtime,
            idx,
            started: Instant::now(),
            context,
            number,
            finished: false,
        }
    }
    pub(crate) fn finish(&mut self, outcome: &'static str, http_status: Option<u16>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(context) = &self.context {
            let mut request = context.lock().unwrap_or_else(|error| error.into_inner());
            request.last_attempt_outcome = outcome;
            request.last_attempt_status = http_status;
            if !matches!(
                outcome,
                "validated" | "stream_headers" | "passthrough_headers" | "cancelled"
            ) {
                request.failed_attempts = request.failed_attempts.saturating_add(1);
            }
            let elapsed = self.started.elapsed().as_millis() as u64;
            if request.endpoint != "other" {
                self.runtime.audit_metrics.update(
                    &self.runtime.policy.routes[self.idx].config.name,
                    &request.class,
                    request.endpoint,
                    |group| group.attempts.observe(elapsed),
                );
            }
            tracing::info!(
                attempt = self.number,
                route = label(&self.runtime.policy.routes[self.idx].config.name),
                elapsed_ms = elapsed,
                outcome,
                http_status,
                "upstream attempt finished"
            );
        }
    }
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        self.finish("cancelled", None);
    }
}
pub(crate) fn observe(value: &Value, output_kind: Option<&'static str>) {
    current(|request| {
        request.usage.merge(Usage::read(value));
        if request.first_output.is_none()
            && let Some(kind) = output_kind
        {
            let elapsed = request.started.elapsed().as_millis() as u64;
            request.first_output = Some(elapsed);
            tracing::info!(
                elapsed_ms = elapsed,
                output_kind = kind,
                "request first output"
            );
        }
    });
}
pub(crate) fn backend(outcome: &'static str) {
    current(|request| request.backend = outcome);
}
fn nonempty(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
}
pub(crate) fn meaningful(value: &Value, responses: bool) -> Option<&'static str> {
    if responses {
        let kind = value.get("type").and_then(Value::as_str)?;
        if nonempty(value.get("delta")) {
            return match kind {
                "response.output_text.delta" | "response.refusal.delta" => Some("text"),
                "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                    Some("reasoning")
                }
                "response.function_call_arguments.delta" => Some("tool_arguments"),
                _ => None,
            };
        }
        fn item_kind(item: &Value) -> Option<&'static str> {
            match item.get("type").and_then(Value::as_str)? {
                "function_call" if nonempty(item.get("arguments")) => Some("tool_arguments"),
                "reasoning"
                    if item
                        .get("summary")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| nonempty(part.get("text")))
                        }) =>
                {
                    Some("reasoning")
                }
                "message"
                    if item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                nonempty(part.get("text")) || nonempty(part.get("refusal"))
                            })
                        }) =>
                {
                    Some("text")
                }
                _ => None,
            }
        }
        if matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        ) {
            return value.get("item").and_then(item_kind);
        }
        if matches!(kind, "response.completed" | "response.incomplete") {
            return value
                .get("response")
                .and_then(|response| response.get("output"))
                .and_then(Value::as_array)
                .and_then(|items| items.iter().find_map(item_kind));
        }
        return None;
    }
    value
        .get("choices")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|choice| {
            let delta = choice.get("delta")?;
            if nonempty(delta.get("content")) || nonempty(delta.get("refusal")) {
                Some("text")
            } else if nonempty(delta.get("reasoning")) || nonempty(delta.get("reasoning_content")) {
                Some("reasoning")
            } else if delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| {
                    calls.iter().any(|call| {
                        nonempty(
                            call.get("function")
                                .and_then(|function| function.get("arguments")),
                        )
                    })
                })
            {
                Some("tool_arguments")
            } else {
                None
            }
        })
}
pub(crate) fn headers(context: &Context, elapsed: u64) {
    let request = context.lock().unwrap_or_else(|error| error.into_inner());
    if request.endpoint != "other"
        && let Some(runtime) = &request.runtime
    {
        runtime
            .audit_metrics
            .update(&request.route, &request.class, request.endpoint, |group| {
                group.headers.observe(elapsed)
            });
    }
}
pub(crate) fn terminal(context: &Context, status: u16, message: &'static str) {
    let request = context.lock().unwrap_or_else(|error| error.into_inner());
    let cancelled = matches!(
        message,
        "request body cancelled" | "request handler cancelled"
    );
    let failed = message == "request stream failed" || status >= 500;
    let outcome = if cancelled {
        "cancelled"
    } else if status == 429 || (status == 503 && request.attempts == 0) {
        "overload"
    } else if failed && request.attempts > 1 {
        "failed_after_retry"
    } else if failed {
        "failed"
    } else if status >= 400 {
        "rejected"
    } else if request.passthrough {
        "passthrough_complete"
    } else if request.attempts > 1 {
        "recovered_after_retry"
    } else {
        "success_first_attempt"
    };
    let elapsed = request.started.elapsed().as_millis() as u64;
    if request.endpoint != "other"
        && let Some(runtime) = &request.runtime
    {
        runtime
            .audit_metrics
            .update(&request.route, &request.class, request.endpoint, |group| {
                group.duration.observe(elapsed);
                group.outcomes[OUTCOMES.iter().position(|name| *name == outcome).unwrap()] += 1;
                if let Some(first) = request.first_output {
                    group.first_output.observe(first);
                }
                group.backend_complete += u64::from(request.backend == "complete");
                group.backend_failed += u64::from(request.backend == "failed");
                group.backend_incomplete += u64::from(request.backend == "incomplete");
                group.backend_unknown += u64::from(request.backend == "unavailable");
                group.delivery_complete +=
                    u64::from(message == "request body completed" && status < 400);
                for (idx, tokens) in [
                    request.usage.input,
                    request.usage.output,
                    request.usage.total,
                ]
                .into_iter()
                .enumerate()
                {
                    if let Some(tokens) = tokens {
                        group.usage_samples[idx] += 1;
                        group.usage_tokens[idx] = group.usage_tokens[idx].saturating_add(tokens);
                    }
                }
            });
    }
    tracing::info!(message,status,elapsed_ms=elapsed,outcome,attempts=request.attempts,failed_attempts=request.failed_attempts,last_attempt_outcome=request.last_attempt_outcome,last_attempt_status=request.last_attempt_status,retries=request.attempts.saturating_sub(1),route=%request.route,class=%request.class,build_source_sha256=audit_identity::BUILD,policy_sha256=request.runtime.as_ref().map(|runtime|runtime.policy_digest.as_str()),policy_generation=request.runtime.as_ref().map(|runtime|runtime.policy_generation),first_output_ms=request.first_output,input_tokens=request.usage.input,output_tokens=request.usage.output,total_tokens=request.usage.total,backend_outcome=request.backend);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_bounded_and_histogram_buckets_are_cumulative() {
        let metrics = Metrics::default();
        metrics.update("route", "bad\nclass", "chat", |group| {
            for elapsed in [0, 5, 6, 900001] {
                group.duration.observe(elapsed);
            }
        });
        let output = metrics.render();
        let labels = "route=\"route\",class=\"unknown\",endpoint=\"chat\"";
        assert!(output.contains(&format!(
            "duration_milliseconds_bucket{{{labels},le=\"5\"}} 2"
        )));
        assert!(output.contains(&format!(
            "duration_milliseconds_bucket{{{labels},le=\"10\"}} 3"
        )));
        assert!(output.contains(&format!(
            "duration_milliseconds_bucket{{{labels},le=\"+Inf\"}} 4"
        )));
        assert!(output.contains(&format!("duration_milliseconds_sum{{{labels}}} 900012")));
        assert!(output.contains(&format!("duration_milliseconds_count{{{labels}}} 4")));
        assert!(output.contains(&format!("first_output_milliseconds_count{{{labels}}} 0")));
        for idx in 0..MAX_GROUPS {
            metrics.update(&format!("route-{idx}"), "class", "chat", |_| {});
        }
        assert_eq!(metrics.snapshot()["groups"], MAX_GROUPS);
        assert_eq!(metrics.snapshot()["observations_dropped"], 1);
        for invalid in ["", "\n", "quoted\"", "ユニコード", &"x".repeat(65)] {
            assert_eq!(label(invalid), "unknown");
        }
    }
}
