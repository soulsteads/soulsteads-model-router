use super::*;
use std::io::Write;

#[tokio::test]
async fn audit_deadline_after_selection_records_no_send_or_retry() {
    use tracing::instrument::WithSubscriber;
    let (_dir, app) = capacity_test_app();
    let old = app.current();
    let mut policy = config::load(&app.path).unwrap();
    policy.config.retry_policy.total_seconds = 1;
    policy.config.class_timeout_budgets.clear();
    let runtime = make_runtime(policy, Some(&old)).unwrap();
    let credential = runtime.auth.read().unwrap().credentials["operator"].clone();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let started = Instant::now();
    let context = crate::audit::context(started, "chat");
    let locked_runtime = runtime.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    // Hold the existing observation lock while selection is in progress. The
    // request enters with budget remaining, then selection finishes after its
    // deadline. This exercises the real forwarding preflight, not an expired
    // request that would return before any route was selected.
    let blocker = std::thread::spawn(move || {
        let _guard = locked_runtime.observations.lock().unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
    });
    ready_rx.recv().unwrap();
    assert!(
        runtime
            .policy
            .config
            .retry_policy
            .remaining(None, started)
            .is_some()
    );
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&runtime);
            crate::audit::category(&credential.class);
            let response = forward_validated(
                &runtime,
                &credential,
                "qwen38",
                &runtime.policy.config.aliases["qwen38"],
                &json!({"model":"qwen38"}),
                false,
                10,
                RequestCapabilities::default(),
                None,
                started,
                ResponseContract::Chat,
            )
            .await
            .err()
            .expect("pre-send deadline must fail");
            assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
            crate::audit::terminal(&context, 504, "request body completed");
        })
        .with_subscriber(subscriber)
        .await;
    blocker.join().unwrap();
    assert_eq!(
        runtime
            .route_attempts
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .sum::<u64>(),
        1,
        "selection must have occurred before the preflight timeout"
    );
    let events = capture.events();
    let terminal = &events.last().unwrap()["fields"];
    assert_eq!(
        terminal["attempts"], 0,
        "route reservation is not an outgoing send"
    );
    assert_eq!(terminal["retries"], 0);
    assert_ne!(terminal["outcome"], "recovered_after_retry");
    assert!(!events.iter().any(
        |event| event["fields"]["message"] == "upstream attempt started"
            || event["fields"]["message"] == "upstream attempt finished"
    ));
    for line in runtime
        .audit_metrics
        .render()
        .lines()
        .filter(|line| line.starts_with("slotline_audit_attempt_milliseconds_count{"))
    {
        assert!(line.ends_with(" 0"), "unexpected attempt sample: {line}");
    }
}

#[tokio::test]
async fn audit_retains_bounded_failed_attempt_status_and_category() {
    use tracing::instrument::WithSubscriber;
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let context = crate::audit::context(Instant::now(), "chat");
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&runtime);
            let mut first = crate::audit::Attempt::new(&runtime, 0);
            first.finish("transport", None);
            let mut second = crate::audit::Attempt::new(&runtime, 1);
            second.finish("invalid_body", Some(200));
            crate::audit::backend("failed");
            crate::audit::terminal(&context, 502, "request body completed");
        })
        .with_subscriber(subscriber)
        .await;
    let events = capture.events();
    let terminal = &events.last().unwrap()["fields"];
    assert_eq!(terminal["failed_attempts"], 2);
    assert_eq!(terminal["last_attempt_outcome"], "invalid_body");
    assert_eq!(terminal["last_attempt_status"], 200);
    assert_eq!(terminal["attempts"], 2);
    assert_eq!(terminal["retries"], 1);
}

