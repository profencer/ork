use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use ork_a2a::ContextId;
use ork_common::auth::AuthContext;
use ork_core::ports::artifact_store::{ArtifactBody, ArtifactMeta, ArtifactScope, ArtifactStore};

use axum::extract::Extension;
use serde::Deserialize;

use crate::state::WebUiState;

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// Optional A2A `context_id` to scope the blob.
    pub context_id: Option<uuid::Uuid>,
}

/// `POST /webui/api/uploads` — multipart `file` field; returns `{ "uri": "<wire ref>" }`.
pub async fn upload(
    State(state): State<WebUiState>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store: Arc<dyn ArtifactStore> = state.artifact_store.clone().ok_or((
        StatusCode::NOT_FOUND,
        "artifact storage not enabled".to_string(),
    ))?;
    let mut file_name = "upload".to_string();
    let mut data: Option<Bytes> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        if field.name() == Some("file") {
            if let Some(n) = field.file_name() {
                file_name = n.to_string();
            }
            let b = field
                .bytes()
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            if b.len() as u64 > state.max_upload_bytes {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, "file too large".to_string()));
            }
            data = Some(b);
            break;
        }
    }
    let data = data.ok_or_else(|| (StatusCode::BAD_REQUEST, "missing file field".to_string()))?;
    if data.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty file".to_string()));
    }

    let context_id = q.context_id.map(ContextId);
    let scope = ArtifactScope {
        tenant_id: auth.tenant_id,
        context_id,
    };
    let meta = ArtifactMeta {
        mime: mime_guess::from_path(&file_name)
            .first_raw()
            .map(str::to_string),
        size: data.len() as u64,
        created_at: chrono::Utc::now(),
        created_by: None,
        task_id: None,
        labels: Default::default(),
    };
    let r#ref = store
        .put(&scope, &file_name, ArtifactBody::Bytes(data), meta)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let uri = r#ref.to_wire();
    Ok(Json(serde_json::json!({ "uri": uri })))
}
