use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use ork_common::auth::AuthContext;
use ork_common::error::OrkError;
use serde::Deserialize;
use uuid::Uuid;

use axum::extract::Extension;

use crate::state::WebUiState;

#[derive(Debug, Deserialize)]
pub struct CreateProjectBody {
    pub label: String,
}

/// `GET /webui/api/projects`
pub async fn list_projects(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let projects = state
        .webui
        .list_projects(ctx.tenant_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(&projects).unwrap_or_default()))
}

/// `POST /webui/api/projects`
pub async fn create_project(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<CreateProjectBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.label.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "label required".to_string()));
    }
    let p = state
        .webui
        .create_project(ctx.tenant_id, &body.label)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(p).unwrap_or_default()))
}

/// `DELETE /webui/api/projects/{id}`
pub async fn delete_project(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .webui
        .delete_project(ctx.tenant_id, id)
        .await
        .map_err(|e| match e {
            OrkError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        })?;
    Ok(StatusCode::NO_CONTENT)
}