#[tokio::test]
#[ignore = "requires loopback socket permission; explicit HTTP verification"]
async fn audit_passthrough_observes_actual_send_without_interpreting_body() {
    use tracing::instrument::WithSubscriber;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/opaque", axum::routing::post(|| async { "opaque" })),
        )
        .await
        .unwrap();
    });
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let context = crate::audit::context(Instant::now(), "chat");
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&runtime);
            crate::audit::passthrough(&runtime, 0);
            let response = crate::passthrough::forward(
                &runtime.client,
                &format!("http://{addr}"),
                "/opaque",
                axum::http::Method::POST,
                &HeaderMap::new(),
                vec![],
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), b"opaque");
            crate::audit::terminal(&context, 200, "request body completed");
        })
        .with_subscriber(subscriber)
        .await;
    server.abort();
    let events = capture.events();
    let attempts: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "upstream attempt finished")
        .collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["fields"]["outcome"], "passthrough_headers");
    let terminal = &events.last().unwrap()["fields"];
    assert_eq!(terminal["attempts"], 1);
    assert_eq!(terminal["backend_outcome"], "unavailable");
}

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
        std::str::from_utf8(&self.0.lock().unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
#[test]
fn audit_selection_explains_unchanged_free_slot_choice() {
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let (idx, permit) = select_test_route(&runtime, None).unwrap();
        assert_eq!(idx, 1);
        drop(permit);
    });
    let events = capture.events();
    let decision = events
        .iter()
        .find(|event| event["fields"]["message"] == "selection decision")
        .expect("missing bounded selection diagnostic");
    assert_eq!(decision["fields"]["reason"], "occupancy_rank");
    let candidates: Value =
        serde_json::from_str(decision["fields"]["candidates"].as_str().unwrap()).unwrap();
    assert_eq!(candidates[1]["free"], 10);
    assert_eq!(candidates[1]["status"], "serving");
    assert_eq!(candidates[1]["eligibility"], "eligible");
}

#[tokio::test]
async fn audit_metrics_bound_labels_and_separate_backend_delivery_samples() {
    let (_dir, app) = capacity_test_app();
    let mut policy = config::load(&app.path).unwrap();
    policy.routes[0].config.name = "PRIVATE\n\"\\".repeat(100);
    let runtime = make_runtime(policy, None).unwrap();
    let context = crate::audit::context(Instant::now(), "chat");
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&runtime);
            record_attempt(&runtime, 0);
            let mut attempt = crate::audit::Attempt::new(&runtime, 0);
            attempt.finish("stream_headers", Some(200));
            crate::audit::observe(
                &json!({"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}),
                Some("text"),
            );
            crate::audit::backend("complete");
            crate::audit::terminal(&context, 200, "request body cancelled");
        })
        .await;
    let metrics = runtime.audit_metrics.render();
    assert!(!metrics.contains("PRIVATE"));
    assert!(metrics.contains("route=\"unknown\""));
    assert!(metrics.contains("outcome=\"complete\"} 1"));
    assert!(metrics.contains("outcome=\"cancelled\"} 1"));
    assert!(metrics.contains("slotline_audit_delivery_completed_total{route=\"unknown\",class=\"unknown\",endpoint=\"chat\"} 0"));
    assert!(metrics.contains("slotline_audit_usage_total_tokens_samples_total{route=\"unknown\",class=\"unknown\",endpoint=\"chat\"} 1"));
    assert!(metrics.contains("slotline_audit_first_output_milliseconds_count{route=\"unknown\",class=\"unknown\",endpoint=\"chat\"} 1"));
    assert!(metrics.contains("slotline_audit_duration_milliseconds_bucket{route=\"unknown\",class=\"unknown\",endpoint=\"chat\",le=\"+Inf\"} 1"));
}

