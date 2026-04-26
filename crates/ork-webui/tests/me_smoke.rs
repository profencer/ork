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
        scopes: vec!["a2a:send".to_string()],
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
    assert_eq!(v["scopes"], serde_json::json!(["a2a:send"]));
    assert!(v["tenant_id"].is_string());
}
