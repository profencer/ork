use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
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

async fn create_tenant(
    State(state): State<AppState>,
    Json(req): Json<CreateTenantRequest>,
) -> impl IntoResponse {
    match state.tenant_service.create_tenant(&req).await {
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
    }
}

async fn list_tenants(State(state): State<AppState>) -> impl IntoResponse {
    match state.tenant_service.list_tenants().await {
        Ok(tenants) => Json(serde_json::to_value(tenants).unwrap()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_tenant(State(state): State<AppState>, Path(id): Path<Uuid>) -> impl IntoResponse {
    match state.tenant_service.get_tenant(TenantId(id)).await {
        Ok(tenant) => Json(serde_json::to_value(tenant).unwrap()).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn update_tenant_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateTenantSettingsRequest>,
) -> impl IntoResponse {
    if ctx.tenant_id.0 != id {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "cannot modify another tenant's settings" })),
        )
            .into_response();
    }

    match state
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
    }
}

async fn delete_tenant(State(state): State<AppState>, Path(id): Path<Uuid>) -> impl IntoResponse {
    match state.tenant_service.delete_tenant(TenantId(id)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
