//! Integration test for the ADR-0009 push-notification config slice (pulled
//! forward by the ADR-0008 plan). Skipped when `DATABASE_URL` is unset.

use chrono::Utc;
use ork_a2a::TaskId;
use ork_common::types::TenantId;
use ork_core::ports::a2a_push_repo::{A2aPushConfigRepository, A2aPushConfigRow};
use ork_persistence::postgres::{a2a_push_repo::PgA2aPushConfigRepository, create_pool};
use sqlx::PgPool;
use url::Url;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    create_pool(&url, 2).await.ok()
}

async fn seed_tenant_and_task(pool: &PgPool) -> (TenantId, TaskId) {
    let tenant = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
        .bind(tenant)
        .bind("push-tenant")
        .bind(format!("p-{tenant}"))
        .execute(pool)
        .await
        .expect("seed tenant");
    let task = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO a2a_tasks (id, context_id, tenant_id, agent_id, state) \
         VALUES ($1, $1, $2, 'planner', 'submitted')",
    )
    .bind(task)
    .bind(tenant)
    .execute(pool)
    .await
    .expect("seed task");
    (TenantId(tenant), TaskId(task))
}

#[tokio::test]
async fn upsert_and_fetch_round_trip() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping push-config round-trip test");
        return;
    };
    let (tenant, task_id) = seed_tenant_and_task(&pool).await;
    let repo = PgA2aPushConfigRepository::new(pool.clone());

    let row = A2aPushConfigRow {
        id: Uuid::now_v7(),
        task_id,
        tenant_id: tenant,
        url: Url::parse("https://example.com/cb").unwrap(),
        token: Some("secret".into()),
        authentication: Some(serde_json::json!({"schemes": ["bearer"]})),
        metadata: serde_json::json!({}),
        created_at: Utc::now(),
    };
    repo.upsert(&row).await.expect("upsert");

    let got = repo
        .get(tenant, task_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.url, row.url);
    assert_eq!(got.token.as_deref(), Some("secret"));
    assert_eq!(got.authentication.as_ref().unwrap()["schemes"][0], "bearer");
}

#[tokio::test]
async fn upsert_overwrites_existing_row_with_same_id() {
    let Some(pool) = pool().await else {
        return;
    };
    let (tenant, task_id) = seed_tenant_and_task(&pool).await;
    let repo = PgA2aPushConfigRepository::new(pool.clone());

    let id = Uuid::now_v7();
    let mut row = A2aPushConfigRow {
        id,
        task_id,
        tenant_id: tenant,
        url: Url::parse("https://example.com/v1").unwrap(),
        token: None,
        authentication: None,
        metadata: serde_json::json!({}),
        created_at: Utc::now(),
    };
    repo.upsert(&row).await.unwrap();
    row.url = Url::parse("https://example.com/v2").unwrap();
    row.token = Some("rotated".into());
    repo.upsert(&row).await.unwrap();

    let got = repo.get(tenant, task_id).await.unwrap().unwrap();
    assert_eq!(got.url.as_str(), "https://example.com/v2");
    assert_eq!(got.token.as_deref(), Some("rotated"));
}
