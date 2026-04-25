pub mod a2a;
pub mod health;
pub mod jwks;
pub mod tenants;
pub mod webhooks;
pub mod workflows;

use axum::{Router, middleware};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::middleware::auth_middleware;
use crate::state::AppState;

pub fn create_router(state: AppState) -> Router {
    create_router_with_gateways(state, Router::new())
}

/// Same as [`create_router`], but merges `gateway_routes` on the public side (each gateway terminates its own auth).
pub fn create_router_with_gateways(state: AppState, gateway_routes: Router) -> Router {
    let public_routes = Router::new()
        .merge(health::routes())
        .merge(webhooks::routes(state.clone()))
        .merge(gateway_routes)
        // ADR-0005: agent cards are public. JSON-RPC / SSE / push lands with ADR-0008.
        .merge(a2a::well_known_routes(state.clone()))
        // ADR-0009 §`Signing`: subscribers fetch active public keys here.
        .merge(jwks::routes(state.clone()));

    let protected_routes = Router::new()
        .merge(tenants::routes(state.clone()))
        .merge(workflows::routes(state.clone()))
        // ADR-0008: A2A JSON-RPC dispatcher, SSE bridge, and convenience
        // endpoints all live behind the same auth middleware.
        .merge(a2a::protected_routes(state.clone()))
        .layer(middleware::from_fn(auth_middleware));

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        // Rate limiting is owned by Kong per ADR-0004
        // (`docs/adrs/0004-hybrid-kong-kafka-transport.md`).
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
}
