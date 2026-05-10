//! ADR-0056 §`Decision`: scorer binding listing + result query.
//!
//! Listing of registered [`ScorerBinding`](ork_app::types::ScorerBinding)s
//! is read-only. `/api/scorer-results` reads through
//! [`ScorerResultSink::list_recent`](ork_eval::live::ScorerResultSink::list_recent);
//! the in-memory v1 sink returns the most recent rows.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Extension, Query};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use ork_app::OrkApp;
use serde::Deserialize;
use serde_json::Value;

use crate::dto::{ScorerBindingSummary, ScorerRow, ScorerRowList};
use crate::error::ApiError;

pub fn routes() -> Router {
    Router::new()
        .route("/api/scorers", get(list_scorers))
        .route("/api/scorer-results", get(list_results))
}

async fn list_scorers(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    let rows: Vec<ScorerBindingSummary> = app
        .scorers()
        .iter()
        .map(|b| ScorerBindingSummary {
            target: serde_json::to_value(&b.target).unwrap_or(Value::Null),
            scorer_id: b.spec.scorer().id().to_string(),
            label: None,
        })
        .collect();
    Json(rows)
}

#[derive(Debug, Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

async fn list_results(
    Extension(app): Extension<Arc<OrkApp>>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<ScorerRowList>, ApiError> {
    let limit = q.limit.unwrap_or(100).min(1000);
    let rows = app.scorer_sink().list_recent(limit).await;
    let dto_rows: Vec<ScorerRow> = rows
        .into_iter()
        .map(|r| ScorerRow {
            run_id: r.run_id.0.to_string(),
            scorer_id: r.scorer_id,
            target: serde_json::json!({
                "agent_id": r.agent_id,
                "workflow_id": r.workflow_id,
            }),
            score: f64::from(r.score),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        })
        .collect();
    Ok(Json(ScorerRowList { rows: dto_rows }))
}
