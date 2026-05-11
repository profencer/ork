//! ADR-0055 §`Mount mechanics`: serve the embedded Vite + React bundle
//! under `/studio/`.
//!
//! `web/dist/` is bundled into the binary via `rust-embed` (per the
//! ADR — diverging from `ork-webui`'s `include_dir` pattern). At dev
//! time `web/dist/` may not exist; the embed gracefully returns
//! `404` for asset requests and a fallback HTML shell for `/studio/`
//! so the routes table still mounts without a frontend build.

use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

#[derive(rust_embed::Embed)]
#[folder = "web/dist/"]
struct StudioAssets;

const FALLBACK_INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Studio (no bundle)</title>
</head>
<body>
  <div id="root" data-state="missing-bundle">
    <h1>Studio bundle not built</h1>
    <p>
      The <code>ork-studio</code> crate is mounted, but
      <code>crates/ork-studio/web/dist/</code> is empty. Run
      <code>pnpm install --frozen-lockfile &amp;&amp; pnpm build</code>
      inside <code>crates/ork-studio/web/</code>, or run
      <code>ork build</code> which orchestrates it.
    </p>
  </div>
</body>
</html>"#;

pub fn routes() -> Router {
    // ADR-0055 §`Mount mechanics`: index at `/studio/`, hashed asset
    // paths under `/studio/assets/*`, and SPA fallback for every other
    // `/studio/...` so client-side routing works.
    Router::new()
        .route("/studio", get(serve_index))
        .route("/studio/", get(serve_index))
        .route("/studio/{*path}", get(serve_path))
}

async fn serve_index() -> Response {
    serve_named("index.html").await
}

async fn serve_path(Path(path): Path<String>) -> Response {
    // Reject path-traversal attempts. `rust-embed` already keys by
    // exact paths so `..` lookups return `None`, but being explicit
    // surfaces a clear 400. Reviewer m6: segment-based check so a
    // legitimate hash-suffix asset like `assets/foo..bar.css` isn't
    // rejected (the substring guard was over-broad).
    if path.split('/').any(|seg| seg == "..") {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    if let Some(resp) = try_serve(&path).await {
        return resp;
    }
    // SPA client-side routing: any unknown `/studio/...` URL falls
    // back to `index.html` so React-Router can hydrate the route.
    serve_named("index.html").await
}

async fn try_serve(path: &str) -> Option<Response> {
    let asset = StudioAssets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let body = Body::from(asset.data.into_owned());
    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(body)
            .unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "embed response build").into_response()
            }),
    )
}

async fn serve_named(name: &str) -> Response {
    if let Some(resp) = try_serve(name).await {
        return resp;
    }
    // `web/dist/` may be empty when running tests or a fresh checkout
    // without `pnpm build`. Return the in-binary fallback shell so the
    // route still answers `200` (rather than a confusing `404`).
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(FALLBACK_INDEX))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "fallback response build").into_response()
        })
}
