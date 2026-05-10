//! ADR-0056 §`Decision`: agent CRUD + generate + stream.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use futures::StreamExt;
use ork_a2a::{Message as A2aMessage, Role, TaskId};
use ork_app::OrkApp;
use ork_common::auth::agent_invoke_scope;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::dto::{
    AgentDetail, AgentGenerateInput, AgentGenerateOutput, AgentSummary, FinishReason, TokenUsage,
};
use crate::error::ApiError;
use crate::scope_check::require_scope;
use crate::sse::encoder::encode_agent_event;

pub fn routes() -> Router {
    Router::new()
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}", get(get_agent))
        .route("/api/agents/{id}/generate", post(generate))
        .route("/api/agents/{id}/stream", post(stream))
}

async fn list_agents(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    let mut summaries: Vec<AgentSummary> = app
        .agents()
        .map(|(id, a)| AgentSummary {
            id: id.into(),
            description: a.card().description.clone(),
            card_name: a.card().name.clone(),
        })
        .collect();
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    Json(summaries)
}

async fn get_agent(
    Extension(app): Extension<Arc<OrkApp>>,
    Path(id): Path<String>,
) -> Result<Json<AgentDetail>, ApiError> {
    let agent = app
        .agent(&id)
        .ok_or_else(|| ApiError::not_found(format!("unknown agent id `{id}`")))?;
    let detail = AgentDetail {
        id: id.clone(),
        description: agent.card().description.clone(),
        card_name: agent.card().name.clone(),
        skills: serde_json::to_value(&agent.card().skills).unwrap_or(Value::Null),
        request_context_schema: app_request_context_schema(&app),
    };
    Ok(Json(detail))
}

fn app_request_context_schema(app: &OrkApp) -> Option<Value> {
    app.manifest().request_context_schema
}

async fn generate(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
    parts: Parts,
    Json(input): Json<AgentGenerateInput>,
) -> Result<Json<AgentGenerateOutput>, ApiError> {
    require_scope(&parts, &agent_invoke_scope(&id))?;
    let agent = app
        .agent(&id)
        .ok_or_else(|| ApiError::not_found(format!("unknown agent id `{id}`")))?;

    validate_request_context(&app, &input.request_context)?;
    let prompt = parse_user_message(&input.message)?;

    let task_id = TaskId::new();
    let ctx = build_ctx(tenant, task_id, &parts);

    let mut stream = agent
        .send_stream(ctx, prompt)
        .await
        .map_err(ApiError::from)?;

    // Fold the event stream into a final message.
    let mut last_message: Option<A2aMessage> = None;
    let timeout = Duration::from_secs(300);
    let folded = tokio::time::timeout(timeout, async {
        while let Some(event) = stream.next().await {
            let event = event?;
            if let ork_a2a::TaskEvent::Message(m) = event {
                last_message = Some(m);
            }
        }
        Ok::<(), ork_common::error::OrkError>(())
    })
    .await;

    match folded {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(ApiError::from(e)),
        Err(_) => {
            return Err(ApiError::new(
                crate::error::ErrorKind::Upstream,
                "agent stream timed out after 5 minutes",
            ));
        }
    }

    let final_msg = last_message.ok_or_else(|| {
        ApiError::new(
            crate::error::ErrorKind::Upstream,
            "agent stream ended without a final message",
        )
    })?;

    let out = AgentGenerateOutput {
        run_id: format!("r-{task_id}"),
        message: serde_json::to_value(&final_msg).unwrap_or(Value::Null),
        structured_output: None,
        usage: TokenUsage::default(),
        finish_reason: FinishReason::Stop,
    };
    Ok(Json(out))
}

async fn stream(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
    parts: Parts,
    Json(input): Json<AgentGenerateInput>,
) -> Result<axum::response::Response, ApiError> {
    require_scope(&parts, &agent_invoke_scope(&id))?;
    let agent = app
        .agent(&id)
        .ok_or_else(|| ApiError::not_found(format!("unknown agent id `{id}`")))?;
    validate_request_context(&app, &input.request_context)?;
    let prompt = parse_user_message(&input.message)?;

    let task_id = TaskId::new();
    let run_id = format!("r-{task_id}");
    let ctx = build_ctx(tenant, task_id, &parts);

    let inner = agent
        .send_stream(ctx, prompt)
        .await
        .map_err(ApiError::from)?;

    let run_id_clone = run_id.clone();
    let mapped = inner.map(move |evt| match evt {
        Ok(event) => {
            Ok::<Event, std::convert::Infallible>(encode_agent_event(&event, Some(&run_id_clone)))
        }
        Err(e) => Ok(Event::default()
            .event("error")
            .data(serde_json::json!({ "kind": "error", "message": e.to_string() }).to_string())),
    });

    let sse = Sse::new(Box::pin(mapped) as futures::stream::BoxStream<'static, _>)
        .keep_alive(KeepAlive::default());
    Ok(sse.into_response())
}

fn validate_request_context(app: &OrkApp, ctx: &Option<Value>) -> Result<(), ApiError> {
    let (Some(schema), Some(body)) = (app.manifest().request_context_schema, ctx) else {
        return Ok(());
    };
    // Compile once per call. The hot path is hot enough to consider
    // caching the compiled schema on `OrkApp` later (ADR-0056 §
    // `Negative/costs`); v1 trades that for simplicity.
    let compiled = jsonschema::validator_for(&schema).map_err(|e| {
        ApiError::internal(format!("invalid request_context_schema configured: {e}"))
    })?;
    if compiled.is_valid(body) {
        Ok(())
    } else {
        let errors: Vec<String> = compiled.iter_errors(body).map(|e| e.to_string()).collect();
        Err(
            ApiError::validation("request_context does not match the configured schema")
                .with_details(serde_json::json!({ "errors": errors })),
        )
    }
}

fn parse_user_message(v: &Value) -> Result<A2aMessage, ApiError> {
    let mut msg: A2aMessage = serde_json::from_value(v.clone())
        .map_err(|e| ApiError::validation(format!("invalid `message` (A2A Message): {e}")))?;
    // Force the role to user for `/generate` and `/stream` regardless of
    // what the client sent. ADR-0003 reserves the agent role for
    // upstream replies.
    msg.role = Role::User;
    Ok(msg)
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

#[allow(dead_code)]
fn _ensure_status_code_in_scope() -> StatusCode {
    StatusCode::OK
}
