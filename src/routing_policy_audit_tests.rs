use super::*;
use std::io::{self, Write};
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        eprintln!("{text}");
        assert!(!text.contains(WORKLOAD_KEY));
        assert!(!text.contains("private task data"));
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn audit_fixture(upstream: &Upstream, max_attempts: usize) -> Fixture {
    *upstream.state.decision_usage.lock().unwrap() =
        Some(json!({"prompt_tokens":701,"completion_tokens":103,"total_tokens":804}));
    *upstream.state.target_usage.lock().unwrap() =
        Some(json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}));
    let mut fixture = fixture_with_custom_config(
        upstream,
        json!({"decision_model":"judge","decision_targets":["target"],"decision_timeout_seconds":1}),
        None,
        true,
        |config| {
            config["retry_policy"]["max_attempts"] = json!(max_attempts);
            config["credentials"][0]["requests_per_minute"] = json!(600);
            config["credentials"][0]["burst"] = json!(10);
            config["credentials"][0]["allowed_routes"] =
                json!(["judge-route", "target-primary", "target-backup"]);
            let route = config["routes"][0].clone();
            config["routes"] = json!([route.clone(), route.clone(), route]);
            for (index, name) in ["judge-route", "target-primary", "target-backup"]
                .into_iter()
                .enumerate()
            {
                config["routes"][index]["name"] = json!(name);
                config["routes"][index]["model"] = json!(name);
            }
            config["aliases"]["judge"]["routes"] = json!([{"route":"judge-route","priority":0}]);
            config["aliases"]["target"]["routes"] = json!([
                {"route":"target-primary","priority":0},
                {"route":"target-backup","priority":1}
            ]);
        },
        |_| {},
    );
    fixture.router = fixture
        .router
        .layer(axum::middleware::from_fn(crate::request_logging::correlate));
    fixture
}

async fn audited_request(fixture: &Fixture, model: &str) -> (StatusCode, String) {
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {WORKLOAD_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-request-id", "caller-controlled-id")
                .body(Body::from(json!({
                    "model":model,"messages":[{"role":"user","content":"private task data"}],"max_tokens":16
                }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(request_id.len(), 32);
    assert_ne!(request_id, "caller-controlled-id");
    axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    (status, request_id)
}

fn terminal<'event>(events: &'event [Value], request_id: &str) -> &'event Value {
    let terminals: Vec<_> = events
        .iter()
        .filter(|event| {
            event["span"]["request_id"] == request_id
                && matches!(
                    event["fields"]["message"].as_str(),
                    Some("request body completed" | "request handler cancelled")
                )
        })
        .collect();
    assert_eq!(terminals.len(), 1, "one correlated terminal event");
    &terminals[0]["fields"]
}

fn assert_permits(fixture: &Fixture) {
    let runtime = fixture.app.current();
    for semaphore in &runtime.semaphores {
        assert_eq!(semaphore.available_permits(), 1);
    }
    assert_eq!(
        runtime.auth.read().unwrap().credentials["fixture"]
            .class_semaphore
            .available_permits(),
        1
    );
    assert!(crate::audit::CURRENT.try_with(|_| ()).is_err());
}

fn assert_metric(metrics: &str, name: &str, route: &str, extra: &str, value: u64) {
    let expected = format!(
        "slotline_audit_{name}{{route=\"{route}\",class=\"fixture\",endpoint=\"chat\"{extra}}} {value}"
    );
    assert!(
        metrics.lines().any(|line| line == expected),
        "missing {expected}\n{metrics}"
    );
}

#[tokio::test]
async fn decision_audit_success_counts_only_target() {
    let upstream = decision_upstream().await;
    let fixture = audit_fixture(&upstream, 1);
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let (status, request_id) = audited_request(&fixture, "smart")
        .with_subscriber(subscriber)
        .await;
    assert_eq!(status, StatusCode::OK);
    let events = capture.events();
    let fields = terminal(&events, &request_id);
    assert_eq!(
        fields["attempts"], 1,
        "judge inference must not count as a target attempt"
    );
    assert_eq!(fields["retries"], 0);
    assert_eq!(fields["failed_attempts"], 0);
    assert_eq!(fields["outcome"], "success_first_attempt");
    assert_eq!(fields["backend_outcome"], "complete");
    assert_eq!(fields["last_attempt_outcome"], "validated");
    assert_eq!(fields["last_attempt_status"], 200);
    assert_eq!(fields["route"], "target-primary");
    assert_eq!(fields["input_tokens"], 11);
    assert_eq!(fields["output_tokens"], 7);
    assert_eq!(fields["total_tokens"], 18);
    let attempts: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "upstream attempt finished")
        .collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["span"]["request_id"], request_id);
    assert_eq!(attempts[0]["fields"]["route"], "target-primary");
    assert_eq!(attempts[0]["fields"]["attempt"], 1);
    let runtime = fixture.app.current();
    let metrics = runtime.audit_metrics.render();
    assert!(!metrics.contains("route=\"judge-route\""));
    assert_metric(
        &metrics,
        "attempt_milliseconds_count",
        "target-primary",
        "",
        1,
    );
    assert_metric(
        &metrics,
        "outcomes_total",
        "target-primary",
        ",outcome=\"success_first_attempt\"",
        1,
    );
    assert_metric(
        &metrics,
        "outcomes_total",
        "target-primary",
        ",outcome=\"recovered_after_retry\"",
        0,
    );
    assert_metric(
        &metrics,
        "usage_total_tokens_total",
        "target-primary",
        "",
        18,
    );
    assert_eq!(
        runtime.route_completions[0].load(AtomicOrdering::Relaxed),
        1
    );
    assert_eq!(
        runtime.route_completions[1].load(AtomicOrdering::Relaxed),
        1
    );
    assert_permits(&fixture);
}

