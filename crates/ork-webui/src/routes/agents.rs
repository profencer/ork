use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::state::WebUiState;

/// JSON for the agent picker: `id` is the A2A path segment; `name` is display-only.
#[derive(Serialize)]
struct WebuiAgentRow {
    id: String,
    name: String,
    description: String,
    version: String,
}

/// `GET /webui/api/agents` — local + remote agents with registry `id` for `POST .../messages`.
pub async fn list_agents(State(state): State<WebUiState>) -> impl axum::response::IntoResponse {
    let rows: Vec<WebuiAgentRow> = state
        .agent_registry
        .list_id_cards()
        .await
        .into_iter()
        .map(|(id, c)| WebuiAgentRow {
            id,
            name: c.name,
            description: c.description,
            version: c.version,
        })
        .collect();
    let mut res = Json(rows).into_response();
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("max-age=5"),
    );
    res
}
