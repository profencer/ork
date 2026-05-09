//! Integration coverage for the libsql backend's working-memory surface
//! (ADR 0053 acceptance criteria §working_memory.rs).
//!
//! The Postgres equivalent is gated behind `DATABASE_URL` and lives in
//! `tests/working_memory_postgres.rs` (added when the backend lands).

use ork_a2a::{ResourceId, ThreadId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::memory_store::{MemoryContext, WorkingMemoryShape};
use ork_memory::{Memory, MemoryOptions, SemanticRecallConfig};
use serde_json::json;
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
async fn write_read_round_trip_user_shape() {
    let td = tempfile::tempdir().unwrap();
    let mem = Memory::libsql(libsql_url(&td, "user"))
        .options(MemoryOptions {
            include_working: true,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: Some(WorkingMemoryShape::User),
            last_messages: 0,
        })
        .open()
        .await
        .expect("open");

    let mc = ctx(
        TenantId(Uuid::now_v7()),
        ResourceId::new(),
        ThreadId::new(),
        "weather",
    );

    assert!(mem.working_memory(&mc).await.unwrap().is_none());

    mem.set_working_memory(&mc, json!({"name": "Arseny"}))
        .await
        .expect("set");
    let v = mem.working_memory(&mc).await.unwrap().expect("present");
    assert_eq!(v["name"], "Arseny");

    mem.set_working_memory(&mc, json!({"name": "Different"}))
        .await
        .expect("update");
    let v = mem.working_memory(&mc).await.unwrap().expect("present");
    assert_eq!(v["name"], "Different");
}

#[tokio::test]
async fn per_tenant_isolation() {
    let td = tempfile::tempdir().unwrap();
    let mem = Memory::libsql(libsql_url(&td, "isolation"))
        .options(MemoryOptions {
            include_working: true,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: Some(WorkingMemoryShape::Free),
            last_messages: 0,
        })
        .open()
        .await
        .expect("open");

    let resource = ResourceId::new();
    let thread = ThreadId::new();
    let agent = "weather";

    let t1 = TenantId(Uuid::now_v7());
    let t2 = TenantId(Uuid::now_v7());

    mem.set_working_memory(
        &ctx(t1, resource, thread, agent),
        json!({"shared_key": "tenant-1"}),
    )
    .await
    .expect("t1 set");
    mem.set_working_memory(
        &ctx(t2, resource, thread, agent),
        json!({"shared_key": "tenant-2"}),
    )
    .await
    .expect("t2 set");

    let v1 = mem
        .working_memory(&ctx(t1, resource, thread, agent))
        .await
        .unwrap()
        .expect("t1 present");
    let v2 = mem
        .working_memory(&ctx(t2, resource, thread, agent))
        .await
        .unwrap()
        .expect("t2 present");

    assert_eq!(v1["shared_key"], "tenant-1");
    assert_eq!(v2["shared_key"], "tenant-2");
}

#[tokio::test]
async fn schema_validation_rejects_out_of_shape_writes() {
    let td = tempfile::tempdir().unwrap();
    let schema = json!({
        "type": "object",
        "properties": {"score": {"type": "integer", "minimum": 0}},
        "required": ["score"]
    });
    let mem = Memory::libsql(libsql_url(&td, "schema"))
        .options(MemoryOptions {
            include_working: true,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: Some(WorkingMemoryShape::Schema(schema)),
            last_messages: 0,
        })
        .open()
        .await
        .expect("open");

    let mc = ctx(
        TenantId(Uuid::now_v7()),
        ResourceId::new(),
        ThreadId::new(),
        "scorer",
    );

    // Out-of-shape — minimum violated.
    let err = mem
        .set_working_memory(&mc, json!({"score": -1}))
        .await
        .unwrap_err();
    assert!(matches!(err, OrkError::Validation(_)));

    // In-shape.
    mem.set_working_memory(&mc, json!({"score": 7}))
        .await
        .expect("ok");
    let v = mem.working_memory(&mc).await.unwrap().expect("present");
    assert_eq!(v["score"], 7);
}
