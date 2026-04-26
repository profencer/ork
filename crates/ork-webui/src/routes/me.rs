use axum::Json;
use axum::extract::Extension;
use ork_common::auth::AuthContext;

/// `GET /webui/api/me` — echo JWT-derived identity (ADR-0017).
pub async fn get_me(Extension(ctx): Extension<AuthContext>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "user_id": ctx.user_id,
        "tenant_id": ctx.tenant_id.0,
        "scopes": ctx.scopes,
    }))
}
