//! Integration tests for the ADR-0008 widened [`A2aTaskRepository`].
//!
//! These exercise the real Postgres driver against a database whose URL is taken
//! from `DATABASE_URL`. When the variable is unset the tests early-return so a
//! laptop `cargo test` (no Postgres) still passes; CI provides the URL and runs
//! every assertion. The schema is the cumulative result of `001_initial.sql`,
//! `002_workflow_status_extensions.sql`, `003_delegation.sql`, and
//! `004_a2a_endpoints.sql`.

use chrono::Utc;
use ork_a2a::{ContextId, MessageId, Part, TaskId, TaskState};
use ork_common::types::TenantId;
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use ork_persistence::postgres::{a2a_task_repo::PgA2aTaskRepository, create_pool};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    create_pool(&url, 2).await.ok()
}

async fn seed_tenant(pool: &PgPool) -> TenantId {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
        .bind(id)
        .bind("test-tenant")
        .bind(format!("t-{id}"))
        .execute(pool)
        .await
        .expect("seed tenant");
    TenantId(id)
}

fn task_row(tenant: TenantId, state: TaskState) -> A2aTaskRow {
    let now = Utc::now();
    A2aTaskRow {
        id: TaskId::new(),
        context_id: ContextId::new(),
        tenant_id: tenant,
        agent_id: "planner".into(),
        parent_task_id: None,
        workflow_run_id: None,
        state,
        metadata: serde_json::json!({}),
        created_at: now,
        updated_at: now,
        completed_at: None,
    }
}

#[tokio::test]
async fn task_crud_round_trip_with_context_and_metadata() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping ADR-0008 a2a_task_repo round-trip test");
        return;
    };
    let tenant = seed_tenant(&pool).await;
    let repo = PgA2aTaskRepository::new(pool.clone());
    let mut row = task_row(tenant, TaskState::Submitted);
    row.metadata = serde_json::json!({"k": "v"});
    repo.create_task(&row).await.expect("create");

    let got = repo
        .get_task(tenant, row.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.context_id, row.context_id);
    assert_eq!(got.metadata["k"], "v");
    assert_eq!(got.state, TaskState::Submitted);
    assert!(got.completed_at.is_none());
}

#[tokio::test]
async fn update_state_to_terminal_sets_completed_at() {
    let Some(pool) = pool().await else {
        return;
    };
    let tenant = seed_tenant(&pool).await;
    let repo = PgA2aTaskRepository::new(pool.clone());
    let row = task_row(tenant, TaskState::Working);
    repo.create_task(&row).await.expect("create");

    repo.update_state(tenant, row.id, TaskState::Completed)
        .await
        .expect("update");

    let got = repo.get_task(tenant, row.id).await.unwrap().unwrap();
    assert_eq!(got.state, TaskState::Completed);
    assert!(
        got.completed_at.is_some(),
        "terminal transition must stamp completed_at"
    );
}

#[tokio::test]
async fn append_and_list_messages_in_seq_order() {
    let Some(pool) = pool().await else {
        return;
    };
    let tenant = seed_tenant(&pool).await;
    let repo = PgA2aTaskRepository::new(pool.clone());
    let row = task_row(tenant, TaskState::Submitted);
    let task_id = row.id;
    repo.create_task(&row).await.expect("create");

    for txt in ["hi", "second"] {
        repo.append_message(&A2aMessageRow {
            id: MessageId::new(),
            task_id,
            role: "user".into(),
            parts: serde_json::to_value(vec![Part::text(txt)]).unwrap(),
            metadata: serde_json::json!({}),
            created_at: Utc::now(),
        })
        .await
        .expect("append");
    }

    let msgs = repo
        .list_messages(tenant, task_id, None)
        .await
        .expect("list");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].parts[0]["text"], "hi");
    assert_eq!(msgs[1].parts[0]["text"], "second");

    let limited = repo
        .list_messages(tenant, task_id, Some(1))
        .await
        .expect("list capped");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].parts[0]["text"], "hi");
}

#[tokio::test]
async fn list_tasks_in_tenant_filters_by_tenant() {
    let Some(pool) = pool().await else {
        return;
    };
    let t1 = seed_tenant(&pool).await;
    let t2 = seed_tenant(&pool).await;
    let repo = PgA2aTaskRepository::new(pool.clone());
    repo.create_task(&task_row(t1, TaskState::Working))
        .await
        .unwrap();
    repo.create_task(&task_row(t1, TaskState::Working))
        .await
        .unwrap();
    repo.create_task(&task_row(t2, TaskState::Working))
        .await
        .unwrap();

    let only_t1 = repo.list_tasks_in_tenant(t1, 100).await.unwrap();
    assert!(only_t1.iter().all(|r| r.tenant_id == t1));
    assert!(only_t1.len() >= 2);

    let only_t2 = repo.list_tasks_in_tenant(t2, 100).await.unwrap();
    assert!(only_t2.iter().all(|r| r.tenant_id == t2));
}
