//! ADR-0016: `GET /api/artifacts/{wire}` — JWT-auth proxy; [`ArtifactRef`]'s tenant must match the caller.
//!
//! ADR-0021 §`Vocabulary` row `artifact:<scope>:<action>`. The route-level
//! gate here is the coarse `artifact:tenant:read` check; the per-context
//! `artifact:context-<id>:read` enforcement lives inside the
//! `ScopeCheckedArtifactStore` wrapper (ADR-0021 §`ArtifactStore boundary`).

use std::io;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use futures::StreamExt;
use ork_common::auth::artifact_scope;
use ork_common::error::OrkError;
use ork_core::ports::artifact_store::{ArtifactBody, ArtifactRef, ArtifactStore};
use ork_storage::ScopeCheckedArtifactStore;

use crate::middleware::AuthContext;
use crate::require_scope;
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
    // ADR-0021 §`Vocabulary` row `artifact:<scope>:<action>`. Tenant-wide
    // read is the route-level gate; per-`context-<id>` narrowing (for
    // tokens that lack the broad tenant read) is a follow-up wired
    // through the `ScopeCheckedArtifactStore` decorator.
    require_scope!(auth, artifact_scope("tenant", "read"));
    let Some(raw_store) = state.artifact_store.as_ref() else {
        return (StatusCode::NOT_FOUND, "artifacts not configured").into_response();
    };
    // ADR-0021 §`Decision points` step 4: the wrapper enforces
    // `artifact:<scope>:<action>` per call. Built per request from the
    // caller's scope set so a long-lived `state.artifact_store` cannot
    // accidentally carry the wrong scope.
    let store: Arc<dyn ArtifactStore> = Arc::new(ScopeCheckedArtifactStore::new(
        raw_store.clone(),
        auth.scopes.clone(),
    ));
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
        Err(OrkError::Forbidden(msg)) => {
            return (StatusCode::FORBIDDEN, msg).into_response();
        }
        Err(_) => {
            return (StatusCode::NOT_FOUND, "artifact not found").into_response();
        }
    };
    let body = match store.get(&aref).await {
        Ok(b) => b,
        Err(OrkError::Forbidden(msg)) => {
            return (StatusCode::FORBIDDEN, msg).into_response();
        }
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
