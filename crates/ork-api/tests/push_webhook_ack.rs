//! ADR-0009 §`Inbound ACK route`: `POST /api/webhooks/a2a-ack`.
//!
//! Subscribers may optionally reach back to confirm receipt; the route is
//! mounted in `public_routes` (no JWT) and returns `202 Accepted` per the
//! plan. We assert both the happy path and a missing-headers / loose-body
//! case to make sure the loopback fixture stays maximally permissive.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ork_api::routes::webhooks;
use serde_json::json;
use tower::ServiceExt;

use crate::common::test_state_with_push;

#[tokio::test]
async fn ack_endpoint_returns_accepted_on_well_formed_body() {
    let t = test_state_with_push().await;
    let app = webhooks::routes(t.state.clone());

    let body = json!({
        "task_id": "11111111-1111-1111-1111-111111111111",
        "tenant_id": "22222222-2222-2222-2222-222222222222",
        "state": "completed",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/webhooks/a2a-ack")
        .header("content-type", "application/json")
        .header("X-A2A-Key-Id", "k_dummy")
        .header("X-A2A-Signature", "headerseg..signatureseg")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "ADR-0009: ack endpoint MUST return 202 so the worker's non-2xx \
         retry path is never accidentally tripped by the loopback"
    );
}

#[tokio::test]
async fn ack_endpoint_accepts_missing_signature_headers() {
    let t = test_state_with_push().await;
    let app = webhooks::routes(t.state.clone());

    let body = json!({
        "task_id": "deadbeef-dead-beef-dead-beefdeadbeef",
        "tenant_id": "feedface-feed-face-feed-facefeedface",
        "state": "failed",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/webhooks/a2a-ack")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "missing signature headers must NOT 4xx — verification is the \
         subscriber's responsibility per ADR-0009"
    );
}

#[tokio::test]
async fn ack_endpoint_tolerates_unknown_extra_fields() {
    let t = test_state_with_push().await;
    let app = webhooks::routes(t.state.clone());

    // Future-compat: ADR-0009 leaves room to grow the body shape.
    let body = json!({
        "task_id": "00000000-0000-0000-0000-000000000001",
        "tenant_id": "00000000-0000-0000-0000-000000000002",
        "state": "completed",
        "received_at": "2026-04-24T00:00:00Z",
        "signature_valid": true,
        "ext": { "anything": "here" },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/webhooks/a2a-ack")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}
