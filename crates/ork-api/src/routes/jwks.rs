//! Public JWKS endpoint (ADR-0009 §`Signing — JWS over the payload`).
//!
//! Subscribers verifying the `X-A2A-Signature` header on inbound push
//! notifications fetch the active public keys here. The endpoint always
//! reflects the in-memory snapshot held by [`ork_push::JwksProvider`], which is
//! refreshed every time `rotate_if_due` flips the current signer (and once
//! per `refresh()` cycle from the boot loop).
//!
//! No auth: per ADR-0009, JWKS is mesh-wide and tenant-agnostic.

use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Router, response::Response};

use crate::state::AppState;

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/.well-known/jwks.json", get(jwks_handler))
        .with_state(state)
}

async fn jwks_handler(State(state): State<AppState>) -> Response {
    let body = state.jwks_provider.jwks().await;
    // RFC 7517 §8.5 advises `application/jwk-set+json`; downstream clients
    // accept `application/json` as well so we set both headers.
    (
        [
            (header::CONTENT_TYPE, "application/jwk-set+json"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        Json(body),
    )
        .into_response()
}
