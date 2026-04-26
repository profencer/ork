//! HTTP routes merged into `ork-api`'s protected stack (JWT middleware).

mod agents;
mod conversations;
mod me;
mod messages;
mod projects;
mod uploads;

use axum::Router;
use axum::routing::{delete, get, post};

use crate::state::WebUiState;

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
        .with_state(state)
}
