//! ADR-0055 AC #9: `DELETE /studio/api/memory/threads/:id` removes the
//! thread end-to-end. The libsql backend's `delete_thread` issues
//! `DELETE FROM mem_messages` and `DELETE FROM mem_embeddings`
//! (`crates/ork-memory/src/libsql_backend.rs:403`); this test asserts
//! both tables are empty after the Studio route is called.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ork_a2a::{ResourceId, ThreadId};
use ork_app::OrkApp;
use ork_app::types::{ServerConfig, StudioConfig};
use ork_common::types::TenantId;
use ork_core::ports::llm::ChatMessage;
use ork_core::ports::memory_store::{MemoryContext, MemoryOptions};
use ork_memory::{DeterministicMockEmbedder, Memory};
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn delete_thread_clears_messages_and_embeddings() {
    // 1. Boot an on-disk libsql backend (each libsql connection on
    //    `:memory:` is its own database, so the DDL on the open
    //    connection wouldn't be visible to subsequent connections —
    //    use a tempdir file URL instead). Semantic recall is on so
    //    appended messages land in both `mem_messages` and
    //    `mem_embeddings`.
    let td = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", td.path().join("studio-mem.db").display());
    let mut options = MemoryOptions::default();
    options.semantic_recall.enabled = true;
    let memory = Memory::libsql(url)
        .options(options)
        .embedder(Arc::new(DeterministicMockEmbedder::default()))
        .open()
        .await
        .expect("open libsql memory");

    let tenant = TenantId::new();
    let resource = ResourceId(Uuid::new_v4());
    let thread = ThreadId::new();
    let agent = "test-agent".to_string();

    let ctx = MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id: thread,
        agent_id: agent.clone(),
    };

    // Seed the thread with two messages so the embeddings table is
    // non-empty.
    memory
        .append_message(&ctx, ChatMessage::user("Hello, Studio."))
        .await
        .expect("append #1");
    memory
        .append_message(&ctx, ChatMessage::assistant("Hi back.", vec![]))
        .await
        .expect("append #2");

    // Sanity: both rows exist before the delete.
    let pre = memory.last_messages(&ctx, 10).await.expect("last messages");
    assert_eq!(pre.len(), 2, "seed didn't land");

    // 2. Boot an OrkApp wired to this memory store and mount the
    //    Studio router.
    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            studio: StudioConfig::Enabled,
            ..ServerConfig::default()
        })
        .memory_arc(memory.clone())
        .build()
        .expect("build app");

    // ork-studio's API routes now ship with `tenant_middleware`
    // attached. Configure `default_tenant` to the same `tenant` we
    // seeded the memory under so the DELETE handler resolves to the
    // same `(tenant, resource, thread)` triple.
    let cfg = ServerConfig {
        host: "127.0.0.1".into(),
        port: 0,
        studio: StudioConfig::Enabled,
        default_tenant: Some(tenant.0.to_string()),
        ..ServerConfig::default()
    };
    let router = ork_studio::router(&app, &cfg).expect("studio enabled");

    // 3. Hit DELETE /studio/api/memory/threads/:id?resource=...
    let uri = format!(
        "/studio/api/memory/threads/{}?resource={}",
        thread.0, resource.0
    );
    let resp = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "DELETE failed");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(
        json.get("studio_api_version").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        json.pointer("/data/ok").and_then(|v| v.as_bool()),
        Some(true)
    );

    // 4. Verify both `mem_messages` and `mem_embeddings` rows are gone.
    let post_msgs = memory.last_messages(&ctx, 10).await.expect("last messages");
    assert!(
        post_msgs.is_empty(),
        "mem_messages still has rows: {post_msgs:?}"
    );

    let post_recall = memory
        .semantic_recall(&ctx, "Hello", 5)
        .await
        .expect("semantic_recall");
    assert!(
        post_recall.is_empty(),
        "mem_embeddings still has rows: {post_recall:?}"
    );
}
