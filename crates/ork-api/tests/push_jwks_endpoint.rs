//! ADR-0009 §`Signing — JWS over the payload`: `/.well-known/jwks.json`.
//!
//! The endpoint is mounted in `public_routes`, so it must be reachable
//! without an `Authorization` header. The body MUST be a JWK set whose
//! first key matches the boot-generated `kid` and carries the
//! `kty=EC`/`crv=P-256`/`alg=ES256`/`use=sig` envelope subscribers expect.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ork_api::routes::jwks;
use tower::ServiceExt;

use crate::common::{read_body, test_state_with_push};

#[tokio::test]
async fn jwks_endpoint_returns_boot_generated_key() {
    let t = test_state_with_push().await;
    let app = jwks::routes(t.state.clone());

    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/jwks.json")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .expect("Content-Type header present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        ct.starts_with("application/jwk-set+json"),
        "JWKS MUST advertise application/jwk-set+json (RFC 7517 §8.5); got {ct}"
    );

    let body = read_body(resp).await;
    let keys = body["keys"].as_array().expect("`keys` array present");
    assert_eq!(keys.len(), 1, "boot path generates exactly one key");
    let key = &keys[0];
    assert_eq!(key["kty"], "EC", "ES256 keys MUST be EC");
    assert_eq!(key["crv"], "P-256", "ES256 implies P-256");
    assert_eq!(key["alg"], "ES256");
    assert_eq!(key["use"], "sig");
    let kid = key["kid"].as_str().expect("kid present").to_string();

    let signer = t
        .jwks_provider
        .current_signer()
        .await
        .expect("boot key present");
    assert_eq!(
        kid, signer.kid,
        "JWKS-served kid MUST match the active signer"
    );
    assert!(
        kid.starts_with("k_"),
        "kid follows the `k_<uuid>` convention (got {kid})"
    );
}

#[tokio::test]
async fn jwks_endpoint_advertises_short_cache() {
    let t = test_state_with_push().await;
    let app = jwks::routes(t.state.clone());

    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/jwks.json")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let cache = resp
        .headers()
        .get(axum::http::header::CACHE_CONTROL)
        .expect("Cache-Control header present")
        .to_str()
        .unwrap();
    assert!(
        cache.contains("max-age"),
        "Cache-Control MUST advertise max-age so subscribers refresh after rotation; got {cache}"
    );
}

#[tokio::test]
async fn jwks_endpoint_reflects_rotation_in_real_time() {
    let t = test_state_with_push().await;

    let before = t.jwks_provider.jwks().await;
    let before_kids: Vec<String> = before["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kid"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(before_kids.len(), 1);

    t.jwks_provider
        .rotate_if_due(chrono::Utc::now(), true)
        .await
        .unwrap()
        .expect("forced rotation must produce an outcome");

    let app = jwks::routes(t.state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/jwks.json")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = read_body(resp).await;
    let kids: Vec<String> = body["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kid"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kids.len(),
        2,
        "both keys MUST appear during the overlap window; got {kids:?}"
    );
    assert!(
        kids.contains(&before_kids[0]),
        "rotated-out key MUST remain in JWKS during overlap"
    );
}
