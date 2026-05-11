//! `GET /studio/api/manifest` — Studio Overview panel data source.
//!
//! Returns the same [`AppManifest`](ork_app::AppManifest) that ADR-0049
//! exposes on `/api/manifest`, wrapped in the Studio envelope. The
//! Overview panel renders the registered agents, workflows, tools, MCP
//! servers, and memory backend at a glance.

use std::sync::Arc;

use axum::{Extension, Router, routing::get};
use ork_app::OrkApp;

use crate::envelope::{StudioEnvelope, ok};

pub fn routes() -> Router {
    Router::new().route("/studio/api/manifest", get(get_manifest))
}

async fn get_manifest(
    Extension(app): Extension<Arc<OrkApp>>,
) -> StudioEnvelope<ork_app::AppManifest> {
    ok(app.manifest())
}
