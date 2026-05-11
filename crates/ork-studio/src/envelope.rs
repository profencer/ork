//! ADR-0055 §`Studio API (introspection-only)`: every JSON response on
//! `/studio/api/*` is wrapped in a versioned envelope so the SPA can
//! detect a server/bundle mismatch and render a "your Studio bundle is
//! older than the server" banner.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Version stamped onto every `/studio/api/*` JSON response. Bumped on
/// breaking changes only.
pub const STUDIO_API_VERSION: u32 = 1;

/// Wrapper applied to every `/studio/api/*` JSON response.
///
/// ```json
/// { "studio_api_version": 1, "data": { ... } }
/// ```
#[derive(Debug, Serialize)]
pub struct StudioEnvelope<T: Serialize> {
    pub studio_api_version: u32,
    pub data: T,
}

impl<T: Serialize> StudioEnvelope<T> {
    pub fn new(data: T) -> Self {
        Self {
            studio_api_version: STUDIO_API_VERSION,
            data,
        }
    }
}

impl<T: Serialize> IntoResponse for StudioEnvelope<T> {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(self)).into_response()
    }
}

/// Convenience: wrap `data` and respond with `200 OK`.
pub fn ok<T: Serialize>(data: T) -> StudioEnvelope<T> {
    StudioEnvelope::new(data)
}
