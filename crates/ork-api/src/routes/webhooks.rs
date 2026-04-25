use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde::Deserialize;
use tracing::info;

use crate::state::AppState;

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/api/webhooks/pipeline", post(pipeline_webhook))
        // ADR-0009 §`Acknowledgements`: subscriber-reachable shim used by
        // dev/test deployments that want to confirm their listener wiring.
        // Production subscribers respond on their own URL — this endpoint is
        // simply a self-test for `ork`'s own loopback fixtures.
        .route("/api/webhooks/a2a-ack", post(a2a_ack_webhook))
        .with_state(state)
}

#[derive(Deserialize)]
struct PipelineWebhook {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    tenant_slug: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

async fn pipeline_webhook(
    State(state): State<AppState>,
    Json(webhook): Json<PipelineWebhook>,
) -> impl IntoResponse {
    info!(
        provider = %webhook.provider,
        event = %webhook.event,
        tenant = ?webhook.tenant_slug,
        "received pipeline webhook"
    );

    let body = ork_gateways::webhook::PipelineShape {
        provider: webhook.provider,
        event: webhook.event,
        tenant_slug: webhook.tenant_slug,
        payload: webhook.payload,
    };
    ork_gateways::webhook::run_pipeline_webhook(
        state.tenant_service.as_ref(),
        state.workflow_service.clone(),
        state.engine.clone(),
        &body,
    )
    .await;

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "accepted" })),
    )
}

/// Body of an inbound A2A push notification (ADR-0009 §`Push notification body`).
/// Field shape mirrors `ork_push::worker::PushNotification` so the loopback
/// stays in sync with what the worker emits.
#[derive(Debug, Deserialize)]
struct A2aAckBody {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    tenant_id: String,
    #[serde(default)]
    state: String,
}

/// Lightweight ack handler used by integration tests and local self-tests of
/// the push pipeline. The handler intentionally does not verify signatures —
/// real subscribers have to do that themselves; this endpoint is for the
/// `ork` service to prove the worker can reach it. Returns `202 Accepted`
/// per ADR-0009 §`Inbound ACK route`; the worker's "non-2xx => retry" branch
/// is therefore never accidentally tripped by a loopback misconfiguration.
async fn a2a_ack_webhook(
    State(_state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<A2aAckBody>,
) -> impl IntoResponse {
    let kid = headers
        .get("X-A2A-Key-Id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let signature_present = headers.contains_key("X-A2A-Signature");
    info!(
        task_id = %body.task_id,
        tenant_id = %body.tenant_id,
        state = %body.state,
        kid,
        signature_present,
        "ADR-0009: a2a-ack webhook received"
    );
    StatusCode::ACCEPTED
}
