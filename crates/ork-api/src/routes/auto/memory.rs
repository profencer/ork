//! ADR-0056 §`Decision`: memory routes (threads + working memory).
//!
//! Backed by [`OrkApp::memory`](ork_app::OrkApp::memory). Returns 503
//! when no [`MemoryStore`] is configured on the app.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Extension, Path, Query};
use axum::http::request::Parts;
use axum::response::Json;
use axum::routing::{delete, get, post};
use ork_a2a::{ResourceId, ThreadId};
use ork_app::OrkApp;
use ork_common::types::TenantId;
use ork_core::ports::memory_store::MemoryContext;
use serde::Deserialize;
use uuid::Uuid;

use crate::dto::{
    AppendMessageInput, AppendMessageOutput, OkResponse, ThreadSummaryDto, WorkingMemoryRead,
    WorkingMemoryWrite,
};
use crate::error::{ApiError, ErrorKind};
use crate::scope_check::require_scope;

/// ADR-0021 §`Vocabulary`: per-resource memory read scope.
fn memory_read_scope(resource_id: &ResourceId) -> String {
    format!("memory:{resource_id}:read")
}

/// ADR-0021 §`Vocabulary`: per-resource memory write scope.
fn memory_write_scope(resource_id: &ResourceId) -> String {
    format!("memory:{resource_id}:write")
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/memory/threads", get(list_threads))
        .route("/api/memory/threads/{id}/messages", post(append_message))
        .route("/api/memory/threads/{id}", delete(delete_thread))
        .route("/api/memory/working", get(read_working).put(put_working))
}

#[derive(Debug, Deserialize)]
struct ResourceQuery {
    resource: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkingMemoryQuery {
    resource: String,
    agent: String,
}

async fn list_threads(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Query(q): Query<ResourceQuery>,
    parts: Parts,
) -> Result<Json<Vec<ThreadSummaryDto>>, ApiError> {
    let memory = app
        .memory()
        .ok_or_else(|| ApiError::new(ErrorKind::Unsupported, "no MemoryStore configured"))?;
    let resource = parse_resource_id(q.resource.as_deref(), tenant)?;
    require_scope(&parts, &memory_read_scope(&resource))?;
    let threads = memory
        .list_threads(tenant, &resource)
        .await
        .map_err(ApiError::from)?;
    let out: Vec<ThreadSummaryDto> = threads
        .into_iter()
        .map(|t| ThreadSummaryDto {
            thread_id: t.thread_id.to_string(),
            last_message_at: t.last_message_at.to_rfc3339(),
            message_count: t.message_count,
        })
        .collect();
    Ok(Json(out))
}

async fn append_message(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(thread_id_str): Path<String>,
    Query(q): Query<ResourceQuery>,
    parts: Parts,
    Json(body): Json<AppendMessageInput>,
) -> Result<Json<AppendMessageOutput>, ApiError> {
    let memory = app
        .memory()
        .ok_or_else(|| ApiError::new(ErrorKind::Unsupported, "no MemoryStore configured"))?;
    let thread_id = parse_thread_id(&thread_id_str)?;
    let resource = parse_resource_id(q.resource.as_deref(), tenant)?;
    require_scope(&parts, &memory_write_scope(&resource))?;
    if body.agent_id.trim().is_empty() {
        return Err(ApiError::validation("`agent_id` must be non-empty"));
    }
    let chat_msg: ork_core::ports::llm::ChatMessage = serde_json::from_value(body.message)
        .map_err(|e| ApiError::validation(format!("invalid `message`: {e}")))?;
    let ctx = MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id,
        agent_id: body.agent_id,
    };
    let id = memory
        .append_message(&ctx, chat_msg)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AppendMessageOutput { id: id.to_string() }))
}

async fn delete_thread(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(thread_id_str): Path<String>,
    Query(q): Query<ResourceQuery>,
    parts: Parts,
) -> Result<Json<OkResponse>, ApiError> {
    let memory = app
        .memory()
        .ok_or_else(|| ApiError::new(ErrorKind::Unsupported, "no MemoryStore configured"))?;
    let thread_id = parse_thread_id(&thread_id_str)?;
    let resource = parse_resource_id(q.resource.as_deref(), tenant)?;
    require_scope(&parts, &memory_write_scope(&resource))?;
    let ctx = MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id,
        agent_id: String::new(),
    };
    memory.delete_thread(&ctx).await.map_err(ApiError::from)?;
    Ok(Json(OkResponse { ok: true }))
}

async fn read_working(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Query(q): Query<WorkingMemoryQuery>,
    parts: Parts,
) -> Result<Json<WorkingMemoryRead>, ApiError> {
    let memory = app
        .memory()
        .ok_or_else(|| ApiError::new(ErrorKind::Unsupported, "no MemoryStore configured"))?;
    if q.agent.trim().is_empty() {
        return Err(ApiError::validation(
            "`agent` query parameter must be non-empty",
        ));
    }
    let resource = parse_resource_id(Some(&q.resource), tenant)?;
    require_scope(&parts, &memory_read_scope(&resource))?;
    let ctx = MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id: ThreadId::new(),
        agent_id: q.agent,
    };
    let value = memory.working_memory(&ctx).await.map_err(ApiError::from)?;
    Ok(Json(WorkingMemoryRead {
        value,
        schema: None,
        updated_at: None,
    }))
}

async fn put_working(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Query(q): Query<WorkingMemoryQuery>,
    parts: Parts,
    Json(body): Json<WorkingMemoryWrite>,
) -> Result<Json<OkResponse>, ApiError> {
    let memory = app
        .memory()
        .ok_or_else(|| ApiError::new(ErrorKind::Unsupported, "no MemoryStore configured"))?;
    if q.agent.trim().is_empty() {
        return Err(ApiError::validation(
            "`agent` query parameter must be non-empty",
        ));
    }
    let resource = parse_resource_id(Some(&q.resource), tenant)?;
    require_scope(&parts, &memory_write_scope(&resource))?;
    let ctx = MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id: ThreadId::new(),
        agent_id: q.agent,
    };
    memory
        .set_working_memory(&ctx, body.value)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(OkResponse { ok: true }))
}

fn parse_thread_id(s: &str) -> Result<ThreadId, ApiError> {
    Uuid::parse_str(s)
        .map(ThreadId)
        .map_err(|_| ApiError::validation(format!("invalid thread id `{s}`")))
}

fn parse_resource_id(s: Option<&str>, tenant: TenantId) -> Result<ResourceId, ApiError> {
    match s {
        Some(s) => Uuid::parse_str(s)
            .map(ResourceId)
            .map_err(|_| ApiError::validation(format!("invalid resource id `{s}`"))),
        None => Ok(ResourceId::anonymous(tenant.0)),
    }
}
