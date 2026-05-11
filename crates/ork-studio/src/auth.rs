//! ADR-0055 §`Authentication`: bearer-token gate for the Studio routes
//! when `StudioConfig::EnabledWithAuth` is configured. The token is
//! generated on `ork dev` boot and printed once to the operator's
//! console.
//!
//! v1 scope: this gate is the *only* auth on `/studio/...`. JWT-backed
//! production auth for Studio (SSO, mTLS, et al.) is owned by ADR-0020
//! and is explicitly out of scope here.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use ork_app::types::StudioAuth;

/// Tower middleware: 401s when the request lacks
/// `Authorization: Bearer <token>` or the token does not match.
pub async fn require_studio_token(
    State(auth): State<Arc<StudioAuth>>,
    req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(token) = presented else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "studio_api_version": super::STUDIO_API_VERSION,
                "error": "missing Authorization: Bearer <token>",
            })),
        )
            .into_response();
    };

    if !auth.matches(token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "studio_api_version": super::STUDIO_API_VERSION,
                "error": "invalid studio token",
            })),
        )
            .into_response();
    }

    next.run(req).await
}
