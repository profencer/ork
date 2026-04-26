//! `GET /` and static file serving: embedded `dist/` (feature `embed-spa`) or `WEBUI_DEV_PROXY` passthrough.
//!
//! ADR-0017: when neither is available, return 503 with operator hints.

use std::io;
use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;

static DEV_PROXY_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[cfg(feature = "embed-spa")]
static SPA: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../client/webui/frontend/dist");

fn dev_proxy_base() -> Option<String> {
    let v = std::env::var("WEBUI_DEV_PROXY").ok()?;
    let t = v.trim();
    if t.is_empty() {
        return None;
    }
    let base = t.trim_end_matches('/').to_string();
    Some(base)
}

/// Plain-text hint when the SPA is unavailable.
async fn service_unavailable_hint() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Web UI: build `client/webui/frontend` with `pnpm build`, then rebuild with \
         `cargo build -p ork-webui --features embed-spa` (or set `WEBUI_DEV_PROXY` to a Vite dev URL, \
         e.g. `http://127.0.0.1:5173` — see `ork webui dev` and `demo/README.md`). \
         JSON API: `/webui/api/*` (JWT).\n",
    )
}

/// Forward browser requests to the Vite dev server (or any HTTP origin).
async fn dev_proxy_get(uri: Uri) -> Response {
    let Some(base) = dev_proxy_base() else {
        return service_unavailable_hint().await.into_response();
    };

    let path = uri.path();
    let mut dest = String::new();
    dest.push_str(&base);
    dest.push_str(path);
    if let Some(q) = uri.query() {
        dest.push('?');
        dest.push_str(q);
    }

    let client = DEV_PROXY_CLIENT.get_or_init(reqwest::Client::new);
    let Ok(resp) = client.get(&dest).send().await else {
        return (StatusCode::BAD_GATEWAY, "upstream unreachable").into_response();
    };

    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(|e| io::Error::other(e.to_string())));
    let body = Body::from_stream(stream);
    let mut res = Response::new(body);
    *res.status_mut() = status;
    if let Some(ct) = ct
        && let Ok(h) = axum::http::HeaderValue::from_str(&ct)
    {
        res.headers_mut().insert(header::CONTENT_TYPE, h);
    }
    res
}

#[cfg(feature = "embed-spa")]
fn embed_response(path: &str) -> Option<Response> {
    let rel = path.trim_start_matches('/');
    let (bytes, guess_path): (&'static [u8], &str) = if rel.is_empty() || rel == "index.html" {
        let f = SPA.get_file("index.html")?;
        (f.contents(), "index.html")
    } else if let Some(f) = SPA.get_file(rel) {
        (f.contents(), rel)
    } else if rel.starts_with("assets/") {
        // Missing hashed asset: avoid SPA fallback hiding 404s.
        return None;
    } else {
        let f = SPA.get_file("index.html")?;
        (f.contents(), "index.html")
    };
    let mime = mime_guess::from_path(guess_path)
        .first_or_octet_stream()
        .to_string();
    let mut res = Response::new(Body::from(bytes));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_str(&mime).ok()?,
    );
    *res.status_mut() = StatusCode::OK;
    Some(res)
}

#[cfg(feature = "embed-spa")]
async fn embed_get(uri: Uri) -> Response {
    let path = uri.path();
    if let Some(r) = embed_response(path) {
        return r;
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(not(feature = "embed-spa"))]
async fn embed_get(_uri: Uri) -> Response {
    service_unavailable_hint().await.into_response()
}

/// Resolve `GET` for static HTML/JS and dev proxy: env wins over embed, then unconfigured embed path.
async fn public_get(uri: Uri) -> Response {
    if dev_proxy_base().is_some() {
        return dev_proxy_get(uri).await;
    }
    embed_get(uri).await
}

/// Routes merged without JWT (browser loads `/` before login).
pub fn public_routes() -> Router {
    Router::new()
        .route("/", get(public_get))
        .route("/{*path}", get(public_get))
}
