//! `/studio/api/memory*` — Studio Memory panel data sources.
//!
//! ADR-0055 §`Decision`: aggregate working memory + thread list +
//! recent semantic-recall hits in a single response so the panel can
//! render without a chained client-side fetch waterfall.
//!
//! Thread delete delegates to the same `MemoryStore::delete_thread`
//! the auto-generated `/api/memory/threads/:id` handler uses (see
//! [`ork_api::routes::auto::memory`]), so the underlying contract —
//! cleanup from `mem_messages` and `mem_embeddings` — is one
//! implementation, not two.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query},
    http::{StatusCode, request::Parts},
    response::IntoResponse,
    routing::{delete, get},
};
use ork_a2a::{ResourceId, ThreadId};
use ork_app::OrkApp;
use ork_common::types::TenantId;
use ork_core::ports::memory_store::MemoryContext;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::{StudioEnvelope, ok};

pub fn routes() -> Router {
    Router::new()
        .route("/studio/api/memory", get(get_memory))
        .route(
            "/studio/api/memory/threads/{id}",
            delete(delete_studio_thread),
        )
}

#[derive(Debug, Deserialize)]
struct MemoryQuery {
    resource: String,
    /// Optional agent slot for working memory. Omit to skip the
    /// working-memory fetch.
    #[serde(default = "default_agent")]
    agent: String,
    /// Optional similarity query. Omit (or pass empty) to skip the
    /// semantic-recall fetch entirely; the response then carries an
    /// empty `recent_recall`. Set to drive top-K vector retrieval
    /// against the working-memory store.
    #[serde(default)]
    recall_query: Option<String>,
    #[serde(default = "default_top_k")]
    top_k: usize,
}

fn default_agent() -> String {
    String::new()
}

fn default_top_k() -> usize {
    5
}

#[derive(Debug, Serialize)]
struct MemoryView {
    working: Option<serde_json::Value>,
    threads: Vec<ThreadDto>,
    recent_recall: Vec<RecallDto>,
}

#[derive(Debug, Serialize)]
struct ThreadDto {
    thread_id: String,
    last_message_at: String,
    message_count: u64,
}

#[derive(Debug, Serialize)]
struct RecallDto {
    message_id: String,
    thread_id: String,
    content: String,
    score: f32,
}

async fn get_memory(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Query(q): Query<MemoryQuery>,
    parts: Parts,
) -> Result<StudioEnvelope<MemoryView>, axum::response::Response> {
    let memory = app.memory().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_envelope("no MemoryStore configured")),
        )
            .into_response()
    })?;

    let resource = parse_resource(Some(&q.resource), tenant)
        .map_err(|m| (StatusCode::BAD_REQUEST, Json(error_envelope(&m))).into_response())?;

    require_scope(&parts, &memory_read_scope(&resource))?;

    let threads = memory
        .list_threads(tenant, &resource)
        .await
        .map_err(internal_error)?;
    let thread_dtos: Vec<ThreadDto> = threads
        .into_iter()
        .map(|t| ThreadDto {
            thread_id: t.thread_id.to_string(),
            last_message_at: t.last_message_at.to_rfc3339(),
            message_count: t.message_count,
        })
        .collect();

    let working = if q.agent.is_empty() {
        None
    } else {
        let ctx = MemoryContext {
            tenant_id: tenant,
            resource_id: resource,
            thread_id: ThreadId::new(),
            agent_id: q.agent.clone(),
        };
        memory.working_memory(&ctx).await.map_err(internal_error)?
    };

    let recent_recall = if let Some(query) = q.recall_query.as_deref().filter(|s| !s.is_empty()) {
        let ctx = MemoryContext {
            tenant_id: tenant,
            resource_id: resource,
            thread_id: ThreadId::new(),
            agent_id: q.agent.clone(),
        };
        let hits = memory
            .semantic_recall(&ctx, query, q.top_k)
            .await
            .map_err(internal_error)?;
        hits.into_iter()
            .map(|h| RecallDto {
                message_id: h.message_id.to_string(),
                thread_id: h.thread_id.to_string(),
                content: h.content,
                score: h.score,
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(ok(MemoryView {
        working,
        threads: thread_dtos,
        recent_recall,
    }))
}

#[derive(Debug, Deserialize)]
struct DeleteThreadQuery {
    resource: String,
}

async fn delete_studio_thread(
    Extension(app): Extension<Arc<OrkApp>>,
    Extension(tenant): Extension<TenantId>,
    Path(thread_id_str): Path<String>,
    Query(q): Query<DeleteThreadQuery>,
    parts: Parts,
) -> Result<StudioEnvelope<OkBody>, axum::response::Response> {
    let memory = app.memory().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_envelope("no MemoryStore configured")),
        )
            .into_response()
    })?;

    let thread_id = Uuid::parse_str(&thread_id_str).map(ThreadId).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(error_envelope(&format!(
                "invalid thread id `{thread_id_str}`"
            ))),
        )
            .into_response()
    })?;

    let resource = parse_resource(Some(&q.resource), tenant)
        .map_err(|m| (StatusCode::BAD_REQUEST, Json(error_envelope(&m))).into_response())?;

    require_scope(&parts, &memory_write_scope(&resource))?;

    let ctx = MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id,
        agent_id: String::new(),
    };

    // ADR-0053 §`delete_thread`: the backend hard-deletes the row from
    // `mem_messages` and `mem_embeddings`. ADR-0055 AC #9 asserts both
    // tables are cleared end-to-end.
    memory.delete_thread(&ctx).await.map_err(internal_error)?;

    Ok(ok(OkBody { ok: true }))
}

#[derive(Debug, Serialize)]
pub struct OkBody {
    pub ok: bool,
}

fn parse_resource(s: Option<&str>, tenant: TenantId) -> Result<ResourceId, String> {
    match s {
        Some(s) if !s.is_empty() => Uuid::parse_str(s)
            .map(ResourceId)
            .map_err(|_| format!("invalid resource id `{s}`")),
        _ => Ok(ResourceId::anonymous(tenant.0)),
    }
}

fn error_envelope(msg: &str) -> serde_json::Value {
    serde_json::json!({
        "studio_api_version": crate::envelope::STUDIO_API_VERSION,
        "error": msg,
    })
}

fn internal_error(e: ork_common::error::OrkError) -> axum::response::Response {
    let msg = e.to_string();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(error_envelope(&msg)),
    )
        .into_response()
}

/// ADR-0021 §`Vocabulary`: per-resource memory read scope. The
/// helper is `pub(crate)` so the auto-memory delegation pattern lives
/// in one place even if more routes adopt it later.
pub(crate) fn memory_read_scope(resource_id: &ResourceId) -> String {
    format!("memory:{resource_id}:read")
}

/// ADR-0021 §`Vocabulary`: per-resource memory write scope.
pub(crate) fn memory_write_scope(resource_id: &ResourceId) -> String {
    format!("memory:{resource_id}:write")
}

#[allow(clippy::result_large_err)] // axum's `Response` is large by design; boxing
// it would force the call sites through an
// extra deref.
fn require_scope(parts: &Parts, scope: &str) -> Result<(), axum::response::Response> {
    use ork_common::auth::AuthContext;
    use ork_security::ScopeChecker;
    let Some(ctx) = parts.extensions.get::<AuthContext>() else {
        // Dev mode: no AuthContext stamped, follow the same bypass the
        // auto-generated routes use (see
        // `ork_api::scope_check::require_scope`).
        return Ok(());
    };
    if ScopeChecker::allows(&ctx.scopes, scope) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(error_envelope(&format!("missing scope {scope}"))),
        )
            .into_response())
    }
}
