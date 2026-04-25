//! ADR-0008 §`Auth + tenant isolation` integration tests for [`auth_middleware`].
//!
//! Pinned behaviours:
//! - JWT `scopes` flow into `AuthContext`.
//! - `X-Tenant-Id` impersonation is honoured iff `tenant:admin` scope is present.
//! - Missing or invalid tokens return 401 (sanity check; not new behaviour).

use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use jsonwebtoken::{EncodingKey, Header, encode};
use ork_api::middleware::{AuthContext, auth_middleware};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

fn token(tenant: Uuid, sub: &str, scopes: &[&str]) -> String {
    let claims = json!({
        "sub": sub,
        "tenant_id": tenant.to_string(),
        "scopes": scopes,
        "exp": (chrono::Utc::now().timestamp() + 60) as usize,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"change-me-in-production"),
    )
    .unwrap()
}

fn legacy_token_no_scopes(tenant: Uuid, sub: &str) -> String {
    let claims = json!({
        "sub": sub,
        "tenant_id": tenant.to_string(),
        "exp": (chrono::Utc::now().timestamp() + 60) as usize,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"change-me-in-production"),
    )
    .unwrap()
}

fn echo_app() -> Router {
    Router::new()
        .route(
            "/x",
            get(|Extension(ctx): Extension<AuthContext>| async move {
                format!("{}|{}", ctx.tenant_id.0, ctx.scopes.join(","))
            }),
        )
        .layer(axum::middleware::from_fn(auth_middleware))
}

async fn body(resp: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(resp.into_body(), 1 << 14).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn jwt_with_scopes_populates_context() {
    let app = echo_app();
    let tenant = Uuid::now_v7();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/x")
                .header(
                    "Authorization",
                    format!(
                        "Bearer {}",
                        token(tenant, "alice", &["a2a:send", "ops:read"])
                    ),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body(resp).await;
    assert!(body.contains(&tenant.to_string()));
    assert!(body.contains("a2a:send"));
    assert!(body.contains("ops:read"));
}

#[tokio::test]
async fn legacy_token_without_scopes_field_still_decodes() {
    let app = echo_app();
    let tenant = Uuid::now_v7();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/x")
                .header(
                    "Authorization",
                    format!("Bearer {}", legacy_token_no_scopes(tenant, "old-client")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body(resp).await;
    assert_eq!(body, format!("{tenant}|"));
}

#[tokio::test]
async fn admin_impersonation_via_x_tenant_id_header() {
    let app = echo_app();
    let admin_tenant = Uuid::now_v7();
    let target = Uuid::now_v7();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/x")
                .header(
                    "Authorization",
                    format!("Bearer {}", token(admin_tenant, "ops", &["tenant:admin"])),
                )
                .header("X-Tenant-Id", target.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body(resp).await;
    assert!(
        body.starts_with(&target.to_string()),
        "admin scope must allow X-Tenant-Id impersonation, got {body}"
    );
}

#[tokio::test]
async fn x_tenant_id_is_ignored_without_admin_scope() {
    let app = echo_app();
    let claim_tenant = Uuid::now_v7();
    let target = Uuid::now_v7();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/x")
                .header(
                    "Authorization",
                    format!("Bearer {}", token(claim_tenant, "user", &["a2a:send"])),
                )
                .header("X-Tenant-Id", target.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body(resp).await;
    assert!(
        body.starts_with(&claim_tenant.to_string()),
        "non-admin caller MUST NOT be able to impersonate via X-Tenant-Id, got {body}"
    );
}

#[tokio::test]
async fn missing_authorization_header_returns_401() {
    let app = echo_app();
    let resp = app
        .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
