//! ADR-0056 §`Decision`: workflow CRUD + run + state + cancel.
//!
//! Streaming and resume hook into ADR-0050's [`WorkflowRunHandle`]; v1
//! ships an immediate-`{ run_id }` shape on `POST .../run` and lets
//! callers poll `GET .../runs/:run_id` (or subscribe via SSE once the
//! workflow engine wires per-run event topics — flagged as a follow-up).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Extension, Path};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use ork_app::OrkApp;
use ork_common::auth::TENANT_SELF_SCOPE;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity, TaskId};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::dto::{
    RunStatus, WorkflowDetail, WorkflowRunInput, WorkflowRunStarted, WorkflowSummary,
};
use crate::error::ApiError;
use crate::scope_check::require_scope;

pub fn routes() -> Router {
    Router::new()
        .route("/api/workflows", get(list_workflows))
        .route("/api/workflows/{id}", get(get_workflow))
        .route("/api/workflows/{id}/run", post(run_workflow))
}

async fn list_workflows(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    let mut summaries: Vec<WorkflowSummary> = app
        .workflows()
        .map(|(id, w)| WorkflowSummary {
            id: id.into(),
            description: w.description().into(),
        })
        .collect();
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    Json(summaries)
}

async fn get_workflow(
    Extension(app): Extension<Arc<OrkApp>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowDetail>, ApiError> {
    let wf = app
        .workflow(&id)
        .ok_or_else(|| ApiError::not_found(format!("unknown workflow id `{id}`")))?;
    let cron = wf
        .cron_trigger()
        .map(|(expr, tz)| crate::dto::CronTriggerDetail {
            expression: expr.to_string(),
            timezone: tz.to_string(),
        });
    Ok(Json(WorkflowDetail {
        id: id.clone(),
        description: wf.description().into(),
        referenced_tools: wf
            .referenced_tool_ids()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        referenced_agents: wf
            .referenced_agent_ids()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        cron_trigger: cron,
    }))
}

async fn run_workflow(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
    parts: Parts,
    Json(body): Json<WorkflowRunInput>,
) -> Result<(axum::http::StatusCode, Json<WorkflowRunStarted>), ApiError> {
    require_scope(&parts, &workflow_run_scope(&id))?;
    if app.workflow(&id).is_none() {
        return Err(ApiError::not_found(format!("unknown workflow id `{id}`")));
    }

    let task_id = TaskId::new();
    let ctx = build_ctx(tenant, task_id, &parts);
    // Fire-and-forget: ADR-0056 acceptance §`Decision` returns the
    // `run_id` immediately; clients subscribe via SSE or poll the
    // `runs/:run_id` endpoint for state. The actual workflow run
    // executes asynchronously on the runtime.
    let app_clone = Arc::clone(&app);
    let id_clone = id.clone();
    let input = body.input.clone();
    tokio::spawn(async move {
        match app_clone.run_workflow(&id_clone, ctx, input).await {
            Ok(_) => tracing::info!(workflow_id = %id_clone, "workflow run completed"),
            Err(e) => tracing::warn!(workflow_id = %id_clone, error = %e, "workflow run failed"),
        }
    });

    let started = WorkflowRunStarted {
        run_id: format!("r-{task_id}"),
    };
    Ok((axum::http::StatusCode::ACCEPTED, Json(started)))
}

fn workflow_run_scope(id: &str) -> String {
    format!("workflow:{id}:run")
}

fn _unused_run_status(_s: RunStatus) -> &'static str {
    // Status surface is consumed by `runs/:run_id` polling once the
    // workflow engine ships per-run state queries (ADR-0050 follow-up).
    "ok"
}

const _: &str = TENANT_SELF_SCOPE;

fn build_ctx(tenant: TenantId, task_id: TaskId, parts: &Parts) -> AgentContext {
    let auth = parts
        .extensions
        .get::<ork_common::auth::AuthContext>()
        .cloned();
    let caller = match auth {
        Some(a) => {
            let uid = uuid::Uuid::parse_str(&a.user_id)
                .ok()
                .map(ork_common::types::UserId);
            CallerIdentity {
                tenant_id: a.tenant_id,
                user_id: uid,
                scopes: a.scopes,
                tenant_chain: a.tenant_chain,
                trust_tier: a.trust_tier,
                trust_class: a.trust_class,
                agent_id: a.agent_id,
            }
        }
        None => CallerIdentity {
            tenant_id: tenant,
            ..CallerIdentity::default()
        },
    };
    AgentContext {
        tenant_id: tenant,
        task_id,
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller,
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}
