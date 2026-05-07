//! Tenant CRUD HTTP surface. ADR-0020 §`Tenant CRUD restricted` constrains who
//! may exercise each verb:
//!
//! | Route                                | Required scope                                 |
//! | ------------------------------------ | ---------------------------------------------- |
//! | `POST   /api/tenants`                | `tenant:admin`                                 |
//! | `GET    /api/tenants`                | `tenant:admin`                                 |
//! | `GET    /api/tenants/{id}`           | `tenant:admin` OR (`tenant:self` AND id == ctx)|
//! | `PUT    /api/tenants/{id}/settings`  | `tenant:admin` OR (`tenant:self` AND id == ctx)|
//! | `DELETE /api/tenants/{id}`           | `tenant:admin`                                 |
//!
//! Every handler emits an audit-stream `tracing::info!` event with attributes
//! `tenant_id`, `actor`, `action`, `resource`, `result` (ADR-0020 §`Auditing`).
//! These fields are picked up by the OpenTelemetry exporter that ADR-0022's
//! successor will wire; until then they appear in the standard tracing log.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use ork_common::auth::{TENANT_ADMIN_SCOPE, TENANT_SELF_SCOPE};
use ork_common::types::TenantId;
use ork_core::models::tenant::{CreateTenantRequest, UpdateTenantSettingsRequest};
use uuid::Uuid;

use crate::middleware::AuthContext;
use crate::state::AppState;

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/api/tenants", post(create_tenant))
        .route("/api/tenants", get(list_tenants))
        .route("/api/tenants/{id}", get(get_tenant))
        .route("/api/tenants/{id}/settings", put(update_tenant_settings))
        .route("/api/tenants/{id}", delete(delete_tenant))
        .with_state(state)
}

/// Result branch for the audit log — always one of `"ok"`, `"forbidden"`, or
/// `"error"`. Kept at module scope so log queries can pivot on it.
fn audit_result(status: u16) -> &'static str {
    if status < 300 {
        "ok"
    } else if status == 401 || status == 403 {
        "forbidden"
    } else {
        "error"
    }
}

fn forbidden(msg: &str) -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

async fn create_tenant(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateTenantRequest>,
) -> impl IntoResponse {
    if !ctx.has_scope(TENANT_ADMIN_SCOPE) {
        tracing::info!(
            actor = %ctx.user_id,
            tid_chain = ?ctx.tenant_chain,
            tenant_id = %ctx.tenant_id,
            action = "tenant.create",
            resource = %req.slug,
            result = "forbidden",
            "ADR-0020 audit"
        );
        return forbidden("tenant:admin scope required to create tenants");
    }
    let resp = match state.tenant_service.create_tenant(&req).await {
        Ok(tenant) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(tenant).unwrap()),
        )
            .into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    };
    tracing::info!(
        actor = %ctx.user_id,
        tid_chain = ?ctx.tenant_chain,
        tenant_id = %ctx.tenant_id,
        action = "tenant.create",
        resource = %req.slug,
        result = audit_result(resp.status().as_u16()),
        "ADR-0020 audit"
    );
    resp
}

async fn list_tenants(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> impl IntoResponse {
    if !ctx.has_scope(TENANT_ADMIN_SCOPE) {
        tracing::info!(
            actor = %ctx.user_id,
            tid_chain = ?ctx.tenant_chain,
            tenant_id = %ctx.tenant_id,
            action = "tenant.list",
            resource = "*",
            result = "forbidden",
            "ADR-0020 audit"
        );
        return forbidden("tenant:admin scope required to list tenants");
    }
    let resp = match state.tenant_service.list_tenants().await {
        Ok(tenants) => Json(serde_json::to_value(tenants).unwrap()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    };
    tracing::info!(
        actor = %ctx.user_id,
        tid_chain = ?ctx.tenant_chain,
        tenant_id = %ctx.tenant_id,
        action = "tenant.list",
        resource = "*",
        result = audit_result(resp.status().as_u16()),
        "ADR-0020 audit"
    );
    resp
}

async fn get_tenant(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let allowed = ctx.has_scope(TENANT_ADMIN_SCOPE)
        || (ctx.has_scope(TENANT_SELF_SCOPE) && ctx.tenant_id.0 == id);
    if !allowed {
        tracing::info!(
            actor = %ctx.user_id,
            tid_chain = ?ctx.tenant_chain,
            tenant_id = %ctx.tenant_id,
            action = "tenant.read",
            resource = %id,
            result = "forbidden",
            "ADR-0020 audit"
        );
        return forbidden("tenant:admin or tenant:self (own tenant) required");
    }
    let resp = match state.tenant_service.get_tenant(TenantId(id)).await {
        Ok(tenant) => Json(serde_json::to_value(tenant).unwrap()).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    };
    tracing::info!(
        actor = %ctx.user_id,
        tid_chain = ?ctx.tenant_chain,
        tenant_id = %ctx.tenant_id,
        action = "tenant.read",
        resource = %id,
        result = audit_result(resp.status().as_u16()),
        "ADR-0020 audit"
    );
    resp
}

async fn update_tenant_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateTenantSettingsRequest>,
) -> impl IntoResponse {
    let allowed = ctx.has_scope(TENANT_ADMIN_SCOPE)
        || (ctx.has_scope(TENANT_SELF_SCOPE) && ctx.tenant_id.0 == id);
    if !allowed {
        tracing::info!(
            actor = %ctx.user_id,
            tid_chain = ?ctx.tenant_chain,
            tenant_id = %ctx.tenant_id,
            action = "tenant.update_settings",
            resource = %id,
            result = "forbidden",
            "ADR-0020 audit"
        );
        return forbidden("tenant:admin or tenant:self (own tenant) required");
    }

    let resp = match state
        .tenant_service
        .update_settings(TenantId(id), &req)
        .await
    {
        Ok(tenant) => Json(serde_json::to_value(tenant).unwrap()).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    };
    tracing::info!(
        actor = %ctx.user_id,
        tid_chain = ?ctx.tenant_chain,
        tenant_id = %ctx.tenant_id,
        action = "tenant.update_settings",
        resource = %id,
        result = audit_result(resp.status().as_u16()),
        "ADR-0020 audit"
    );
    resp
}

async fn delete_tenant(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if !ctx.has_scope(TENANT_ADMIN_SCOPE) {
        tracing::info!(
            actor = %ctx.user_id,
            tid_chain = ?ctx.tenant_chain,
            tenant_id = %ctx.tenant_id,
            action = "tenant.delete",
            resource = %id,
            result = "forbidden",
            "ADR-0020 audit"
        );
        return forbidden("tenant:admin scope required to delete tenants");
    }
    let resp = match state.tenant_service.delete_tenant(TenantId(id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    };
    tracing::info!(
        actor = %ctx.user_id,
        tid_chain = ?ctx.tenant_chain,
        tenant_id = %ctx.tenant_id,
        action = "tenant.delete",
        resource = %id,
        result = audit_result(resp.status().as_u16()),
        "ADR-0020 audit"
    );
    resp
}
