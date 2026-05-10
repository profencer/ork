//! ADR-0056 §`Auth and tenant scoping`: missing `X-Ork-Tenant` returns 400.
//!
//! Covers the acceptance criterion "Tenant header: missing
//! `X-Ork-Tenant` returns 400 (configurable via
//! `ServerConfig::default_tenant`)."

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ork_a2a::{MessageId, ResourceId};
use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::llm::ChatMessage;
use ork_core::ports::memory_store::{MemoryContext, MemoryStore, RecallHit, ThreadSummary};
use tower::util::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn missing_tenant_header_returns_400() {
    let app = OrkApp::builder().build().expect("build app");
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/manifest")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["kind"], "validation");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("X-Ork-Tenant"),
        "expected X-Ork-Tenant in message, got {v}"
    );
}

#[tokio::test]
async fn x_ork_tenant_header_passes() {
    let app = OrkApp::builder().build().expect("build app");
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/manifest")
                .header("X-Ork-Tenant", Uuid::new_v4().to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn legacy_x_tenant_id_alias_passes() {
    let app = OrkApp::builder().build().expect("build app");
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/manifest")
                .header("X-Tenant-Id", Uuid::new_v4().to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn default_tenant_makes_header_optional() {
    let app = OrkApp::builder().build().expect("build app");
    let cfg = ServerConfig {
        default_tenant: Some(Uuid::new_v4().to_string()),
        ..ServerConfig::default()
    };
    let router = ork_api::router_for(&app, &cfg);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/manifest")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn healthz_does_not_require_tenant_header() {
    // ADR-0056 §`Decision`: `/healthz` is liveness; load balancers and
    // probes hit it without auth or tenant context. Mounted ahead of
    // `tenant_middleware` in `router_for`.
    let app = OrkApp::builder().build().expect("build app");
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// Stub MemoryStore so this test compiles even though we don't use one;
// the type is referenced from the trait module imports above.
#[allow(dead_code)]
struct UnusedMem;

#[async_trait]
impl MemoryStore for UnusedMem {
    fn name(&self) -> &str {
        "unused"
    }
    async fn append_message(
        &self,
        _: &MemoryContext,
        _: ChatMessage,
    ) -> Result<MessageId, OrkError> {
        Ok(MessageId::new())
    }
    async fn last_messages(
        &self,
        _: &MemoryContext,
        _: usize,
    ) -> Result<Vec<ChatMessage>, OrkError> {
        Ok(Vec::new())
    }
    async fn working_memory(
        &self,
        _: &MemoryContext,
    ) -> Result<Option<serde_json::Value>, OrkError> {
        Ok(None)
    }
    async fn set_working_memory(
        &self,
        _: &MemoryContext,
        _: serde_json::Value,
    ) -> Result<(), OrkError> {
        Ok(())
    }
    async fn semantic_recall(
        &self,
        _: &MemoryContext,
        _: &str,
        _: usize,
    ) -> Result<Vec<RecallHit>, OrkError> {
        Ok(Vec::new())
    }
    async fn list_threads(
        &self,
        _: TenantId,
        _: &ResourceId,
    ) -> Result<Vec<ThreadSummary>, OrkError> {
        Ok(Vec::new())
    }
    async fn delete_thread(&self, _: &MemoryContext) -> Result<(), OrkError> {
        Ok(())
    }
}
