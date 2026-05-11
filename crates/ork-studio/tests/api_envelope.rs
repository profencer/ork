//! ADR-0055 AC #7 + §`Studio API (introspection-only)`: every JSON
//! response on `/studio/api/*` carries the versioned envelope
//! `{ "studio_api_version": 1, "data": ... }`. The SPA uses the version
//! to detect a server/bundle mismatch and render an upgrade banner.
//!
//! This contract test boots a minimal `OrkApp`, mounts the Studio
//! router, and asserts every successful introspection route (plus
//! the deferred Traces/Logs panels' 501 envelopes) carries the
//! `studio_api_version` field.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ork_app::OrkApp;
use ork_app::types::{ServerConfig, StudioConfig};
use tower::ServiceExt;

fn make_app() -> OrkApp {
    OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            studio: StudioConfig::Enabled,
            ..ServerConfig::default()
        })
        .build()
        .expect("build app")
}

fn router(app: &OrkApp) -> axum::Router {
    // ork-studio now layers `tenant_middleware` on its API routes so
    // the SPA can call them without an `X-Ork-Tenant` header when the
    // operator has configured `ServerConfig::default_tenant`. Mirror
    // that contract here: build the cfg with a pinned default tenant.
    ork_studio::router(app, &app.manifest_server_config()).expect("studio enabled")
}

#[tokio::test]
async fn manifest_envelope() {
    let app = make_app();
    let resp = router(&app)
        .oneshot(
            Request::builder()
                .uri("/studio/api/manifest")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(
        json.get("studio_api_version").and_then(|v| v.as_u64()),
        Some(u64::from(ork_studio::STUDIO_API_VERSION))
    );
    assert!(json.get("data").is_some(), "missing `data`: {json}");
}

#[tokio::test]
async fn scorers_envelope() {
    let app = make_app();
    for path in ["/studio/api/scorers", "/studio/api/scorers/aggregate"] {
        let resp = router(&app)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json.get("studio_api_version").and_then(|v| v.as_u64()),
            Some(u64::from(ork_studio::STUDIO_API_VERSION)),
            "{path}: missing studio_api_version"
        );
    }
}

#[tokio::test]
async fn deferred_traces_envelope() {
    let app = make_app();
    let resp = router(&app)
        .oneshot(
            Request::builder()
                .uri("/studio/api/traces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(
        json.get("studio_api_version").and_then(|v| v.as_u64()),
        Some(u64::from(ork_studio::STUDIO_API_VERSION))
    );
    assert_eq!(
        json.get("deferred_to").and_then(|v| v.as_str()),
        Some("observability-adr")
    );
}

#[tokio::test]
async fn deferred_logs_envelope() {
    let app = make_app();
    let resp = router(&app)
        .oneshot(
            Request::builder()
                .uri("/studio/api/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(
        json.get("studio_api_version").and_then(|v| v.as_u64()),
        Some(u64::from(ork_studio::STUDIO_API_VERSION))
    );
}

#[tokio::test]
async fn studio_disabled_returns_no_router() {
    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            studio: StudioConfig::Disabled,
            ..ServerConfig::default()
        })
        .build()
        .expect("build app");
    let cfg = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        studio: StudioConfig::Disabled,
        ..ServerConfig::default()
    };
    assert!(
        ork_studio::router(&app, &cfg).is_none(),
        "Disabled must return None so production builds pay no cost"
    );
}

// ---- Test helpers --------------------------------------------------

trait OrkAppTestExt {
    /// The `OrkApp` doesn't expose its `ServerConfig` directly; the
    /// manifest carries the listen host/port summary but not the
    /// full struct. For contract tests we reuse the same shape we
    /// passed to the builder; production code reads it from the
    /// `Arc<ServerConfig>` extension installed by `router_for`.
    fn manifest_server_config(&self) -> ServerConfig;
}

impl OrkAppTestExt for OrkApp {
    fn manifest_server_config(&self) -> ServerConfig {
        let m = self.manifest();
        ServerConfig {
            host: m.server.host.clone(),
            port: m.server.port,
            studio: StudioConfig::Enabled,
            default_tenant: Some("11111111-1111-1111-1111-111111111111".into()),
            ..ServerConfig::default()
        }
    }
}

