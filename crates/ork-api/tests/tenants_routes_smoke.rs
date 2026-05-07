//! ADR-0020 §`Tenant CRUD restricted`: scope gating on the `/api/tenants` HTTP
//! surface. Each verb is exercised once with the wrong scope (expecting 403)
//! and once with the right scope (expecting whatever the stub repository
//! produces). The `StubTenantRepository` returns:
//!   - `create`        → `Err(Internal)`            → 500 when allowed
//!   - `get_by_id`     → `Err(NotFound)`            → 404 when allowed
//!   - `list`          → `Ok(vec![])`               → 200 when allowed
//!   - `update_settings` → `Err(Internal)`          → 500 when allowed
//!   - `delete`        → `Ok(())`                   → 204 when allowed
//!
//! A 200/204/404/500 result therefore proves the scope gate let the request
//! through; a 403 proves the gate rejected it. We only assert "gate behaviour"
//! here — full repo behaviour is covered by `crates/ork-persistence/tests/`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn;
use ork_api::middleware::AuthContext;
use ork_api::routes::tenants;
use ork_common::auth::{TENANT_ADMIN_SCOPE, TENANT_SELF_SCOPE};
use ork_common::types::TenantId;
use tower::ServiceExt;
use uuid::Uuid;

use common::{auth_for_with_scopes, test_state};

fn auth(tenant_id: TenantId, scopes: &[&str]) -> AuthContext {
    auth_for_with_scopes(tenant_id, scopes)
}

fn router_with_auth(state: ork_api::state::AppState, ctx: AuthContext) -> axum::Router {
    let inject = move |mut req: Request<Body>, next: axum::middleware::Next| {
        let ctx = ctx.clone();
        async move {
            req.extensions_mut().insert(ctx);
            next.run(req).await
        }
    };
    tenants::routes(state).layer(from_fn(inject))
}

async fn send(router: axum::Router, req: Request<Body>) -> StatusCode {
    router.oneshot(req).await.expect("oneshot").status()
}

#[tokio::test]
async fn create_tenant_requires_admin_scope() {
    let t = test_state().await;
    let tenant = TenantId::new();
    let body = r#"{"name":"x","slug":"x"}"#;

    // No admin scope → 403.
    let r = router_with_auth(t.state.clone(), auth(tenant, &[TENANT_SELF_SCOPE]));
    let req = Request::post("/api/tenants")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(r, req).await, StatusCode::FORBIDDEN);

    // With admin scope → falls through to the stub repo (which errors 500).
    let r = router_with_auth(t.state.clone(), auth(tenant, &[TENANT_ADMIN_SCOPE]));
    let req = Request::post("/api/tenants")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(r, req).await, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn list_tenants_requires_admin_scope() {
    let t = test_state().await;
    let tenant = TenantId::new();

    let r = router_with_auth(t.state.clone(), auth(tenant, &[]));
    assert_eq!(
        send(r, Request::get("/api/tenants").body(Body::empty()).unwrap()).await,
        StatusCode::FORBIDDEN
    );

    let r = router_with_auth(t.state.clone(), auth(tenant, &[TENANT_ADMIN_SCOPE]));
    assert_eq!(
        send(r, Request::get("/api/tenants").body(Body::empty()).unwrap()).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn delete_tenant_requires_admin_scope() {
    let t = test_state().await;
    let tenant = TenantId::new();
    let target = Uuid::now_v7();
    let path = format!("/api/tenants/{target}");

    let r = router_with_auth(t.state.clone(), auth(tenant, &[TENANT_SELF_SCOPE]));
    assert_eq!(
        send(r, Request::delete(&path).body(Body::empty()).unwrap()).await,
        StatusCode::FORBIDDEN
    );

    let r = router_with_auth(t.state.clone(), auth(tenant, &[TENANT_ADMIN_SCOPE]));
    assert_eq!(
        send(r, Request::delete(&path).body(Body::empty()).unwrap()).await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn get_tenant_self_only_for_own_tenant() {
    let t = test_state().await;
    let me = TenantId::new();
    let other = Uuid::now_v7();

    // tenant:self with own id → falls through to stub (NotFound).
    let r = router_with_auth(t.state.clone(), auth(me, &[TENANT_SELF_SCOPE]));
    let path = format!("/api/tenants/{}", me.0);
    assert_eq!(
        send(r, Request::get(&path).body(Body::empty()).unwrap()).await,
        StatusCode::NOT_FOUND
    );

    // tenant:self trying to read a different tenant → 403.
    let r = router_with_auth(t.state.clone(), auth(me, &[TENANT_SELF_SCOPE]));
    let path = format!("/api/tenants/{other}");
    assert_eq!(
        send(r, Request::get(&path).body(Body::empty()).unwrap()).await,
        StatusCode::FORBIDDEN
    );

    // tenant:admin can read any tenant.
    let r = router_with_auth(t.state.clone(), auth(me, &[TENANT_ADMIN_SCOPE]));
    let path = format!("/api/tenants/{other}");
    assert_eq!(
        send(r, Request::get(&path).body(Body::empty()).unwrap()).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn update_settings_requires_self_or_admin() {
    let t = test_state().await;
    let me = TenantId::new();
    let other = Uuid::now_v7();
    let body = r#"{"github_token":"ghp_test"}"#;

    // tenant:self updating someone else's settings → 403 (would have been
    // allowed pre-A5 because the old branch only checked id-equality, but
    // now the explicit scope+id rule catches it).
    let r = router_with_auth(t.state.clone(), auth(me, &[TENANT_SELF_SCOPE]));
    let path = format!("/api/tenants/{other}/settings");
    let req = Request::put(&path)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(r, req).await, StatusCode::FORBIDDEN);

    // tenant:self updating own settings → falls through to stub (Internal).
    let r = router_with_auth(t.state.clone(), auth(me, &[TENANT_SELF_SCOPE]));
    let path = format!("/api/tenants/{}/settings", me.0);
    let req = Request::put(&path)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(r, req).await, StatusCode::INTERNAL_SERVER_ERROR);
}
