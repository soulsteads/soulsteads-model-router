use super::*;

pub(super) fn record_probe(
    runtime: &Runtime,
    idx: usize,
    stage: &'static str,
    outcome: &'static str,
    http_status: Option<u16>,
    started: Instant,
) {
    if outcome == "skipped_busy" {
        let diagnostic = ProbeDiagnostic {
            stage,
            outcome,
            http_status,
            latency_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            at: Instant::now(),
        };
        runtime.observations.lock().unwrap()[idx].last_probe = Some(diagnostic);
        return;
    }
    let diagnostic = ProbeDiagnostic {
        stage,
        outcome,
        http_status,
        latency_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        at: Instant::now(),
    };
    let mut observations = runtime.observations.lock().unwrap();
    observations[idx].last_probe = Some(diagnostic);
    if outcome != "success" {
        observations[idx].last_failed_probe = Some(diagnostic);
        drop(observations);
        warn!(route=%runtime.policy.routes[idx].config.name, stage, outcome, ?http_status,
            latency_ms=diagnostic.latency_ms, "route probe failed");
    }
}

pub(super) fn mark_failure(runtime: &Runtime, idx: usize) {
    let mut observations = runtime.observations.lock().unwrap();
    let obs = &mut observations[idx];
    obs.invalidation_epoch = obs.invalidation_epoch.wrapping_add(1);
    let prior = obs.status;
    obs.failures = obs.failures.saturating_add(1);
    obs.served = None;
    if obs.qualified.is_some() {
        obs.last_successful_qualification = obs.qualified;
    }
    obs.qualified = None;
    let seconds = 2u64.saturating_pow(obs.failures.min(6)).min(60);
    obs.cooldown_until = Some(Instant::now() + Duration::from_secs(seconds));
    obs.status = RouteStatus::CoolingDown;
    drop(observations);
    runtime.route_failure_events[idx].fetch_add(1, Ordering::Relaxed);
    if prior != RouteStatus::CoolingDown {
        warn!(route=%runtime.policy.routes[idx].config.name,from=prior.name(),to="cooling_down",cooldown_seconds=seconds,"route health changed");
    }
}
pub(super) fn mark_success(runtime: &Runtime, idx: usize) {
    let mut observations = runtime.observations.lock().unwrap();
    let obs = &mut observations[idx];
    let prior = obs.status;
    obs.failures = 0;
    obs.cooldown_until = None;
    obs.served = Some(Instant::now());
    let qualified = obs
        .qualified
        .is_some_and(|at| at.elapsed() <= Duration::from_secs(900));
    obs.status = if qualified {
        RouteStatus::Serving
    } else {
        RouteStatus::Ready
    };
    let next = obs.status;
    drop(observations);
    if prior != next {
        info!(route=%runtime.policy.routes[idx].config.name,from=prior.name(),to=next.name(),reason="successful_serving_evidence","route health changed");
    }
}

pub(super) async fn refresh_one(runtime: Arc<Runtime>, idx: usize) {
    if refresh_once(runtime.clone(), idx).await {
        tokio::time::sleep(Duration::from_secs(2)).await;
        refresh_once(runtime, idx).await;
    }
}

