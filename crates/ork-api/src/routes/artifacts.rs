//! ADR-0016: `GET /api/artifacts/{wire}` — JWT-auth proxy; [`ArtifactRef`]'s tenant must match the caller.
//!
// ADR-0021: add fine-grained artifact ACLs (user-level) here.

use std::io;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use futures::StreamExt;
use ork_core::ports::artifact_store::{ArtifactBody, ArtifactRef};

use crate::middleware::AuthContext;
use crate::state::AppState;

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/api/artifacts/{*wire}", get(get_artifact))
        .with_state(state)
}

async fn get_artifact(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(wire): Path<String>,
) -> Response {
    let Some(store) = state.artifact_store.as_ref() else {
        return (StatusCode::NOT_FOUND, "artifacts not configured").into_response();
    };
    let wire = match urlencoding::decode(&wire) {
        Ok(c) => c.into_owned(),
        Err(_) => wire,
    };
    let aref = match ArtifactRef::parse(&wire) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    };
    if aref.tenant_id != auth.tenant_id {
        return (StatusCode::FORBIDDEN, "tenant mismatch").into_response();
    }
    let head = match store.head(&aref).await {
        Ok(h) => h,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "artifact not found").into_response();
        }
    };
    let body = match store.get(&aref).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "artifact not found").into_response();
        }
    };
    let frame = match body {
        ArtifactBody::Bytes(b) => Body::from(b),
        ArtifactBody::Stream(s) => {
            let mapped = s.map(|r| r.map_err(|e| io::Error::other(e.to_string())));
            Body::from_stream(mapped)
        }
    };
    let mut res = Response::new(frame);
    *res.status_mut() = StatusCode::OK;
    if let Some(ct) = &head.mime
        && let Ok(hv) = ct.parse()
    {
        res.headers_mut().insert(header::CONTENT_TYPE, hv);
    }
    res
}
