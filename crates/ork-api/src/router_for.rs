//! ADR-0056 §`Decision`: composer that walks `OrkApp::manifest()` and
//! produces an [`axum::Router`] with the auto-generated REST + SSE
//! routes.
//!
//! ## Coexistence with the legacy A2A surface
//!
//! [`OrkApp::serve()`](ork_app::OrkApp::serve) mounts only the auto
//! surface produced here. The existing A2A endpoints
//! ([`crate::routes::create_router_with_gateways`]) require a heavy
//! [`crate::state::AppState`] (Postgres, Redis, Kafka, push outbox)
//! that the registry-only [`OrkApp`] does not own. Deployments that
//! need both surfaces compose them in `main.rs`:
//!
//! ```ignore
//! let auto = ork_api::router_for(&app, &cfg);
//! let legacy = ork_api::routes::create_router_with_gateways(state, ...);
//! let combined = auto.merge(legacy);
//! axum::serve(listener, combined).await?;
//! ```
//!
//! ADR-0056 §`Server adapters` documents this seam; reviewer finding
//! C1 tracks the follow-up to make the merge automatic once
//! `OrkAppBuilder` exposes `AppState` wiring.
//!
//! ## Middleware order (ADR-0056 §`Auth and tenant scoping`)
//!
//! Tower applies layers outside-in. We layer in this order so the
//! request flow is `auth → tenant → handler`:
//!
//! 1. `auth_middleware` (when `cfg.auth.is_some()`) — JWT → `AuthContext`.
//! 2. `tenant_middleware` — `X-Ork-Tenant` header parse, *plus* a
//!    consistency check against `AuthContext::tenant_id` so a
//!    cross-tenant header without admin scope is rejected.
//! 3. handler — reads `Extension<TenantId>` and `Extension<AuthContext>`.

use std::sync::Arc;

use axum::{Extension, Router, middleware};
use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::middleware::auth_middleware;
use crate::routes::auto;

/// Build the auto-generated router for `app`. See the module-level
/// docs for the layer ordering and the host-merge composition.
pub fn router_for(app: &OrkApp, cfg: &ServerConfig) -> Router {
    let app_arc = Arc::new(app.clone());
    let cfg_arc = Arc::new(cfg.clone());

    let mut tenant_scoped = Router::new()
        .merge(auto::manifest::routes())
        .merge(auto::agents::routes())
        .merge(auto::workflows::routes())
        .merge(auto::tools::routes())
        .merge(auto::memory::routes())
        .merge(auto::scorers::routes());

    // Order matters here. Tower applies layers outside-in: the LAST
    // `.layer(...)` call wraps OUTERMOST and runs FIRST. We want
    // auth → tenant → handler, so we layer in reverse: tenant first,
    // auth second.
    tenant_scoped = tenant_scoped.layer(middleware::from_fn(auto::tenant::tenant_middleware));
    if cfg.auth.is_some() {
        tenant_scoped = tenant_scoped.layer(middleware::from_fn(auth_middleware));
    }

    let public = auto::manifest::public_routes();
    // ADR-0056 §`Decision`: `/swagger-ui` and `/api/openapi.json` are
    // documentation surfaces. Browsers cannot send `X-Ork-Tenant`
    // ahead of loading a page, so they must sit on `public_routes`
    // ahead of `tenant_middleware`. They still consult
    // `Extension<Arc<OrkApp>>` for the live manifest walk.
    let mut docs = Router::new();
    if cfg.swagger_ui {
        docs = docs.merge(auto::swagger::routes());
    }
    docs = docs.merge(auto::manifest::openapi_routes());

    Router::new()
        .merge(public)
        .merge(docs)
        .merge(tenant_scoped)
        .layer(Extension(Arc::clone(&app_arc)))
        .layer(Extension(Arc::clone(&cfg_arc)))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
}
