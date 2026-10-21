use super::*;

// Isolated HTTP fixture: discovery/readiness and ordinary requests remain healthy,
// but a shared-engine delay makes the one-token qualification response slow.
async fn loaded_probe_route(
    delay: Arc<AtomicU64>,
    load_started: Arc<tokio::sync::Notify>,
) -> MockRoute {
    let engine = Arc::new(tokio::sync::Semaphore::new(1));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let router = Router::new()
        .route(
            "/v1/models",
            get(|| async { axum::Json(json!({"data":[{"id":"fixture"}]})) }),
        )
        .route("/health", get(|| async { StatusCode::OK }))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let delay = delay.clone();
                let counter = counter.clone();
                let engine = engine.clone();
                let load_started = load_started.clone();
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    let _engine_slot = engine.acquire().await.unwrap();
                    if body["fixture_load"] == true {
                        load_started.notify_one();
                        tokio::time::sleep(Duration::from_millis(delay.load(Ordering::Relaxed)))
                            .await;
                    }
                    axum::Json(json!({"model":"fixture","choices":[{"message":{"content":"ok"}}]}))
                }
            }),
        );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    MockRoute {
        address,
        task,
        calls,
    }
}

#[tokio::test]
async fn qualification_load_recovers_after_one_bounded_fast_reprobe() {
    let delay = Arc::new(AtomicU64::new(250));
    let load_started = Arc::new(tokio::sync::Notify::new());
    let routes = vec![loaded_probe_route(delay.clone(), load_started.clone()).await];
    let (_dir, mut runtime, headers) = fixture(&routes, "chat");
    // Scale only the test probe deadline; production uses the same reqwest path
    // with 15 seconds. Do not send a slow request to a production backend.
    Arc::get_mut(&mut runtime).unwrap().probes = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    runtime.observations.lock().unwrap()[0].served = None;
    // A successful synthetic inference outside Slotline occupies the fixture's
    // shared engine. The qualification request queues behind healthy work.
    let backend_url = format!("http://{}/v1/chat/completions", routes[0].address);
    let load = tokio::spawn(async move {
        reqwest::Client::new()
            .post(backend_url)
            .json(&json!({"model":"fixture","max_tokens":2,"fixture_load":true}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(1), load_started.notified())
        .await
        .unwrap();
    let refresh = tokio::spawn(refresh_one(runtime.clone(), 0));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if runtime.observations.lock().unwrap()[0].status == RouteStatus::Serving {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    refresh.await.unwrap();
    {
        let observations = runtime.observations.lock().unwrap();
        assert!(observations[0].model_present);
        assert!(observations[0].qualified.is_some());
        assert_eq!(observations[0].status, RouteStatus::Serving);
        assert_eq!(
            observations[0].last_failed_probe.as_ref().unwrap().outcome,
            "timeout"
        );
    }
    // Backend readiness still succeeds independently while Slotline refuses work.
    assert!(
        reqwest::get(format!("http://{}/health", routes[0].address))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        invoke(runtime.clone(), headers.clone(), "chat", json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 4);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    let completed_load = load.await.unwrap();
    assert_eq!(completed_load.status(), StatusCode::OK);
    assert_eq!(
        completed_load.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "ok"
    );
    assert!(runtime.observations.lock().unwrap()[0].qualified.is_some());
    delay.store(0, Ordering::Relaxed);
    refresh_one(runtime.clone(), 0).await;
    assert_eq!(
        runtime.observations.lock().unwrap()[0].status,
        RouteStatus::Serving
    );
    let response = invoke(runtime.clone(), headers, "chat", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
}

#[tokio::test]
async fn qualification_shared_physical_capacity_skips_busy_probe_without_client_accounting() {
    let delay = Arc::new(AtomicU64::new(0));
    let physical = loaded_probe_route(delay, Arc::new(tokio::sync::Notify::new())).await;
    let routes = vec![physical];
    let address = routes[0].address;
    let route = |name: &str, physical: Option<&str>| {
        let mut value = json!({"name":name,"kind":"local","base_url":format!("http://{address}/v1"),"model":"fixture","safe_concurrency":1,"context_limit":2000,"adapter_version":"mock-v1","capabilities":["text","responses"],"streaming":true,"timeout_seconds":2,"enabled":true});
        if let Some(physical) = physical {
            value["physical_route"] = json!(physical);
            value["kind"] = json!("headroom");
            value["health_path"] = json!("/healthz");
        }
        value
    };
    let policy = json!({"routes":[route("route-0",None),route("proxy",Some("route-0"))],"aliases":{"test":{"endpoint":"chat","context_limit":2000,"capabilities":["text","responses"],"streaming":true,"routes":[{"route":"route-0","priority":0},{"route":"proxy","priority":1}]}}});
    let (_dir, runtime, _) = fixture_with_policy(&routes, "chat", policy);
    assert!(Arc::ptr_eq(&runtime.semaphores[0], &runtime.semaphores[1]));
    let held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    assert!(runtime.semaphores[1].clone().try_acquire_owned().is_err());
    runtime.observations.lock().unwrap()[1].served = None;
    let (attempts_before, _) = traffic_counts(&runtime);
    refresh_one(runtime.clone(), 1).await;
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        runtime.observations.lock().unwrap()[1]
            .last_probe
            .as_ref()
            .unwrap()
            .stage,
        "serving"
    );
    assert_eq!(
        runtime.observations.lock().unwrap()[1]
            .last_probe
            .unwrap()
            .outcome,
        "skipped_busy"
    );
    assert!(Arc::ptr_eq(
        &runtime.qualification_locks[0],
        &runtime.qualification_locks[1]
    ));
    assert_eq!(traffic_counts(&runtime).0, attempts_before);
    assert_eq!(runtime.route_completions[1].load(Ordering::Relaxed), 0);
    drop(held);
    refresh_one(runtime.clone(), 1).await;
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 1);
    assert_eq!(traffic_counts(&runtime).0, attempts_before);
    assert_eq!(runtime.route_completions[1].load(Ordering::Relaxed), 0);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(runtime.semaphores[1].available_permits(), 1);
    assert_eq!(
        runtime.observations.lock().unwrap()[1].status,
        RouteStatus::Serving
    );
}

async fn delayed_qualification_route(
    delay: Duration,
    started: Arc<tokio::sync::Notify>,
) -> MockRoute {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let count = calls.clone();
    let router = Router::new()
        .route(
            "/v1/models",
            get(|| async { axum::Json(json!({"data":[{"id":"fixture"}]})) }),
        )
        .route("/health", get(|| async { StatusCode::OK }))
        .route(
            "/v1/chat/completions",
            post(move || {
                let started = started.clone();
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    started.notify_one();
                    tokio::time::sleep(delay).await;
                    axum::Json(json!({"model":"fixture","choices":[{"message":{"content":"ok"}}]}))
                }
            }),
        );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    MockRoute {
        address,
        task,
        calls,
    }
}

#[tokio::test]
async fn qualification_sustained_timeout_stops_after_two_attempts() {
    let routes = vec![
        delayed_qualification_route(
            Duration::from_millis(250),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await,
    ];
    let (_dir, mut runtime, _) = fixture(&routes, "chat");
    Arc::get_mut(&mut runtime).unwrap().probes = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    runtime.observations.lock().unwrap()[0].served = None;
    tokio::time::timeout(Duration::from_secs(4), refresh_one(runtime.clone(), 0))
        .await
        .unwrap();
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 2);
    let observations = runtime.observations.lock().unwrap();
    assert!(observations[0].qualified.is_none());
    assert_eq!(observations[0].status, RouteStatus::Stale);
    drop(observations);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert!(runtime.qualification_locks[0].try_lock().is_ok());
    assert_eq!(traffic_counts(&runtime).0, vec![0]);
}

#[tokio::test]
async fn qualification_late_success_is_fenced_after_failure_and_ordinary_success() {
    let started = Arc::new(tokio::sync::Notify::new());
    let routes =
        vec![delayed_qualification_route(Duration::from_millis(200), started.clone()).await];
    let (_dir, runtime, _) = fixture(&routes, "chat");
    runtime.observations.lock().unwrap()[0].served = None;
    let refresh = tokio::spawn(refresh_one(runtime.clone(), 0));
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    mark_failure(&runtime, 0);
    mark_success(&runtime, 0);
    refresh.await.unwrap();
    let observations = runtime.observations.lock().unwrap();
    assert!(observations[0].qualified.is_none());
    assert_eq!(observations[0].status, RouteStatus::Ready);
    assert_eq!(observations[0].failures, 0);
    drop(observations);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
}

#[tokio::test]
async fn qualification_admission_busy_is_observable_without_failure_accounting() {
    let routes = vec![
        loaded_probe_route(
            Arc::new(AtomicU64::new(0)),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await,
    ];
    let (_dir, runtime, _) = fixture(&routes, "chat");
    record_probe(&runtime, 0, "serving", "timeout", None, Instant::now());
    let prior = runtime.observations.lock().unwrap()[0]
        .last_failed_probe
        .unwrap();
    runtime.observations.lock().unwrap()[0].served = None;
    let held = runtime.semaphores[0].clone().try_acquire_owned().unwrap();
    refresh_one(runtime.clone(), 0).await;
    let observations = runtime.observations.lock().unwrap();
    let probe = observations[0].last_probe.unwrap();
    assert_eq!(probe.stage, "serving");
    assert_eq!(probe.outcome, "skipped_busy");
    assert_eq!(observations[0].last_failed_probe.unwrap().at, prior.at);
    drop(observations);
    drop(held);
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 0);
    assert_eq!(runtime.route_failure_events[0].load(Ordering::Relaxed), 0);
    assert_eq!(traffic_counts(&runtime).0, vec![0]);
}

#[tokio::test]
async fn qualification_probe_lock_busy_preserves_last_failure_and_admission() {
    let routes = vec![
        loaded_probe_route(
            Arc::new(AtomicU64::new(0)),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await,
    ];
    let (_dir, runtime, _) = fixture(&routes, "chat");
    record_probe(&runtime, 0, "serving", "timeout", None, Instant::now());
    let prior = runtime.observations.lock().unwrap()[0]
        .last_failed_probe
        .unwrap();
    runtime.observations.lock().unwrap()[0].served = None;
    let held = runtime.qualification_locks[0].lock().await;
    refresh_one(runtime.clone(), 0).await;
    let observations = runtime.observations.lock().unwrap();
    assert_eq!(observations[0].last_probe.unwrap().outcome, "skipped_busy");
    assert_eq!(observations[0].last_failed_probe.unwrap().at, prior.at);
    drop(observations);
    drop(held);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn qualification_body_deadline_gets_one_fast_reprobe() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let count = calls.clone();
    let router = Router::new()
        .route("/v1/models", get(|| async { axum::Json(json!({"data":[{"id":"fixture"}]})) }))
        .route("/health", get(|| async { StatusCode::OK }))
        .route("/v1/chat/completions", post(move || {
            count.fetch_add(1, Ordering::Relaxed);
            async move { Body::from_stream(async_stream::stream! {
                yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"{"));
                tokio::time::sleep(Duration::from_millis(250)).await;
                yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(br#""model":"fixture","choices":[{"message":{"content":"ok"}}]}"#));
            }) }
        }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let routes = vec![MockRoute {
        address,
        task,
        calls,
    }];
    let (_dir, mut runtime, _) = fixture(&routes, "chat");
    Arc::get_mut(&mut runtime).unwrap().probes = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    runtime.observations.lock().unwrap()[0].served = None;
    tokio::time::timeout(Duration::from_secs(4), refresh_one(runtime.clone(), 0))
        .await
        .unwrap();
    assert_eq!(routes[0].calls.load(Ordering::Relaxed), 2);
    let observations = runtime.observations.lock().unwrap();
    let failure = observations[0].last_failed_probe.unwrap();
    assert_eq!(failure.outcome, "timeout");
    assert_eq!(failure.http_status, Some(200));
    assert!(observations[0].qualified.is_none());
    drop(observations);
    assert_eq!(runtime.semaphores[0].available_permits(), 1);
}