#[tokio::test]
async fn decision_audit_missing_target_usage_stays_unknown() {
    let upstream = decision_upstream().await;
    let fixture = audit_fixture(&upstream, 1);
    *upstream.state.target_usage.lock().unwrap() = None;
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let (status, request_id) = audited_request(&fixture, "smart")
        .with_subscriber(subscriber)
        .await;
    assert_eq!(status, StatusCode::OK);
    let events = capture.events();
    let fields = terminal(&events, &request_id);
    for field in ["input_tokens", "output_tokens", "total_tokens"] {
        assert!(
            fields[field].is_null(),
            "target without usage must not inherit judge {field}: {fields}"
        );
    }
    assert_eq!(fields["attempts"], 1);
    assert_eq!(fields["route"], "target-primary");
    let metrics = fixture.app.current().audit_metrics.render();
    for kind in ["input", "output", "total"] {
        assert_metric(
            &metrics,
            &format!("usage_{kind}_tokens_samples_total"),
            "target-primary",
            "",
            0,
        );
    }
    assert_permits(&fixture);
}

#[tokio::test]
async fn decision_audit_genuine_target_retry_is_preserved() {
    let upstream = decision_upstream().await;
    let fixture = audit_fixture(&upstream, 2);
    *upstream.state.failing_target.lock().unwrap() = Some("target-primary".into());
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let (status, request_id) = audited_request(&fixture, "smart")
        .with_subscriber(subscriber)
        .await;
    assert_eq!(status, StatusCode::OK);
    let events = capture.events();
    let fields = terminal(&events, &request_id);
    assert_eq!(fields["attempts"], 2);
    assert_eq!(fields["retries"], 1);
    assert_eq!(fields["failed_attempts"], 1);
    assert_eq!(fields["outcome"], "recovered_after_retry");
    assert_eq!(fields["backend_outcome"], "complete");
    assert_eq!(fields["route"], "target-backup");
    assert_eq!(fields["total_tokens"], 18);
    let attempts: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "upstream attempt finished")
        .collect();
    assert_eq!(attempts.len(), 2);
    for (index, event) in attempts.iter().enumerate() {
        assert_eq!(event["span"]["request_id"], request_id);
        assert_eq!(event["fields"]["attempt"], index + 1);
    }
    assert_eq!(attempts[0]["fields"]["http_status"], 502);
    assert_eq!(attempts[1]["fields"]["outcome"], "validated");
    let metrics = fixture.app.current().audit_metrics.render();
    assert!(!metrics.contains("route=\"judge-route\""));
    assert_metric(
        &metrics,
        "attempt_milliseconds_count",
        "target-primary",
        "",
        1,
    );
    assert_metric(
        &metrics,
        "attempt_milliseconds_count",
        "target-backup",
        "",
        1,
    );
    assert_metric(
        &metrics,
        "outcomes_total",
        "target-backup",
        ",outcome=\"recovered_after_retry\"",
        1,
    );
    assert_permits(&fixture);
}

