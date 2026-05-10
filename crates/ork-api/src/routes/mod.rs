pub mod a2a;
pub mod artifacts;
pub mod auto;
pub mod health;
pub mod jwks;
pub mod tenants;
pub mod webhooks;
pub mod workflows;

use std::sync::Arc;

use axum::{Extension, Router, middleware};
use ork_security::MeshTokenSigner;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::middleware::{RuntimeEnv, auth_middleware};
use crate::state::AppState;

pub fn create_router(state: AppState) -> Router {
    create_router_with_gateways(state, Router::new(), Router::new(), None)
}

/// Same as [`create_router`], but merges gateway routes: `gateway_routes` on the public
/// side (each gateway may terminate its own auth), `gateway_protected` with [`auth_middleware`]
/// (ADR-0017 Web UI). `mesh_signer` plumbs the optional ADR-0020 mesh-token verifier into
/// `auth_middleware` via a request extension; pass `None` for legacy / dev deployments.
pub fn create_router_with_gateways(
    state: AppState,
    gateway_routes: Router,
    gateway_protected: Router,
    mesh_signer: Option<Arc<dyn MeshTokenSigner>>,
) -> Router {
    // ADR-0020 §`Edge trust`: the runtime env selector (canonical source is
    // `state.config.env`, populated from TOML + `ORK__ENV` env var) drives
    // the once-per-process Kong-headers warning in `auth_middleware`. Carry
    // it via a request extension so the middleware doesn't have to re-read
    // `std::env::var` (which misses the case where `env` is set in the TOML
    // file but the env var is not exported to the process).
    let runtime_env = RuntimeEnv(state.config.env.clone());
    let public_routes = Router::new()
        .merge(health::routes())
        .merge(webhooks::routes(state.clone()))
        .merge(gateway_routes)
        // ADR-0005: agent cards are public. JSON-RPC / SSE / push lands with ADR-0008.
        .merge(a2a::well_known_routes(state.clone()))
        // ADR-0009 §`Signing`: subscribers fetch active public keys here.
        .merge(jwks::routes(state.clone()));

    let mut protected_routes = Router::new()
        .merge(artifacts::routes(state.clone()))
        .merge(tenants::routes(state.clone()))
        .merge(workflows::routes(state.clone()))
        .merge(gateway_protected)
        // ADR-0008: A2A JSON-RPC dispatcher, SSE bridge, and convenience
        // endpoints all live behind the same auth middleware.
        .merge(a2a::protected_routes(state.clone()))
        .layer(middleware::from_fn(auth_middleware))
        .layer(Extension(runtime_env));
    // ADR-0020 §`Mesh trust`: hand the signer to `auth_middleware` via a
    // request extension. Layered AFTER `from_fn(auth_middleware)` so the
    // extension is present when the middleware runs (axum applies layers
    // outside-in).
    if let Some(signer) = mesh_signer {
        protected_routes = protected_routes.layer(Extension(signer));
    }

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        // Rate limiting is owned by Kong per ADR-0004
        // (`docs/adrs/0004-hybrid-kong-kafka-transport.md`).
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
}
