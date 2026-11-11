use super::*;
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use serde_json::json;

fn fixture(verifier: bool) -> (tempfile::TempDir, Arc<Runtime>, HeaderMap) {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("workload.key");
    let token = if verifier {
        format!("slk_{}_{}", "a".repeat(24), "b".repeat(64))
    } else {
        "synthetic-copilot-workload-token".into()
    };
    fs::write(&key, &token).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let path = directory.path().join("policy.json");
    let config = json!({
        "bind":"100.127.255.254", "port":4000, "routes":[], "aliases":{},
        "credentials":[{"name":"workload", "key_file":key,
            "aliases":["native-chat", "native-responses"], "class":"test",
            "requests_per_minute":1, "burst":2, "max_in_flight":1,
            "cloud_allowed":true, "allowed_routes":["copilot"]}],
        "copilot":{"enabled":true, "auth_source":"oauth_token_file",
            "oauth_token_file":directory.path().join("absent-synthetic-oauth"),
            "models":[
                {"alias":"native-chat", "model":"synthetic-chat", "endpoint":"chat_completions"},
                {"alias":"native-responses", "model":"synthetic-responses", "endpoint":"responses"}
            ]}
    });
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut policy = config::load(&path).unwrap();
    if verifier {
        let credential = &policy.config.credentials[0];
        let key_policy = key_manager::KeyPolicy {
            id: credential.name.clone(),
            aliases: credential.aliases.clone(),
            class: credential.class.clone(),
            requests_per_minute: credential.requests_per_minute,
            burst: credential.burst,
            max_in_flight: credential.max_in_flight,
            operator: false,
            agent_affinity: false,
            allowed_routes: credential.allowed_routes.clone(),
            cloud_allowed: true,
            expires_at: None,
        };
        let salt = SaltString::encode_b64(b"synthetic-test-salt").unwrap();
        let hash = Argon2::default()
            .hash_password("b".repeat(64).as_bytes(), &salt)
            .unwrap()
            .to_string();
        policy.key_snapshot = Some(Arc::new(HashMap::from([(
            "a".repeat(24),
            key_manager::SnapshotKey {
                verifier: hash,
                policy: key_policy,
                expires_at: None,
            },
        )])));
        policy.config.key_store = Some(
            directory
                .path()
                .join("unused-synthetic-store")
                .display()
                .to_string(),
        );
    }
    let runtime = make_runtime(policy, None).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    (directory, runtime, headers)
}

async fn invoke(
    runtime: Arc<Runtime>,
    headers: &HeaderMap,
    alias: &str,
    endpoint: &str,
) -> Response {
    let mut request = Request::new(Body::from(json!({"model":alias}).to_string()));
    *request.headers_mut() = headers.clone();
    dispatch(runtime, request, endpoint).await
}

#[tokio::test]
async fn copilot_dispatch_depleted_bucket_blocks_auth_for_both_protocols_and_policy_sources() {
    for verifier in [false, true] {
        let (_directory, runtime, headers) = fixture(verifier);
        let credential = runtime.auth.read().unwrap().credentials["workload"].clone();
        credential.rate.lock().unwrap().tokens = 0.0;
        for (alias, endpoint) in [
            ("native-chat", "/chat/completions"),
            ("native-responses", "/responses"),
        ] {
            let response = invoke(runtime.clone(), &headers, alias, endpoint).await;
            assert_eq!(
                response.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "verifier={verifier} endpoint={endpoint}"
            );
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "rate_limit_exceeded");
            assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
            assert_eq!(credential.class_semaphore.available_permits(), 1);
        }
    }
}

#[tokio::test]
async fn copilot_dispatch_authorized_control_consumes_exactly_once() {
    for verifier in [false, true] {
        let (_directory, runtime, headers) = fixture(verifier);
        let credential = runtime.auth.read().unwrap().credentials["workload"].clone();
        for (alias, endpoint) in [
            ("native-chat", "/chat/completions"),
            ("native-responses", "/responses"),
        ] {
            let before = credential.rate.lock().unwrap().tokens;
            let response = invoke(runtime.clone(), &headers, alias, endpoint).await;
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "upstream_auth_error");
            let after = credential.rate.lock().unwrap().tokens;
            assert!(
                (before - after - 1.0).abs() < 0.05,
                "exactly one token per authorized dispatch: {before} -> {after}"
            );
            assert_eq!(credential.class_semaphore.available_permits(), 1);
        }
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 2);
    }
}

#[tokio::test]
async fn copilot_dispatch_authorization_rejection_does_not_debit_bucket() {
    let (_directory, runtime, headers) = fixture(false);
    let credential = runtime.auth.read().unwrap().credentials["workload"].clone();
    let response = invoke(runtime.clone(), &headers, "native-chat", "/responses").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(credential.rate.lock().unwrap().tokens, 2.0);
    assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn copilot_dispatch_denied_grants_precede_rate_admission() {
    for denial in ["cloud", "route", "alias"] {
        let (_directory, runtime, headers) = fixture(false);
        {
            let mut auth = runtime.auth.write().unwrap();
            let auth = Arc::get_mut(&mut auth).unwrap();
            let credential = Arc::get_mut(auth.credentials.get_mut("workload").unwrap()).unwrap();
            match denial {
                "cloud" => credential.config.cloud_allowed = false,
                "route" => credential.config.allowed_routes.clear(),
                _ => credential
                    .config
                    .aliases
                    .retain(|alias| alias != "native-chat"),
            }
            credential.rate.lock().unwrap().tokens = 0.0;
        }
        let response = invoke(
            runtime.clone(),
            &headers,
            "native-chat",
            "/chat/completions",
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{denial} must be checked before rate admission"
        );
        assert_eq!(runtime.upstream_errors.load(Ordering::Relaxed), 0);
        assert_eq!(
            runtime.auth.read().unwrap().credentials["workload"]
                .rate
                .lock()
                .unwrap()
                .tokens,
            0.0
        );
    }
}

#[tokio::test]
async fn copilot_dispatch_keeps_pinned_exemption_and_codex_managed_admission() {
    for codex in [false, true] {
        let (_directory, mut runtime, headers) = fixture(false);
        let mutable = Arc::get_mut(&mut runtime).unwrap();
        if codex {
            mutable.policy.config.codex.alias = Some("other-native".into());
        } else {
            mutable.policy.config.aliases.insert(
                "other-native".into(),
                serde_json::from_value(json!({
                    "context_limit":2000, "capabilities":["text"], "routes":[],
                    "passthrough_route":"unavailable-synthetic-route"
                }))
                .unwrap(),
            );
        }
        {
            let mut auth = runtime.auth.write().unwrap();
            let auth = Arc::get_mut(&mut auth).unwrap();
            let credential = Arc::get_mut(auth.credentials.get_mut("workload").unwrap()).unwrap();
            credential.config.aliases.push("other-native".into());
            credential.rate.lock().unwrap().tokens = 0.0;
        }
        let response = invoke(
            runtime.clone(),
            &headers,
            "other-native",
            "/chat/completions",
        )
        .await;
        assert_eq!(
            response.status(),
            if codex {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["error"]["code"],
            if codex {
                "rate_limit_exceeded"
            } else {
                "route_unavailable"
            }
        );
    }
}
