//! Forward `message/stream` JSON-RPC to the public A2A URL (same process in dev).

use std::io;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use ork_common::auth::AuthContext;
use serde::Deserialize;
use uuid::Uuid;

use axum::extract::Extension;

use crate::state::WebUiState;

#[derive(Debug, Deserialize)]
pub struct PostMessageBody {
    pub agent_id: String,
    /// JSON-RPC envelope for `message/stream` (method, id, params).
    pub jsonrpc: serde_json::Value,
}

/// `POST /webui/api/conversations/{id}/messages` — proxy to `POST /a2a/agents/{agent_id}`.
pub async fn post_conversation_message(
    State(state): State<WebUiState>,
    Extension(ctx): Extension<AuthContext>,
    Path(conv_id): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<PostMessageBody>,
) -> Result<Response, (StatusCode, String)> {
    if state.a2a_public_base.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "set `a2a_public_base` in [[gateways]] config for type=webui or ORK_A2A_PUBLIC_BASE"
                .to_string(),
        ));
    }
    let c = state
        .webui
        .get_conversation(ctx.tenant_id, conv_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if c.is_none() {
        return Err((StatusCode::NOT_FOUND, "unknown conversation".to_string()));
    }

    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                "missing Authorization".to_string(),
            )
        })?;

    let base = state.a2a_public_base.trim_end_matches('/');
    let url = format!(
        "{}/a2a/agents/{}",
        base,
        urlencoding::encode(&body.agent_id)
    );

    let mut req = state
        .http
        .post(url)
        .header(axum::http::header::AUTHORIZATION, auth)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .json(&body.jsonrpc);

    if let Some(h) = headers.get("X-Tenant-Id") {
        req = req.header("X-Tenant-Id", h);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(|e| io::Error::other(e.to_string())));
    let body = Body::from_stream(stream);
    let mut res = Response::new(body);
    *res.status_mut() = status;
    if let Some(ct) = ct
        && let Ok(h) = axum::http::HeaderValue::from_str(&ct)
    {
        res.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, h);
    }
    Ok(res)
}