#[tokio::test]
#[ignore = "requires loopback socket permission; explicit HTTP verification"]
async fn audit_attempt_timeline_successful_recovery_and_metrics() {
    use tracing::instrument::WithSubscriber;
    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_addr = first.local_addr().unwrap();
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second_addr = second.local_addr().unwrap();
    let failure = tokio::spawn(async move {
        axum::serve(
            first,
            Router::new().route(
                "/v1/chat/completions",
                post(|| async { StatusCode::BAD_GATEWAY }),
            ),
        )
        .await
        .unwrap()
    });
    let success = tokio::spawn(async move {
        axum::serve(second,Router::new().route("/v1/chat/completions",post(||async {axum::Json(json!({"model":"qwen38","choices":[{"message":{"content":"PRIVATE"}}],"usage":{"prompt_tokens":9,"completion_tokens":1,"total_tokens":10}}))}))).await.unwrap()
    });
    let (_dir, app) = capacity_test_app();
    let mut policy = config::load(&app.path).unwrap();
    for (idx, addr) in [second_addr, first_addr].into_iter().enumerate() {
        policy.routes[idx].config.base_url = format!("http://{addr}/v1");
        policy.config.routes[idx].base_url = format!("http://{addr}/v1");
    }
    let runtime = make_runtime(policy, None).unwrap();
    for observation in runtime.observations.lock().unwrap().iter_mut() {
        observation.status = RouteStatus::Serving;
        observation.checked = Some(Instant::now());
        observation.qualified = Some(Instant::now());
    }
    let credential = runtime.auth.read().unwrap().credentials["operator"].clone();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let context = crate::audit::context(Instant::now(), "chat");
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&runtime);
            crate::audit::category(&credential.class);
            let (idx, permit, body) = forward_validated(
                &runtime,
                &credential,
                "qwen38",
                &runtime.policy.config.aliases["qwen38"],
                &json!({"model":"qwen38"}),
                false,
                10,
                RequestCapabilities::default(),
                None,
                Instant::now(),
                ResponseContract::Chat,
            )
            .await
            .unwrap();
            assert_eq!(idx, 0);
            assert!(matches!(body, ValidatedBody::Json(_)));
            drop(permit);
            crate::audit::terminal(&context, 200, "request body completed");
        })
        .with_subscriber(subscriber)
        .await;
    failure.abort();
    success.abort();
    let events = capture.events();
    let attempts: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "upstream attempt finished")
        .collect();
    assert_eq!(attempts.len(), 2, "missing attempt timeline");
    assert_eq!(attempts[0]["fields"]["attempt"], 1);
    assert_eq!(attempts[0]["fields"]["http_status"], 502);
    assert_eq!(attempts[1]["fields"]["attempt"], 2);
    assert_eq!(attempts[1]["fields"]["outcome"], "validated");
    let terminal = &events.last().unwrap()["fields"];
    assert_eq!(terminal["outcome"], "recovered_after_retry");
    assert_eq!(terminal["total_tokens"], 10);
    let metrics = runtime.audit_metrics.render();
    assert!(metrics.contains("outcome=\"recovered_after_retry\"} 1"));
    assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE"));
}

#[tokio::test]
async fn audit_failed_after_retry_not_success_or_overload() {
    use tracing::instrument::WithSubscriber;
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let context = crate::audit::context(Instant::now(), "chat");
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&runtime);
            crate::audit::category("test");
            record_attempt(&runtime, 0);
            let mut first = crate::audit::Attempt::new(&runtime, 0);
            first.finish("transport", None);
            record_attempt(&runtime, 1);
            let mut second = crate::audit::Attempt::new(&runtime, 1);
            second.finish("transport", None);
            crate::audit::terminal(&context, 502, "request body completed");
        })
        .with_subscriber(subscriber)
        .await;
    let events = capture.events();
    let terminal = &events.last().unwrap()["fields"];
    assert_eq!(terminal["outcome"], "failed_after_retry");
    assert_eq!(terminal["retries"], 1);
}

