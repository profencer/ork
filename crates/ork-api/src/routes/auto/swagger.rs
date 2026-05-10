//! ADR-0056 §`Decision`: `/swagger-ui` static page pointing at
//! `/api/openapi.json`.
//!
//! v1 ships a tiny inline HTML page that pulls the Swagger UI bundle
//! from a CDN. Production deployments wanting a fully self-hosted UI
//! can layer their own router via [`crate::router_for::router_for`]
//! and skip this one (`ServerConfig::swagger_ui = false`).

use axum::Router;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

const SWAGGER_HTML: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>ork API — Swagger UI</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5.17.14/swagger-ui.css" />
    <style>body { margin: 0 } #ork { padding: 0 }</style>
  </head>
  <body>
    <div id="ork"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5.17.14/swagger-ui-bundle.js" crossorigin></script>
    <script>
      window.addEventListener('load', function () {
        window.ui = SwaggerUIBundle({
          url: '/api/openapi.json',
          dom_id: '#ork',
          deepLinking: true,
        });
      });
    </script>
  </body>
</html>"#;

pub fn routes() -> Router {
    Router::new().route("/swagger-ui", get(swagger_index))
}

async fn swagger_index() -> Response {
    let mut resp = Html(SWAGGER_HTML).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        "public, max-age=300".parse().unwrap(),
    );
    resp
}
