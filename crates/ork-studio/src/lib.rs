//! ADR-0055 — Studio: local developer dashboard.
//!
//! `ork-studio` is the gateway-shaped crate that mounts the Studio SPA
//! at `/studio` and the introspection routes at `/studio/api/*`. It
//! consumes the auto-generated REST + SSE surface from
//! [`ork_api`] (ADR-0056) for chat / workflow / memory / scorer
//! reads and adds Studio-specific routes (manifest, memory aggregate,
//! scorer aggregate, evals, [501-deferred] traces + logs).
//!
//! ## Mount entry point
//!
//! [`router`] returns `Some(Router)` when [`StudioConfig`] is enabled
//! and `None` when it's [`StudioConfig::Disabled`]. The
//! [`ork_server::AxumServer`] adapter merges the returned router into
//! the auto-generated one before serving.
//!
//! ## Hexagonal placement
//!
//! Studio is at the gateway boundary alongside `ork-api` and
//! `ork-webui`. AGENTS.md §3 forbids domain crates (`ork-core`,
//! `ork-agents`, `ork-workflow`, `ork-tool`, `ork-memory`, `ork-eval`)
//! from depending on it. The acceptance criterion in ADR-0055 §2
//! makes the divergence explicit.

#![deny(rustdoc::broken_intra_doc_links)]

pub mod auth;
pub mod embed;
pub mod envelope;
pub mod routes;

pub use envelope::{STUDIO_API_VERSION, StudioEnvelope};

use std::sync::Arc;

use axum::{Extension, Router, middleware};
use ork_api::routes::auto::tenant::tenant_middleware;
use ork_app::OrkApp;
use ork_app::types::{ServerConfig, StudioConfig};

/// Build the Studio sub-router or return `None` when Studio is
/// disabled. The caller is responsible for `.merge(...)` into the
/// auto-generated router from [`ork_api::router_for`].
///
/// ADR-0055 §`Mount mechanics`: when [`StudioConfig::EnabledWithAuth`]
/// is set, the returned router enforces `Authorization: Bearer
/// <token>` on every `/studio/...` route (static assets included).
/// The token is shared via [`StudioConfig::auth()`].
#[must_use]
pub fn router(app: &OrkApp, cfg: &ServerConfig) -> Option<Router> {
    if matches!(cfg.studio, StudioConfig::Disabled) {
        return None;
    }

    let app_arc = Arc::new(app.clone());
    let cfg_arc = Arc::new(cfg.clone());

    // API routes that need an `Extension<TenantId>` (memory at least).
    // Layer the same `tenant_middleware` the auto-router uses so a
    // browser-issued `GET /studio/api/memory` doesn't 500 on a missing
    // extension when no `X-Ork-Tenant` header is present and the
    // operator has configured `ServerConfig::default_tenant`.
    let api = Router::new()
        .merge(routes::manifest::routes())
        .merge(routes::memory::routes())
        .merge(routes::scorers::routes())
        .merge(routes::evals::routes())
        .merge(routes::deferred::routes())
        .layer(middleware::from_fn(tenant_middleware));

    let mut r = Router::new().merge(api).merge(embed::routes());

    if let Some(auth) = cfg.studio.auth().cloned() {
        r = r.layer(axum::middleware::from_fn_with_state(
            Arc::new(auth),
            auth::require_studio_token,
        ));
    }

    Some(
        r.layer(Extension(Arc::clone(&app_arc)))
            .layer(Extension(Arc::clone(&cfg_arc))),
    )
}
