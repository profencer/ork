//! HTTP routes merged into `ork-api`'s protected stack (JWT middleware).
//!
//! ADR-0021 §`Vocabulary` row `webui:access` gates every route in this
//! module — applied as a single `from_fn` middleware at the router level
//! so handlers don't have to repeat the `require_scope!` call.

mod agents;
mod conversations;
mod me;
mod messages;
mod projects;
mod uploads;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ork_common::auth::AuthContext;

use crate::state::WebUiState;

/// ADR-0021 audit + 403 wrapper. Reads [`AuthContext`] from request
/// extensions (placed by `ork-api::middleware::auth_middleware` upstream)
/// and rejects anything that isn't carrying `webui:access`.
async fn require_webui_access(req: Request, next: Next) -> Response {
    let allowed = req
        .extensions()
        .get::<AuthContext>()
        .is_some_and(|ctx| ctx.scopes.iter().any(|s| s == "webui:access"));
    if !allowed {
        if let Some(ctx) = req.extensions().get::<AuthContext>() {
            tracing::info!(
                actor = ?ctx.user_id,
                tenant_id = %ctx.tenant_id,
                tid_chain = ?ctx.tenant_chain,
                scope = "webui:access",
                result = "forbidden",
                event = ork_security::audit::SCOPE_DENIED_EVENT,
                "ADR-0021 audit"
            );
        }
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "missing scope webui:access" })),
        )
            .into_response();
    }
    next.run(req).await
}

/// Routes that require `Authorization: Bearer` and expose [`ork_common::auth::AuthContext`].
pub fn protected_routes(state: WebUiState) -> Router {
    Router::new()
        .route("/webui/api/me", get(me::get_me))
        .route("/webui/api/agents", get(agents::list_agents))
        .route(
            "/webui/api/projects",
            get(projects::list_projects).post(projects::create_project),
        )
        .route("/webui/api/projects/{id}", delete(projects::delete_project))
        .route(
            "/webui/api/conversations",
            get(conversations::list_conversations).post(conversations::create_conversation),
        )
        .route(
            "/webui/api/conversations/{id}",
            get(conversations::get_conversation),
        )
        .route(
            "/webui/api/conversations/{id}/messages",
            post(messages::post_conversation_message),
        )
        .route("/webui/api/uploads", post(uploads::upload))
        .layer(from_fn(require_webui_access))
        .with_state(state)
}
