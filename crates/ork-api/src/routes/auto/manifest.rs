//! ADR-0056 §`Decision`: manifest, OpenAPI, health, and ready routes.

use std::sync::Arc;

use axum::Router;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use ork_app::OrkApp;

use crate::openapi;

/// Tenant-scoped routes: `/api/manifest` is gated behind the tenant
/// middleware. `/api/openapi.json` is *not* — it's a documentation
/// surface a browser fetches ahead of having a tenant header
/// (`auto::manifest::openapi_routes`).
pub fn routes() -> Router {
    Router::new().route("/api/manifest", get(get_manifest))
}

/// Documentation routes that must be reachable without a tenant
/// header: the OpenAPI document itself.
pub fn openapi_routes() -> Router {
    Router::new().route("/api/openapi.json", get(get_openapi))
}

/// Public routes: liveness/readiness, mounted ahead of the tenant
/// middleware so load balancers and probes can reach them without
/// needing an `X-Ork-Tenant` header.
pub fn public_routes() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

async fn get_manifest(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    Json(app.manifest())
}

async fn get_openapi(Extension(app): Extension<Arc<OrkApp>>) -> impl IntoResponse {
    Json(openapi::openapi_spec(&app))
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn readyz() -> StatusCode {
    // ADR-0056 acceptance §`Decision`: "200 OK once OrkApp::serve() is
    // ready". The router is only mounted by AxumServer after `serve()`
    // has bound, so any request reaching this handler is by definition
    // ready.
    StatusCode::OK
}
