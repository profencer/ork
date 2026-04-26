use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use ork_a2a::ContextId;
use ork_common::auth::AuthContext;
use serde::Deserialize;
use uuid::Uuid;

use axum::extract::Extension;

use crate::state::WebUiState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct CreateConversationBody {
    pub project_id: Option<Uuid>,
    pub context_id: Uuid,
    #[serde(default)]
    pub label: String,
}

/// `GET /webui/api/conversations`
pub async fn list_conversations(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let v = state
        .webui
        .list_conversations(ctx.tenant_id, q.project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(v).unwrap_or_default()))
}

/// `POST /webui/api/conversations`
pub async fn create_conversation(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<CreateConversationBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let c = state
        .webui
        .create_conversation(
            ctx.tenant_id,
            body.project_id,
            ContextId(body.context_id),
            if body.label.is_empty() {
                "chat"
            } else {
                &body.label
            },
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(c).unwrap_or_default()))
}

/// `GET /webui/api/conversations/{id}` (lookup)
pub async fn get_conversation(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let c = state
        .webui
        .get_conversation(ctx.tenant_id, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(c) = c else {
        return Err((StatusCode::NOT_FOUND, "unknown conversation".to_string()));
    };
    Ok(Json(serde_json::to_value(c).unwrap_or_default()))
}
