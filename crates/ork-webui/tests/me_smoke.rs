//! `GET /webui/api/me` with injected auth (mirrors `auth_middleware` output).

use axum::{body::Body, extract::Request, http::StatusCode, middleware::Next, response::Response};
use ork_common::auth::AuthContext;
use ork_common::types::TenantId;
use ork_webui::WebUiState;
use ork_webui::routes::protected_routes;
use tower::ServiceExt;
use uuid::Uuid;

async fn inject_auth(mut req: Request, next: Next) -> Response {
    req.extensions_mut().insert(AuthContext {
        tenant_id: TenantId(Uuid::now_v7()),
        user_id: "tester".to_string(),
        // ADR-0021 §`Vocabulary` row `webui:access` gates the entire
        // webui surface; tests minted before ADR-0021 carried only the
        // legacy `a2a:send` shape, which is no longer enough.
        scopes: vec!["webui:access".to_string(), "a2a:send".to_string()],
        tenant_chain: Vec::new(),
        trust_tier: ork_common::auth::TrustTier::default(),
        trust_class: ork_common::auth::TrustClass::default(),
        agent_id: None,
    });
    next.run(req).await
}

async fn inject_auth_without_webui_access(mut req: Request, next: Next) -> Response {
    req.extensions_mut().insert(AuthContext {
        tenant_id: TenantId(Uuid::now_v7()),
        user_id: "tester".to_string(),
        // No `webui:access` → router-level gate must reject.
        scopes: vec!["a2a:send".to_string()],
        tenant_chain: Vec::new(),
        trust_tier: ork_common::auth::TrustTier::default(),
        trust_class: ork_common::auth::TrustClass::default(),
        agent_id: None,
    });
    next.run(req).await
}

#[tokio::test]
async fn get_me_returns_user_tenant_scopes() {
    let app =
        protected_routes(WebUiState::test_stub()).layer(axum::middleware::from_fn(inject_auth));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/webui/api/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["user_id"], "tester");
    assert_eq!(v["scopes"], serde_json::json!(["webui:access", "a2a:send"]));
    assert!(v["tenant_id"].is_string());
}

/// ADR-0021 §`Vocabulary`: a token without `webui:access` cannot reach
/// any webui handler. Pinned so a future refactor of the layered router
/// cannot silently expose the surface.
#[tokio::test]
async fn get_me_without_webui_access_is_forbidden() {
    let app = protected_routes(WebUiState::test_stub())
        .layer(axum::middleware::from_fn(inject_auth_without_webui_access));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/webui/api/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
