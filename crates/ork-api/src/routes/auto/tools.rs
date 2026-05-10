//! ADR-0056 §`Decision`: tool listing + invoke.
//!
//! `POST /api/tools/:id/invoke` validates the body against the tool's
//! `input_schema()` (ADR-0051 / ADR-0056 §`Validation`), then calls
//! [`ToolDef::invoke`](ork_core::ports::tool_def::ToolDef::invoke).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Extension, Path};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use ork_app::OrkApp;
use ork_common::auth::tool_invoke_scope;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity, TaskId};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::dto::{ToolDetail, ToolInvokeInput, ToolInvokeOutput, ToolSummary};
use crate::error::ApiError;
use crate::scope_check::require_scope;

pub fn routes() -> Router {
    Router::new()
        .route("/api/tools", get(list_tools))
        .route("/api/tools/{id}", get(get_tool))
        .route("/api/tools/{id}/invoke", post(invoke_tool))
}

async fn list_tools(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    let mut summaries: Vec<ToolSummary> = app
        .tools()
        .map(|(id, t)| ToolSummary {
            id: id.into(),
            description: t.description().to_string(),
        })
        .collect();
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    Json(summaries)
}

async fn get_tool(
    Extension(app): Extension<Arc<OrkApp>>,
    Path(id): Path<String>,
) -> Result<Json<ToolDetail>, ApiError> {
    let t = app
        .tool(&id)
        .ok_or_else(|| ApiError::not_found(format!("unknown tool id `{id}`")))?;
    Ok(Json(ToolDetail {
        id: id.clone(),
        description: t.description().to_string(),
        input_schema: t.input_schema().clone(),
        output_schema: t.output_schema().clone(),
    }))
}

async fn invoke_tool(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
    parts: Parts,
    Json(body): Json<ToolInvokeInput>,
) -> Result<Json<ToolInvokeOutput>, ApiError> {
    require_scope(&parts, &tool_invoke_scope(&id))?;
    let tool = app
        .tool(&id)
        .ok_or_else(|| ApiError::not_found(format!("unknown tool id `{id}`")))?;

    // Validate input against the tool's declared schema.
    let schema = tool.input_schema();
    let compiled = jsonschema::validator_for(schema).map_err(|e| {
        ApiError::internal(format!(
            "tool `{id}` has an invalid input_schema configured: {e}"
        ))
    })?;
    if !compiled.is_valid(&body.input) {
        let errors: Vec<String> = compiled
            .iter_errors(&body.input)
            .map(|e| e.to_string())
            .collect();
        return Err(
            ApiError::validation(format!("input does not match tool `{id}` input_schema"))
                .with_details(Value::from(errors)),
        );
    }

    let ctx = build_ctx(tenant, TaskId::new(), &parts);
    let output = tool
        .invoke(&ctx, &body.input)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ToolInvokeOutput { output }))
}

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