#[tokio::test]
async fn decision_audit_target_failure_is_not_a_retry() {
    let upstream = decision_upstream().await;
    let fixture = audit_fixture(&upstream, 1);
    *upstream.state.failing_target.lock().unwrap() = Some("target-primary".into());
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let (status, request_id) = audited_request(&fixture, "smart")
        .with_subscriber(subscriber)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let events = capture.events();
    let fields = terminal(&events, &request_id);
    assert_eq!(fields["attempts"], 1);
    assert_eq!(fields["retries"], 0);
    assert_eq!(fields["failed_attempts"], 1);
    assert_eq!(fields["outcome"], "failed");
    assert_eq!(fields["backend_outcome"], "failed");
    assert_eq!(fields["route"], "target-primary");
    assert!(fields["total_tokens"].is_null());
    let metrics = fixture.app.current().audit_metrics.render();
    assert_metric(
        &metrics,
        "outcomes_total",
        "target-primary",
        ",outcome=\"failed\"",
        1,
    );
    assert_metric(
        &metrics,
        "backend_total",
        "target-primary",
        ",outcome=\"failed\"",
        1,
    );
    assert_permits(&fixture);
}

#[tokio::test]
async fn decision_audit_failures_do_not_claim_target_execution() {
    for mode in ["invalid", "failure", "timeout"] {
        let upstream = decision_upstream().await;
        let fixture = audit_fixture(&upstream, 1);
        match mode {
            "invalid" => *upstream.state.decision_reply.lock().unwrap() = DecisionReply::Malformed,
            "failure" => {
                *upstream.state.decision_status.lock().unwrap() = Some(StatusCode::BAD_GATEWAY)
            }
            _ => *upstream.state.decision_reply.lock().unwrap() = DecisionReply::Timeout,
        }
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.clone())
            .finish();
        let (status, request_id) = audited_request(&fixture, "smart")
            .with_subscriber(subscriber)
            .await;
        assert!(status.is_server_error());
        assert_eq!(
            status,
            match mode {
                "invalid" => StatusCode::BAD_GATEWAY,
                "failure" => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::GATEWAY_TIMEOUT,
            }
        );
        let events = capture.events();
        let fields = terminal(&events, &request_id);
        assert_eq!(fields["attempts"], 0, "{mode}");
        assert_eq!(fields["retries"], 0);
        assert_eq!(fields["failed_attempts"], 0);
        assert_eq!(fields["route"], "unselected");
        assert_eq!(fields["backend_outcome"], "unavailable");
        assert!(fields["total_tokens"].is_null());
        assert_eq!(upstream.state.target_calls.load(AtomicOrdering::Relaxed), 0);
        assert_permits(&fixture);
    }
}

#[tokio::test]
async fn decision_audit_cancellation_restores_context_and_permits() {
    let upstream = decision_upstream().await;
    let fixture = audit_fixture(&upstream, 1);
    *upstream.state.decision_reply.lock().unwrap() = DecisionReply::Timeout;
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let (_, next_request_id) = async {
        {
            let request = audited_request(&fixture, "smart");
            tokio::pin!(request);
            tokio::select! {
                _ = &mut request => panic!("judge must still be in flight"),
                _ = tokio::time::timeout(Duration::from_secs(1), async {
                    while upstream.state.decision_calls.load(AtomicOrdering::Relaxed) == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }) => assert_eq!(upstream.state.decision_calls.load(AtomicOrdering::Relaxed), 1),
            }
        }
        assert_eq!(upstream.state.target_calls.load(AtomicOrdering::Relaxed), 0);
        assert_permits(&fixture);
        audited_request(&fixture, "target").await
    }
    .with_subscriber(subscriber)
    .await;
    let events = capture.events();
    let cancelled: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "request handler cancelled")
        .collect();
    assert_eq!(cancelled.len(), 1);
    let fields = &cancelled[0]["fields"];
    assert_eq!(fields["attempts"], 0);
    assert_eq!(fields["retries"], 0);
    assert_eq!(fields["outcome"], "cancelled");
    assert_eq!(fields["backend_outcome"], "unavailable");
    assert_ne!(cancelled[0]["span"]["request_id"], next_request_id);
    let next = terminal(&events, &next_request_id);
    assert_eq!(next["attempts"], 1);
    assert_eq!(next["outcome"], "success_first_attempt");
    assert_eq!(next["total_tokens"], 18);
    assert_permits(&fixture);
}
