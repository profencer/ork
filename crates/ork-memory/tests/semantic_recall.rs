//! Integration coverage for the libsql backend's semantic-recall surface
//! (ADR 0053 acceptance criteria §semantic_recall.rs).

use std::sync::Arc;

use ork_a2a::{ResourceId, ThreadId};
use ork_common::types::TenantId;
use ork_core::ports::llm::ChatMessage;
use ork_core::ports::memory_store::{MemoryContext, Scope};
use ork_memory::{DeterministicMockEmbedder, Memory, MemoryOptions, SemanticRecallConfig};
use uuid::Uuid;

fn libsql_url(td: &tempfile::TempDir, suffix: &str) -> String {
    format!("file:{}", td.path().join(format!("{suffix}.db")).display())
}

fn ctx(tenant: TenantId, resource: ResourceId, thread: ThreadId, agent: &str) -> MemoryContext {
    MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id: thread,
        agent_id: agent.to_string(),
    }
}

#[tokio::test]
async fn embed_store_retrieve_round_trip() {
    let td = tempfile::tempdir().unwrap();
    let mem = Memory::libsql(libsql_url(&td, "round-trip"))
        .options(MemoryOptions {
            include_working: false,
            semantic_recall: SemanticRecallConfig {
                enabled: true,
                top_k: 3,
                scope: Scope::Resource,
            },
            working_memory: None,
            last_messages: 0,
        })
        .embedder(Arc::new(DeterministicMockEmbedder::new(16)))
        .open()
        .await
        .expect("open");

    let mc = ctx(
        TenantId(Uuid::now_v7()),
        ResourceId::new(),
        ThreadId::new(),
        "weather",
    );
    for body in ["hello world", "today is sunny", "the weather in Berlin"] {
        mem.append_message(&mc, ChatMessage::user(body))
            .await
            .expect("append");
    }

    // The deterministic embedder produces stable vectors — querying with
    // an exact past message must rank that message first.
    let hits = mem
        .semantic_recall(&mc, "today is sunny", 3)
        .await
        .expect("recall");
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(hits[0].content, "today is sunny");
}

#[tokio::test]
async fn scope_resource_shares_hits_across_threads() {
    let td = tempfile::tempdir().unwrap();
    let mem = Memory::libsql(libsql_url(&td, "scope-resource"))
        .options(MemoryOptions {
            include_working: false,
            semantic_recall: SemanticRecallConfig {
                enabled: true,
                top_k: 5,
                scope: Scope::Resource,
            },
            working_memory: None,
            last_messages: 0,
        })
        .embedder(Arc::new(DeterministicMockEmbedder::new(16)))
        .open()
        .await
        .expect("open");

    let tenant = TenantId(Uuid::now_v7());
    let resource = ResourceId::new();
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();

    mem.append_message(
        &ctx(tenant, resource, thread_a, "agent"),
        ChatMessage::user("alpha thread A"),
    )
    .await
    .expect("append A");
    mem.append_message(
        &ctx(tenant, resource, thread_b, "agent"),
        ChatMessage::user("beta thread B"),
    )
    .await
    .expect("append B");

    // Query from thread A — Resource scope MUST surface the thread-B
    // message.
    let hits = mem
        .semantic_recall(
            &ctx(tenant, resource, thread_a, "agent"),
            "beta thread B",
            5,
        )
        .await
        .expect("recall");
    let any_b = hits.iter().any(|h| h.content == "beta thread B");
    assert!(
        any_b,
        "Scope::Resource must include cross-thread hits, got {hits:?}"
    );
}

#[tokio::test]
async fn scope_thread_isolates() {
    let td = tempfile::tempdir().unwrap();
    let mem = Memory::libsql(libsql_url(&td, "scope-thread"))
        .options(MemoryOptions {
            include_working: false,
            semantic_recall: SemanticRecallConfig {
                enabled: true,
                top_k: 5,
                scope: Scope::Thread,
            },
            working_memory: None,
            last_messages: 0,
        })
        .embedder(Arc::new(DeterministicMockEmbedder::new(16)))
        .open()
        .await
        .expect("open");

    let tenant = TenantId(Uuid::now_v7());
    let resource = ResourceId::new();
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();

    mem.append_message(
        &ctx(tenant, resource, thread_a, "agent"),
        ChatMessage::user("alpha thread A"),
    )
    .await
    .expect("append A");
    mem.append_message(
        &ctx(tenant, resource, thread_b, "agent"),
        ChatMessage::user("beta thread B"),
    )
    .await
    .expect("append B");

    // Query from thread A with Scope::Thread — must NOT see thread B.
    let hits = mem
        .semantic_recall(
            &ctx(tenant, resource, thread_a, "agent"),
            "beta thread B",
            5,
        )
        .await
        .expect("recall");
    assert!(
        hits.iter().all(|h| h.content != "beta thread B"),
        "Scope::Thread must not leak across threads, got {hits:?}"
    );
}
