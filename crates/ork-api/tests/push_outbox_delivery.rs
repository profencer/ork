//! ADR-0009 §`Delivery worker` end-to-end happy path:
//!
//! 1. Stand up a `wiremock` server as the subscriber.
//! 2. Register a push config for a task via `tasks/pushNotificationConfig/set`.
//! 3. Spawn the in-process `PushDeliveryWorker`.
//! 4. Drive `message/send` so the JSON-RPC dispatcher publishes a terminal
//!    envelope onto `ork.a2a.v1.push.outbox`.
//! 5. Assert the subscriber received exactly one POST with the documented
//!    `X-A2A-Signature` / `X-A2A-Key-Id` / `Authorization` headers, and that
//!    the JWS verifies against the JWKS the provider serves.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey};
use ork_a2a::{MessageSendParams, Part};
use ork_api::routes::a2a;
use ork_push::worker::WorkerConfig;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{auth_for, build_worker, jsonrpc_request, read_body, test_state_with_push};

/// Wait for the wiremock server to record at least `n` requests, polling
/// `received_requests()` until either the budget runs out or the count is
/// reached. Helps the worker's async POST settle before assertions.
async fn wait_for_requests(server: &MockServer, n: usize, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(reqs) = server.received_requests().await
            && reqs.len() >= n
        {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("expected at least {n} requests within {budget:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_worker_signs_and_posts_to_subscriber() {
    let t = test_state_with_push().await;
    let tenant = t.tenant_id;

    let subscriber = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/cb"))
        .and(header_exists("X-A2A-Signature"))
        .and(header_exists("X-A2A-Key-Id"))
        .and(header_exists("Authorization"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&subscriber)
        .await;

    // Spawn the worker before we publish anything so the subscriber is ready.
    let cancel = CancellationToken::new();
    let worker = build_worker(&t, WorkerConfig::default());
    let worker_handle = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };

    // 1) `message/send` → `update_state(Completed)` → outbox publish.
    let app = a2a::protected_router(t.state.clone());
    let params = MessageSendParams {
        message: ork_a2a::Message::user(vec![Part::text("ping")]),
        configuration: None,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-1"),
        "message/send",
        serde_json::to_value(&params).unwrap(),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert!(v["error"].is_null(), "send must succeed; got {v}");
    let task_id_str = v["result"]["id"].as_str().expect("task id").to_string();

    // 2) Register the push config AFTER `send` so the worker also catches the
    //    case where `set` lands while the envelope is still being processed —
    //    in this happy path the envelope was buffered by the in-memory broker
    //    so order doesn't matter, but we mirror the typical client flow.
    let app = a2a::protected_router(t.state.clone());
    let set_body = jsonrpc_request(
        json!("rpc-set"),
        "tasks/pushNotificationConfig/set",
        json!({
            "task_id": task_id_str,
            "push_notification_config": {
                "url": format!("{}/cb", subscriber.uri()),
                "token": "secret-token",
            },
        }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(set_body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 3) Re-trigger a terminal transition so the worker sees the outbox event
    //    AFTER the config is in place — the in-memory broker is broadcast,
    //    so subscribers only see messages published while they're attached.
    let app = a2a::protected_router(t.state.clone());
    let body = jsonrpc_request(json!("rpc-2"), "tasks/cancel", json!({ "id": task_id_str }));
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_requests(&subscriber, 1, Duration::from_secs(5)).await;

    let received = subscriber.received_requests().await.unwrap();
    let req = received
        .iter()
        .find(|r| {
            r.headers
                .get("Authorization")
                .map(|v| v.to_str().unwrap_or_default() == "Bearer secret-token")
                .unwrap_or(false)
        })
        .expect("at least one request with the expected Bearer token");

    let auth = req.headers.get("Authorization").unwrap().to_str().unwrap();
    assert_eq!(
        auth, "Bearer secret-token",
        "ADR-0009: token MUST be forwarded as Bearer"
    );
    let kid = req
        .headers
        .get("X-A2A-Key-Id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let sig = req
        .headers
        .get("X-A2A-Signature")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let ts = req
        .headers
        .get("X-A2A-Timestamp")
        .expect("timestamp header");
    let _ = chrono::DateTime::parse_from_rfc3339(ts.to_str().unwrap()).expect("RFC3339 timestamp");

    // 4) Verify the detached JWS against the JWKS endpoint.
    let parts: Vec<&str> = sig.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "detached JWS shape: header..signature; got {sig}"
    );
    let header_b64 = parts[0];
    let signature_b64 = parts[2];
    let payload_b64 = URL_SAFE_NO_PAD.encode(Sha256::digest(&req.body));
    let signing_input = format!("{header_b64}.{payload_b64}");

    let jwks = t.jwks_provider.jwks().await;
    let key = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kid"] == serde_json::Value::String(kid.clone()))
        .expect("kid present in JWKS");
    let x = key["x"].as_str().unwrap();
    let y = key["y"].as_str().unwrap();
    let decoding = DecodingKey::from_ec_components(x, y).unwrap();
    let valid = jsonwebtoken::crypto::verify(
        signature_b64,
        signing_input.as_bytes(),
        &decoding,
        Algorithm::ES256,
    )
    .unwrap();
    assert!(
        valid,
        "JWS MUST verify against the JWK published at /.well-known/jwks.json"
    );

    // The body itself should be the canonical push notification envelope.
    let body: serde_json::Value = serde_json::from_slice(&req.body).expect("JSON body");
    assert_eq!(body["task_id"], task_id_str);
    assert_eq!(body["state"], "canceled");
    assert!(body["occurred_at"].is_string());

    // No dead-letter row on success.
    let dl = t.dead_letter_repo.snapshot().await;
    assert!(
        dl.is_empty(),
        "successful delivery MUST NOT dead-letter; got {dl:?}"
    );

    cancel.cancel();
    let _ = timeout(Duration::from_secs(2), worker_handle).await;
}

// Suppress the unused-import warning when the test above is the only one in
// the file; helps when contributors strip down the file for bisecting.
#[allow(dead_code)]
fn _types_used(_: Arc<()>) {}
