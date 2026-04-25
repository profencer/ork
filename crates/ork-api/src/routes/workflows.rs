use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::Utc;
use ork_common::types::{WorkflowId, WorkflowRunId};
use ork_core::models::workflow::{WorkflowDefinition, WorkflowTrigger};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::middleware::AuthContext;
use crate::state::AppState;

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/api/workflows", post(create_workflow))
        .route("/api/workflows", get(list_workflows))
        .route("/api/workflows/{id}", get(get_workflow))
        .route("/api/workflows/{id}/runs", post(start_run))
        .route("/api/workflows/{id}/runs", get(list_runs))
        .route("/api/runs/{run_id}", get(get_run))
        .with_state(state)
}

#[derive(Deserialize)]
struct CreateWorkflowRequest {
    name: String,
    version: String,
    trigger: WorkflowTrigger,
    steps: Vec<ork_core::models::workflow::WorkflowStep>,
}

async fn create_workflow(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateWorkflowRequest>,
) -> impl IntoResponse {
    let def = WorkflowDefinition {
        id: WorkflowId::new(),
        tenant_id: ctx.tenant_id,
        name: req.name,
        version: req.version,
        trigger: req.trigger,
        steps: req.steps,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    match state
        .workflow_service
        .create_definition(ctx.tenant_id, &def)
        .await
    {
        Ok(created) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(created).unwrap()),
        )
            .into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_workflows(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> impl IntoResponse {
    match state.workflow_service.list_definitions(ctx.tenant_id).await {
        Ok(defs) => Json(serde_json::to_value(defs).unwrap()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_workflow(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .workflow_service
        .get_definition(ctx.tenant_id, WorkflowId(id))
        .await
    {
        Ok(def) => Json(serde_json::to_value(def).unwrap()).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct StartRunRequest {
    #[serde(default)]
    input: serde_json::Value,
}

async fn start_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<StartRunRequest>,
) -> impl IntoResponse {
    match state
        .workflow_service
        .start_run(ctx.tenant_id, WorkflowId(id), req.input)
        .await
    {
        Ok(run) => {
            let engine = state.engine.clone();
            let wf = state.workflow_service.clone();
            let tenant_id = ctx.tenant_id;
            let run_exec = run.clone();
            tokio::spawn(async move {
                if let Err(e) = wf.run_workflow(engine, tenant_id, run_exec).await {
                    error!(run_id = %run.id, error = %e, "workflow execution failed");
                }
            });
            (
                StatusCode::CREATED,
                Json(serde_json::to_value(run).unwrap()),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ListRunsQuery {
    workflow_id: Option<Uuid>,
}

async fn list_runs(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(_id): Path<Uuid>,
    Query(query): Query<ListRunsQuery>,
) -> impl IntoResponse {
    let wf_id = query.workflow_id.map(WorkflowId);
    match state.workflow_service.list_runs(ctx.tenant_id, wf_id).await {
        Ok(runs) => Json(serde_json::to_value(runs).unwrap()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_run(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(run_id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .workflow_service
        .get_run(ctx.tenant_id, WorkflowRunId(run_id))
        .await
    {
        Ok(run) => Json(serde_json::to_value(run).unwrap()).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
