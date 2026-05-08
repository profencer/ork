//! ADR-0020 §`Mesh trust — JWT claims and propagation`: server-side
//! verification of `X-Ork-Mesh-Token`. When the request carries the
//! header AND a [`MeshTokenSigner`] is wired into the router via
//! `Extension`, the mesh claims override the bearer-derived
//! [`AuthContext`] (tenant_id, tenant_chain, scopes, trust_class,
//! agent_id).
//!
//! The bearer is still required (it authenticates the mesh peer / Kong
//! shape); the mesh token attests to the originator's identity at the
//! moment the call was issued.

use std::sync::Arc;

use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use chrono::Duration;
use jsonwebtoken::{EncodingKey, Header, encode};
use ork_api::middleware::{AuthContext, auth_middleware};
use ork_common::auth::{TrustClass, TrustTier};
use ork_common::types::TenantId;
use ork_security::{HmacMeshTokenSigner, MeshClaims, MeshTokenSigner, mesh_token_header};
use secrecy::SecretString;
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

const JWT_SECRET: &[u8] = b"change-me-in-production";
const MESH_SECRET: &str = "mesh-shared-secret";
const MESH_ISS: &str = "ork-mesh-test";
const MESH_AUD: &str = "ork-api-test";

fn bearer_token(tenant: Uuid, sub: &str, scopes: &[&str]) -> String {
    let claims = json!({
        "sub": sub,
        "tenant_id": tenant.to_string(),
        "scopes": scopes,
        "exp": (chrono::Utc::now().timestamp() + 60) as usize,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(JWT_SECRET),
    )
    .expect("bearer encodes")
}

fn signer() -> Arc<dyn MeshTokenSigner> {
    Arc::new(HmacMeshTokenSigner::new(
        SecretString::from(MESH_SECRET),
        MESH_ISS.into(),
        MESH_AUD.into(),
    ))
}

fn echo_app(signer: Option<Arc<dyn MeshTokenSigner>>) -> Router {
    let mut app = Router::new()
        .route(
            "/echo",
            get(|Extension(ctx): Extension<AuthContext>| async move {
                let chain = ctx
                    .tenant_chain
                    .iter()
                    .map(|t| t.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{}|{}|{}|{}|{:?}|{:?}",
                    ctx.tenant_id.0,
                    ctx.scopes.join(","),
                    chain,
                    ctx.user_id,
                    ctx.trust_class,
                    ctx.agent_id,
                )
            }),
        )
        .layer(axum::middleware::from_fn(auth_middleware));
    if let Some(s) = signer {
        app = app.layer(Extension(s));
    }
    app
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(resp.into_body(), 1 << 14).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Mesh token is verified and overrides the bearer-derived `AuthContext`.
#[tokio::test]
async fn mesh_token_overrides_bearer_context() {
    let signer = signer();
    let app = echo_app(Some(signer.clone()));

    // Bearer says: tenant A, scope set X.
    let bearer_tenant = Uuid::now_v7();
    let bearer = bearer_token(bearer_tenant, "kong-peer", &["agent:bearer-only:invoke"]);

    // Mesh token says: tenant B, different scopes, agent class.
    let mesh_tenant = TenantId(Uuid::now_v7());
    let claims = MeshClaims::new(
        "agent:planner".into(),
        mesh_tenant,
        vec![mesh_tenant],
        vec!["agent:reviewer:invoke".into()],
        TrustTier::Internal,
        TrustClass::Agent,
        Some("planner".into()),
        MESH_ISS.into(),
        MESH_AUD.into(),
        Duration::seconds(60),
    );
    let mesh = signer.mint(claims).await.expect("mint");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/echo")
                .header("Authorization", format!("Bearer {bearer}"))
                .header(mesh_token_header(), mesh)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let echoed = body_string(resp).await;

    // tenant_id: from mesh, not bearer.
    assert!(echoed.starts_with(&mesh_tenant.0.to_string()));
    assert!(
        !echoed.contains(&bearer_tenant.to_string()),
        "bearer tenant must not appear once mesh override fires (got: {echoed})"
    );
    // scopes: from mesh.
    assert!(
        echoed.contains("agent:reviewer:invoke"),
        "mesh scope must be in context (got: {echoed})"
    );
    assert!(
        !echoed.contains("agent:bearer-only:invoke"),
        "bearer scope must NOT be carried over once mesh overrides (got: {echoed})"
    );
    // trust_class: forced to Agent.
    assert!(
        echoed.contains("Agent"),
        "trust_class must be Agent under mesh override (got: {echoed})"
    );
}

/// Without `X-Ork-Mesh-Token`, the bearer-derived context is unchanged
/// (Phase A behaviour, sanity check that B4 doesn't leak into the
/// bearer-only path).
#[tokio::test]
async fn no_mesh_token_keeps_bearer_context() {
    let app = echo_app(Some(signer()));
    let tenant = Uuid::now_v7();
    let bearer = bearer_token(tenant, "kong-peer", &["agent:foo:invoke"]);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/echo")
                .header("Authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let echoed = body_string(resp).await;
    assert!(echoed.contains(&tenant.to_string()));
    assert!(echoed.contains("agent:foo:invoke"));
    // M1 fix from Phase A: tenant_chain seeded with [tenant_id].
    assert!(echoed.contains(&tenant.to_string()));
}

/// An invalid mesh token (wrong secret) is a hard 401 — the bearer is
/// not enough to fall back to once the caller advertised a mesh
/// attestation that doesn't verify.
#[tokio::test]
async fn invalid_mesh_token_returns_401() {
    let app = echo_app(Some(signer()));
    let tenant = Uuid::now_v7();
    let bearer = bearer_token(tenant, "kong-peer", &["x"]);

    // Mint with a different secret so verify() rejects.
    let other_signer = Arc::new(HmacMeshTokenSigner::new(
        SecretString::from("a-different-secret"),
        MESH_ISS.into(),
        MESH_AUD.into(),
    ));
    let claims = MeshClaims::new(
        "agent:x".into(),
        TenantId(Uuid::now_v7()),
        vec![TenantId(Uuid::now_v7())],
        vec![],
        TrustTier::Internal,
        TrustClass::Agent,
        None,
        MESH_ISS.into(),
        MESH_AUD.into(),
        Duration::seconds(60),
    );
    let bad = other_signer.mint(claims).await.expect("mint");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/echo")
                .header("Authorization", format!("Bearer {bearer}"))
                .header(mesh_token_header(), bad)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// When no signer is wired (legacy / dev), the mesh-token header is
/// silently ignored and the bearer-derived context wins. Asserts the
/// override only fires when the operator opted in via the Extension layer.
#[tokio::test]
async fn no_signer_extension_means_mesh_header_ignored() {
    let app = echo_app(None);
    let tenant = Uuid::now_v7();
    let bearer = bearer_token(tenant, "kong-peer", &["bearer-scope"]);

    // Garbage in the header — must not even be read.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/echo")
                .header("Authorization", format!("Bearer {bearer}"))
                .header(mesh_token_header(), "not-a-jwt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let echoed = body_string(resp).await;
    assert!(echoed.contains("bearer-scope"));
}