#[tokio::test]
async fn audit_request_generation_and_metrics_survive_policy_reload() {
    let (_dir, app) = capacity_test_app();
    let old = app.current();
    let mut policy = config::load(&app.path).unwrap();
    policy.config.affinity_lease_seconds += 1;
    let new = make_runtime(policy, Some(&old)).unwrap();
    assert!(Arc::ptr_eq(&old.audit_metrics, &new.audit_metrics));
    assert_ne!(old.policy_digest, new.policy_digest);
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let context = crate::audit::context(Instant::now(), "responses");
    use tracing::instrument::WithSubscriber;
    crate::audit::CURRENT
        .scope(context.clone(), async {
            crate::audit::bind(&old);
            crate::audit::bind(&new);
            crate::audit::terminal(&context, 200, "request body cancelled");
        })
        .with_subscriber(subscriber)
        .await;
    let events = capture.events();
    let terminal = &events.last().unwrap()["fields"];
    assert_eq!(terminal["policy_sha256"], old.policy_digest);
    assert_eq!(terminal["policy_generation"], old.policy_generation);
    assert!(
        new.audit_metrics
            .render()
            .contains("outcome=\"cancelled\"} 1")
    );
}

#[tokio::test]
#[ignore = "synthetic overhead exercise; run explicitly in release mode"]
async fn audit_inprocess_release_overhead_measurement() {
    use tower::ServiceExt;
    use tracing::instrument::WithSubscriber;
    const SAMPLES: usize = 7;
    const REQUESTS: usize = 10000;
    let (_dir, app) = capacity_test_app();
    let runtime = app.current();
    let mut modes = Vec::new();
    for mode in [
        "no_middleware_disabled",
        "audit_disabled",
        "audit_enabled_null",
        "audit_enabled_async_null",
    ] {
        let subscriber: Box<dyn tracing::Subscriber + Send + Sync> = match mode {
            "audit_enabled_null" => Box::new(
                tracing_subscriber::fmt()
                    .json()
                    .event_format(crate::audit_sink::formatter())
                    .with_writer(std::io::sink)
                    .finish(),
            ),
            "audit_enabled_async_null" => Box::new(
                tracing_subscriber::fmt()
                    .json()
                    .event_format(crate::audit_sink::formatter())
                    .with_writer(crate::audit_sink::Sink::start(std::io::sink()).unwrap())
                    .finish(),
            ),
            _ => Box::new(tracing::subscriber::NoSubscriber::default()),
        };
        let audit_runtime = runtime.clone();
        let mut router = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let runtime = audit_runtime.clone();
                async move {
                    crate::audit::bind(&runtime);
                    crate::audit::category("test");
                    let (idx, permit) = select_test_route(&runtime, None).unwrap();
                    let mut attempt = crate::audit::Attempt::new(&runtime, idx);
                    attempt.finish("validated", Some(200));
                    crate::audit::backend("complete");
                    drop(permit);
                    "ok"
                }
            }),
        );
        if mode != "no_middleware_disabled" {
            router = router.layer(axum::middleware::from_fn(crate::request_logging::correlate));
        }
        let samples = async {
            let mut samples = Vec::new();
            for sample in 0..=SAMPLES {
                let requests = if sample == 0 { 1000 } else { REQUESTS };
                let started = Instant::now();
                for _ in 0..requests {
                    let request = axum::http::Request::builder()
                        .method("POST")
                        .uri("/v1/chat/completions")
                        .body(Body::empty())
                        .unwrap();
                    let response = router.clone().oneshot(request).await.unwrap();
                    assert_eq!(
                        axum::body::to_bytes(response.into_body(), 1024)
                            .await
                            .unwrap(),
                        "ok"
                    );
                }
                if sample > 0 {
                    samples.push(started.elapsed().as_nanos() as f64 / requests as f64);
                }
            }
            samples
        }
        .with_subscriber(subscriber)
        .await;
        modes.push(json!({"mode":mode,"ns_per_request_samples":samples}));
    }
    println!(
        "AUDIT_REQUEST_BENCHMARK={}",
        json!({"requests_per_sample":REQUESTS,"warmup_requests":1000,"samples_per_mode":SAMPLES,"modes":modes,"build_source_sha256":crate::audit_identity::BUILD,"sink":crate::audit_sink::COUNTERS.snapshot(),"boundary":"synthetic in-process admission + valid response lifecycle; no socket/inference/journal; no historical source comparison"})
    );
}