async fn refresh_once(runtime: Arc<Runtime>, idx: usize) -> bool {
    let route = &runtime.policy.routes[idx].config;
    if !route.enabled {
        return false;
    }
    {
        let observations = runtime.observations.lock().unwrap();
        if observations[idx]
            .cooldown_until
            .is_some_and(|until| Instant::now() < until)
        {
            return false;
        }
    }
    let base = &route.base_url;
    let mut models_request = runtime.probes.get(format!("{base}/models"));
    if let Some(key) = &runtime.policy.routes[idx].key {
        models_request = models_request.bearer_auth(String::from_utf8_lossy(key).as_ref());
    }
    let started = Instant::now();
    let (models, outcome, http_status) = match models_request.send().await {
        Ok(r) => {
            let code = r.status().as_u16();
            if !r.status().is_success() {
                (None, "http_error", Some(code))
            } else {
                match r.json::<Value>().await {
                    Ok(body) => (Some(body), "success", Some(code)),
                    Err(_) => (None, "invalid_response", Some(code)),
                }
            }
        }
        Err(e) => (
            None,
            if e.is_timeout() {
                "timeout"
            } else {
                "transport_error"
            },
            None,
        ),
    };
    let catalog_valid = models
        .as_ref()
        .and_then(|v| v.get("data"))
        .and_then(Value::as_array)
        .is_some();
    let present = models
        .as_ref()
        .and_then(|v| v.get("data"))
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|m| m.get("id").and_then(Value::as_str) == Some(route.model.as_str()))
        });
    let prior_status = {
        let mut observations = runtime.observations.lock().unwrap();
        let obs = &mut observations[idx];
        let prior = obs.status;
        obs.checked = Some(Instant::now());
        obs.model_present = present;
        if !present {
            obs.served = None;
            obs.qualified = None;
            obs.invalidation_epoch = obs.invalidation_epoch.wrapping_add(1);
        }
        prior
    };
    if !present {
        record_probe(
            &runtime,
            idx,
            "discovery",
            if catalog_valid {
                "expected_model_missing"
            } else if models.is_some() {
                "invalid_response"
            } else {
                outcome
            },
            http_status,
            started,
        );
        set_status(
            &runtime,
            idx,
            if models.is_none() {
                RouteStatus::Unreachable
            } else {
                RouteStatus::Stale
            },
            if models.is_none() {
                "discovery_failed"
            } else {
                "expected_model_missing"
            },
        );
        return false;
    }
    record_probe(&runtime, idx, "discovery", "success", http_status, started);
    if prior_status != RouteStatus::Serving {
        set_status(
            &runtime,
            idx,
            RouteStatus::Advertised,
            "expected_model_present",
        )
    }
    let health_url = format!("{}{}", base.trim_end_matches("/v1"), route.health_path);
    let started = Instant::now();
    let (healthy, outcome, http_status) = match runtime.probes.get(health_url).send().await {
        Ok(r) => (
            r.status().is_success(),
            if r.status().is_success() {
                "success"
            } else {
                "http_error"
            },
            Some(r.status().as_u16()),
        ),
        Err(e) => (
            false,
            if e.is_timeout() {
                "timeout"
            } else {
                "transport_error"
            },
            None,
        ),
    };
    record_probe(&runtime, idx, "readiness", outcome, http_status, started);
    if !healthy {
        let mut observations = runtime.observations.lock().unwrap();
        observations[idx].served = None;
        observations[idx].qualified = None;
        observations[idx].invalidation_epoch = observations[idx].invalidation_epoch.wrapping_add(1);
        drop(observations);
        set_status(&runtime, idx, RouteStatus::Reachable, "readiness_failed");
        return false;
    }
    let recent = {
        let observations = runtime.observations.lock().unwrap();
        observations[idx]
            .qualified
            .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
            && observations[idx]
                .served
                .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
    };
    if recent {
        set_status(
            &runtime,
            idx,
            RouteStatus::Serving,
            "recent_serving_evidence",
        );
        return false;
    }
    if prior_status != RouteStatus::Serving {
        set_status(&runtime, idx, RouteStatus::Ready, "readiness_passed")
    }
    let Ok(_qualification_guard) = runtime.qualification_locks[idx].clone().try_lock_owned() else {
        record_probe(
            &runtime,
            idx,
            "serving",
            "skipped_busy",
            None,
            Instant::now(),
        );
        return false;
    };
    let epoch = runtime.observations.lock().unwrap()[idx].invalidation_epoch;
    // A bounded fixed text fixture qualifies serving. It is never counted as client traffic.
    let (path, mut fixture) = if route.kind == RouteKind::Embedding {
        ("embeddings", json!({"model":route.model,"input":"ping"}))
    } else {
        (
            "chat/completions",
            json!({"model":route.model,"messages":[{"role":"user","content":"ping"}],"max_tokens":1,"stream":false}),
        )
    };
    let tokenizer = runtime
        .policy
        .config
        .aliases
        .iter()
        .find_map(|(name, alias)| {
            if alias.routes.iter().any(|entry| entry.route == route.name) {
                runtime.policy.tokenizers.get(name)
            } else {
                None
            }
        });
    if route.disable_thinking || tokenizer.is_some() {
        fixture["chat_template_kwargs"] = json!({"enable_thinking":false});
    }
    let expected_prompt = tokenizer
        .and_then(|t| {
            tokenized_prompt(
                fixture["messages"].as_array()?,
                fixture.get("chat_template_kwargs"),
                t,
                false,
            )
        })
        .map(|n| n - 32);
    let Ok(_probe_permit) = runtime.semaphores[idx].clone().try_acquire_owned() else {
        record_probe(
            &runtime,
            idx,
            "serving",
            "skipped_busy",
            None,
            Instant::now(),
        );
        return false;
    };
    let mut probe = runtime.probes.post(format!("{base}/{path}")).json(&fixture);
    if let Some(key) = &runtime.policy.routes[idx].key {
        probe = probe.bearer_auth(String::from_utf8_lossy(key).as_ref());
    }
    let started = Instant::now();
    let (payload, outcome, http_status) = match probe.send().await {
        Ok(response) => {
            let code = response.status().as_u16();
            if !response.status().is_success() {
                (None, "http_error", Some(code))
            } else {
                match response.json::<Value>().await {
                    Ok(body) => (Some(body), "success", Some(code)),
                    Err(e) => (
                        None,
                        if e.is_timeout() {
                            "timeout"
                        } else {
                            "invalid_response"
                        },
                        Some(code),
                    ),
                }
            }
        }
        Err(e) => (
            None,
            if e.is_timeout() {
                "timeout"
            } else {
                "transport_error"
            },
            None,
        ),
    };
    let valid = payload.as_ref().is_some_and(|body| {
        let identity = body.get("model").and_then(Value::as_str) == Some(route.model.as_str());
        if route.kind == RouteKind::Embedding {
            identity
                && body
                    .get("data")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
        } else {
            identity
                && body
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                && expected_prompt.is_none_or(|n| {
                    body.get("usage")
                        .and_then(|u| u.get("prompt_tokens"))
                        .and_then(Value::as_u64)
                        == Some(n)
                })
        }
    });
    if valid {
        // Check the epoch and publish qualification in the same critical section.
        let mut observations = runtime.observations.lock().unwrap();
        if observations[idx].invalidation_epoch != epoch {
            return false;
        }
        let now = Instant::now();
        observations[idx].last_probe = Some(ProbeDiagnostic {
            stage: "serving",
            outcome: "success",
            http_status,
            latency_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            at: now,
        });
        observations[idx].qualified = Some(now);
        observations[idx].last_successful_qualification = Some(now);
        observations[idx].served = Some(now);
        observations[idx].failures = 0;
        observations[idx].cooldown_until = None;
        observations[idx].status = RouteStatus::Serving;
    } else {
        record_probe(
            &runtime,
            idx,
            "serving",
            if payload.is_some() {
                "invalid_response"
            } else {
                outcome
            },
            http_status,
            started,
        );
        let mut observations = runtime.observations.lock().unwrap();
        observations[idx].invalidation_epoch = observations[idx].invalidation_epoch.wrapping_add(1);
        observations[idx].served = None;
        observations[idx].qualified = None;
        drop(observations);
        set_status(&runtime, idx, RouteStatus::Stale, "serving_probe_failed");
        warn!(route=%route.name,"serving qualification failed");
        return outcome == "timeout";
    }
    false
}
pub(super) async fn refresh_loop(app: Arc<AppState>) {
    loop {
        let runtime = app.current();
        let jobs = (0..runtime.policy.routes.len()).map(|idx| {
            let runtime = runtime.clone();
            async move {
                tokio::join!(
                    refresh_one(runtime.clone(), idx),
                    crate::backend_telemetry::refresh(runtime, idx)
                );
            }
        });
        futures_util::future::join_all(jobs).await;
        tokio::time::sleep(Duration::from_secs(runtime.policy.config.refresh_seconds)).await;
    }
}

