//! ADR-0055 ships eight panels; the Traces and Logs panels depend on
//! the observability ingestion ADR (referenced as "0058" in the ADR
//! body, since reassigned — see `## Reviewer findings`) that has not
//! landed. v1 returns a structured `501 Not Implemented` so the SPA
//! can render a banner and clients see a stable contract before and
//! after the observability ADR ships.

use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};

pub fn routes() -> Router {
    Router::new()
        .route("/studio/api/traces", get(deferred_traces))
        .route("/studio/api/traces/{run_id}", get(deferred_traces))
        .route("/studio/api/logs", get(deferred_logs))
}

async fn deferred_traces() -> impl IntoResponse {
    deferred(
        "traces",
        "Observability/OTel ingestion ADR has not landed; \
         Studio's Traces panel renders an empty state until then.",
    )
}

async fn deferred_logs() -> impl IntoResponse {
    deferred(
        "logs",
        "Observability/log ingestion ADR has not landed; \
         Studio's Logs panel renders an empty state until then.",
    )
}

fn deferred(panel: &'static str, message: &'static str) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "studio_api_version": crate::envelope::STUDIO_API_VERSION,
            "error": "not implemented",
            "panel": panel,
            "message": message,
            "deferred_to": "observability-adr",
        })),
    )
}