pub(super) async fn health(State(runtime): State<Arc<Runtime>>, headers: HeaderMap) -> Response {
    let credential = match authorize(&runtime, &headers).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    if !credential.operator {
        return json_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Operator credential required",
        );
    }
    let observations = runtime.observations.lock().unwrap();
    let (attempts, recent) = traffic_counts(&runtime);
    let total: u64 = attempts.iter().sum();
    let recent_total: u64 = recent.iter().sum();
    let routes: HashMap<_,_>=runtime.policy.routes.iter().enumerate().map(|(idx,r)|(&r.config.name,json!({
        "state":effective_route_status(&runtime,idx,&observations).name(),"model_present":observations[idx].model_present,
        "active":r.config.safe_concurrency-runtime.semaphores[idx].available_permits(),"limit":r.config.safe_concurrency,
        "last_checked_age_seconds":observations[idx].checked.map(|t|t.elapsed().as_secs()),
        "last_served_age_seconds":observations[idx].served.map(|t|t.elapsed().as_secs()),
        "last_qualified_age_seconds":observations[idx].qualified.map(|t|t.elapsed().as_secs()),
        "attempts_total":attempts[idx],"completed_total":runtime.route_completions[idx].load(Ordering::Relaxed),
        "traffic_percent_total":percent(attempts[idx],total),"traffic_percent_5m":percent(recent[idx],recent_total)
    }))).collect();
    let mut response = Response::new(Body::from(json!({"routes":routes}).to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}
pub(super) async fn status(State(runtime): State<Arc<Runtime>>, headers: HeaderMap) -> Response {
    let credential = match authorize(&runtime, &headers).await {
        Ok(value) => value,
        Err(error) => return error,
    };
    if !credential.operator {
        return json_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Operator credential required",
        );
    }
    let payload = status_payload(&runtime);
    let mut response = Response::new(Body::from(payload.to_string()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

pub(super) fn status_payload(runtime: &Runtime) -> Value {
    let observations = runtime.observations.lock().unwrap();
    let telemetry = runtime.backend_telemetry.lock().unwrap();
    let (attempts, recent) = traffic_counts(runtime);
    let total: u64 = attempts.iter().sum();
    let recent_total: u64 = recent.iter().sum();
    let routes: HashMap<_, _> = runtime.policy.routes.iter().enumerate().map(|(idx, route)| {
        let cfg = &route.config;
        (cfg.name.clone(), json!({
            "base_url": cfg.base_url, "model": cfg.model, "enabled": cfg.enabled,
            "physical_route": cfg.physical_route,
            "server_metrics": telemetry[physical_route_index(&runtime.policy, idx).unwrap_or(idx)].view_at(Instant::now(), Duration::from_secs(runtime.policy.config.refresh_seconds.saturating_mul(3))),
            "state": effective_route_status(runtime, idx, &observations).name(),
            "model_present": observations[idx].model_present,
            "last_checked_age_seconds": observations[idx].checked.map(|at| at.elapsed().as_secs()),
            "last_qualified_age_seconds": observations[idx].qualified.map(|at| at.elapsed().as_secs()),
            "last_successful_qualification_age_seconds": observations[idx].last_successful_qualification.map(|at| at.elapsed().as_secs()),
            "last_probe": observations[idx].last_probe.map(|probe| json!({
                "stage": probe.stage, "outcome": probe.outcome,
                "http_status": probe.http_status,
                "latency_ms": probe.latency_ms,
                "age_seconds": probe.at.elapsed().as_secs(),
            })),
            "last_failed_probe": observations[idx].last_failed_probe.map(|probe| json!({
                "stage": probe.stage, "outcome": probe.outcome,
                "http_status": probe.http_status,
                "latency_ms": probe.latency_ms,
                "age_seconds": probe.at.elapsed().as_secs(),
            })),
            "active": cfg.safe_concurrency - runtime.semaphores[idx].available_permits(),
            "limit": cfg.safe_concurrency, "attempts_total": attempts[idx],
            "attempts_5m": recent[idx],
            "traffic_percent_total": percent(attempts[idx], total),
            "traffic_percent_5m": percent(recent[idx], recent_total),
        }))
    }).collect();
    drop(observations);
    let auth = runtime.auth.read().unwrap().clone();
    let credentials: HashMap<_, _> = auth
        .credentials
        .values()
        .filter(|c| {
            auth.snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.values().any(|key| key.policy.id == c.name))
        })
        .map(|c| {
            let aliases: HashMap<_, _> = c
                .aliases
                .iter()
                .filter_map(|name| {
                    let alias = runtime.policy.config.aliases.get(name)?;
                    let entries: Vec<_> = alias
                        .routes
                        .iter()
                        .filter(|entry| entry.enabled)
                        .filter(|entry| {
                            c.allowed_routes.is_empty() || c.allowed_routes.contains(&entry.route)
                        })
                        .filter_map(|entry| {
                            let route = runtime
                                .policy
                                .routes
                                .iter()
                                .find(|r| r.config.name == entry.route && r.config.enabled)?;
                            Some(
                                json!({"route": entry.route, "endpoint": route.config.base_url,
                        "priority": entry.priority, "fallback_only": entry.fallback_only}),
                            )
                        })
                        .collect();
                    Some((name.clone(), entries))
                })
                .collect();
            (
                c.name.clone(),
                json!({"class": c.class, "agent_affinity": c.agent_affinity,
            "operator": c.operator, "aliases": aliases}),
            )
        })
        .collect();
    let now = Instant::now();
    let lease_ttl = runtime.policy.config.affinity_lease_seconds;
    let mut leases: Vec<_> = runtime.selection.lock().unwrap().leases.values()
        .filter(|lease| now.duration_since(lease.last_seen).as_secs_f64() < lease_ttl as f64)
        .filter_map(|lease| runtime.policy.routes.get(lease.route_idx).map(|route| json!({
            "route": route.config.name,
            "age_seconds": now.duration_since(lease.created_at).as_secs(),
            "idle_seconds": now.duration_since(lease.last_seen).as_secs(),
            "remaining_seconds": lease_ttl.saturating_sub(now.duration_since(lease.last_seen).as_secs()),
        }))).collect();
    leases.sort_by(|a, b| {
        a["route"]
            .as_str()
            .cmp(&b["route"].as_str())
            .then_with(|| a["age_seconds"].as_u64().cmp(&b["age_seconds"].as_u64()))
    });
    json!({"routes": routes, "credentials": credentials, "leases": leases, "audit":crate::audit_identity::status(runtime),
        "lease_ttl_seconds": lease_ttl, "traffic_window_seconds": 300,
        "requests_total": runtime.requests.load(Ordering::Relaxed),
        "rejects_total": runtime.rejects.load(Ordering::Relaxed),
        "upstream_errors_total": runtime.upstream_errors.load(Ordering::Relaxed)})
}

pub(super) async fn metrics(State(runtime): State<Arc<Runtime>>, headers: HeaderMap) -> Response {
    let credential = match authorize(&runtime, &headers).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    if !credential.operator {
        return json_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Operator credential required",
        );
    }
    let affinity_leases = runtime
        .selection
        .lock()
        .unwrap()
        .leases
        .values()
        .filter(|lease| {
            lease.last_seen.elapsed()
                < Duration::from_secs(runtime.policy.config.affinity_lease_seconds)
        })
        .count();
    let observations = runtime.observations.lock().unwrap();
    let (attempts, recent) = traffic_counts(&runtime);
    let total: u64 = attempts.iter().sum();
    let recent_total: u64 = recent.iter().sum();
    let mut lines = format!(
        "slotline_requests_total {}\nslotline_rejects_total {}\nslotline_upstream_errors_total {}\nslotline_affinity_leases {}\nslotline_affinity_hits_total {}\nslotline_affinity_rebinds_total {}\nslotline_affinity_expired_total {}\n",
        runtime.requests.load(Ordering::Relaxed),
        runtime.rejects.load(Ordering::Relaxed),
        runtime.upstream_errors.load(Ordering::Relaxed),
        affinity_leases,
        runtime.affinity_hits.load(Ordering::Relaxed),
        runtime.affinity_rebinds.load(Ordering::Relaxed),
        runtime.affinity_expired.load(Ordering::Relaxed)
    );
    lines.push_str(&runtime.audit_metrics.render());
    for (idx, route) in runtime.policy.routes.iter().enumerate() {
        let name = &route.config.name;
        let obs = &observations[idx];
        let cap = route.config.safe_concurrency;
        let free = runtime.semaphores[idx].available_permits();
        lines.push_str(&format!("slotline_route_active{{route=\"{name}\"}} {}\nslotline_route_capacity{{route=\"{name}\"}} {cap}\nslotline_route_free{{route=\"{name}\"}} {free}\nslotline_route_state{{route=\"{name}\",state=\"{}\"}} 1\nslotline_route_model_present{{route=\"{name}\"}} {}\n",cap-free,effective_route_status(&runtime,idx,&observations).name(),u8::from(obs.model_present)));
        lines.push_str(&format!("slotline_route_attempts_total{{route=\"{name}\"}} {}\nslotline_route_completions_total{{route=\"{name}\"}} {}\nslotline_route_failure_events_total{{route=\"{name}\"}} {}\nslotline_route_traffic_percent{{route=\"{name}\",window=\"total\"}} {:.4}\nslotline_route_traffic_percent{{route=\"{name}\",window=\"5m\"}} {:.4}\n",attempts[idx],runtime.route_completions[idx].load(Ordering::Relaxed),runtime.route_failure_events[idx].load(Ordering::Relaxed),percent(attempts[idx],total),percent(recent[idx],recent_total)));
        if let Some(at) = obs.checked {
            lines.push_str(&format!(
                "slotline_route_last_checked_age_seconds{{route=\"{name}\"}} {}\n",
                at.elapsed().as_secs()
            ));
        }
        if let Some(at) = obs.qualified {
            lines.push_str(&format!(
                "slotline_route_last_qualified_age_seconds{{route=\"{name}\"}} {}\n",
                at.elapsed().as_secs()
            ));
        }
    }
    for ((idx, reason), count) in runtime.route_skips.lock().unwrap().iter() {
        // The backend gauges below are not admission or routing inputs.
        let name = &runtime.policy.routes[*idx].config.name;
        lines.push_str(&format!(
            "slotline_route_skips_total{{route=\"{name}\",reason=\"{reason}\"}} {count}\n"
        ));
    }
    let telemetry = runtime.backend_telemetry.lock().unwrap();
    for (idx, route) in runtime.policy.routes.iter().enumerate() {
        let name = &route.config.name;
        let m = telemetry[idx].view_at(
            Instant::now(),
            Duration::from_secs(runtime.policy.config.refresh_seconds.saturating_mul(3)),
        );
        lines.push_str(&format!(
            "slotline_backend_metrics_available{{route=\"{name}\"}} {}\n",
            u8::from(m["status"] == "available")
        ));
        lines.push_str(&format!(
            "slotline_backend_acceptance_supported{{route=\"{name}\"}} {}\n",
            u8::from(m["acceptance_supported"] == true)
        ));
        for (field, metric) in [
            (
                "generation_tokens_per_second",
                "generation_tokens_per_second",
            ),
            ("prompt_tokens_per_second", "prompt_tokens_per_second"),
            ("acceptance_ratio", "acceptance_ratio"),
            ("age_seconds", "sample_age_seconds"),
            ("window_seconds", "window_seconds"),
            (
                "reported_generation_tokens_per_second",
                "reported_generation_tokens_per_second",
            ),
            (
                "reported_prompt_tokens_per_second",
                "reported_prompt_tokens_per_second",
            ),
            ("requests_running", "requests_running"),
            ("requests_waiting", "requests_waiting"),
            ("kv_cache_tokens", "kv_cache_tokens"),
            ("kv_cache_usage_ratio", "kv_cache_usage_ratio"),
            ("slots_total", "slots_total"),
        ] {
            if let Some(value) = m[field].as_f64() {
                lines.push_str(&format!(
                    "slotline_backend_{metric}{{route=\"{name}\"}} {value}\n"
                ));
            }
        }
    }
    let mut response = Response::new(Body::from(lines));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

pub(super) fn make_runtime(
    policy: Policy,
    previous: Option<&Arc<Runtime>>,
) -> Result<Arc<Runtime>, String> {
    let route_count = policy.routes.len();
    let policy_digest = crate::audit_identity::digest(&policy.config);
    if let Some(old) = previous {
        let auth = if let Some(snapshot) = policy.key_snapshot.as_ref() {
            let previous_auth = old.auth.read().unwrap().clone();
            Arc::new(RwLock::new(replaced_auth(
                &previous_auth,
                policy.config.credentials.clone(),
                snapshot.clone(),
            )))
        } else {
            old.auth.clone()
        };
        return Ok(Arc::new(Runtime {
            policy_digest,
            policy_generation: old.policy_generation + 1,
            audit_metrics: old.audit_metrics.clone(),
            policy,
            auth,
            auth_limiter: old.auth_limiter.clone(),
            client: old.client.clone(),
            probes: old.probes.clone(),
            codex_semaphore: old.codex_semaphore.clone(),
            codex_client: old.codex_client.clone(),
            semaphores: old.semaphores.clone(),
            qualification_locks: old.qualification_locks.clone(),
            observations: old.observations.clone(),
            backend_telemetry: old.backend_telemetry.clone(),
            selection: old.selection.clone(),
            requests: old.requests.clone(),
            rejects: old.rejects.clone(),
            upstream_errors: old.upstream_errors.clone(),
            route_attempts: old.route_attempts.clone(),
            route_completions: old.route_completions.clone(),
            route_failure_events: old.route_failure_events.clone(),
            route_skips: old.route_skips.clone(),
            recent_routes: old.recent_routes.clone(),
            affinity_hits: old.affinity_hits.clone(),
            affinity_rebinds: old.affinity_rebinds.clone(),
            affinity_expired: old.affinity_expired.clone(),
        }));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;
    let probes = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let codex_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|_| "Codex transport initialization failed".to_string())?;
    let now = Instant::now();
    let auth = initial_auth(&policy);
    let physical_semaphores: Vec<_> = policy
        .routes
        .iter()
        .map(|route| Arc::new(Semaphore::new(route.config.safe_concurrency)))
        .collect();
    let semaphores = (0..route_count)
        .map(|idx| {
            physical_route_index(&policy, idx)
                .map(|physical_idx| physical_semaphores[physical_idx].clone())
                .unwrap_or_else(|| physical_semaphores[idx].clone())
        })
        .collect();
    let physical_locks: Vec<_> = (0..route_count)
        .map(|_| Arc::new(tokio::sync::Mutex::new(())))
        .collect();
    let qualification_locks = (0..route_count)
        .map(|idx| {
            physical_route_index(&policy, idx)
                .map(|physical_idx| physical_locks[physical_idx].clone())
                .unwrap_or_else(|| physical_locks[idx].clone())
        })
        .collect();
    Ok(Arc::new(Runtime {
        policy_digest,
        policy_generation: 1,
        audit_metrics: Arc::new(crate::audit::Metrics::default()),
        codex_semaphore: Arc::new(Semaphore::new(8)),
        codex_client,
        semaphores,
        qualification_locks,
        auth: Arc::new(RwLock::new(auth)),
        auth_limiter: Arc::new(Semaphore::new(8)),
        observations: Arc::new(Mutex::new(
            policy
                .routes
                .iter()
                .map(|_| RouteObservation {
                    status: RouteStatus::Unknown,
                    checked: None,
                    served: None,
                    qualified: None,
                    last_successful_qualification: None,
                    cooldown_until: None,
                    failures: 0,
                    model_present: false,
                    last_probe: None,
                    last_failed_probe: None,
                    invalidation_epoch: 0,
                })
                .collect(),
        )),
        backend_telemetry: Arc::new(Mutex::new(
            (0..route_count)
                .map(|_| crate::backend_telemetry::Track::default())
                .collect(),
        )),
        selection: Arc::new(Mutex::new(SelectionState {
            sequence: 0,
            last_selected: vec![0; route_count],
            leases: HashMap::new(),
            last_prune: now,
        })),
        policy,
        client,
        probes,
        requests: Arc::new(AtomicU64::new(0)),
        rejects: Arc::new(AtomicU64::new(0)),
        upstream_errors: Arc::new(AtomicU64::new(0)),
        route_attempts: (0..route_count)
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect(),
        route_completions: (0..route_count)
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect(),
        route_failure_events: (0..route_count)
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect(),
        route_skips: Arc::new(Mutex::new(HashMap::new())),
        recent_routes: Arc::new(Mutex::new(VecDeque::new())),
        affinity_hits: Arc::new(AtomicU64::new(0)),
        affinity_rebinds: Arc::new(AtomicU64::new(0)),
        affinity_expired: Arc::new(AtomicU64::new(0)),
    }))
}
