//! ADR-0020 §`Mesh trust — JWT claims and propagation`: RLS isolation under
//! [`open_tenant_tx`].
//!
//! Two assertions, separated because they have different load-bearing
//! conditions.
//!
//! **Policy presence (always-on).** Reads `pg_policies` to verify the
//! `tenant_isolation_runs` and `tenant_isolation_definitions` policies
//! declared in `migrations/001_initial.sql` are still attached to
//! `workflow_runs` / `workflow_definitions`. Catches "a future migration
//! accidentally drops a policy" regressions and runs even when the
//! connection role bypasses RLS.
//!
//! **Cross-tenant isolation under a non-superuser role.** Seeds tenant A,
//! tenant B, and a workflow_definition under A, writes a workflow_run via
//! `repo.create_run`, then reads under tenant A's scope (must see 1) and
//! tenant B's scope (must see 0). This assertion is only load-bearing
//! when the connection role does NOT bypass RLS — superusers and
//! `BYPASSRLS` roles always see all rows. The test logs and skips that
//! assertion when bypass is detected; CI must use a non-superuser role
//! for it to be meaningful.
//!
//! Run with `DATABASE_URL=postgres://...@localhost/ork_test cargo test -p
//! ork-persistence --test rls_smoke`.

use chrono::Utc;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::models::workflow::{WorkflowRun, WorkflowRunStatus};
use ork_core::ports::repository::WorkflowRepository;
use ork_persistence::postgres::{create_pool, open_tenant_tx, workflow_repo::PgWorkflowRepository};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    create_pool(&url, 2).await.ok()
}

/// True when the connection role bypasses RLS — the assertion below is
/// vacuous in that case (the row would always be visible). CI must run as a
/// non-superuser role for the test to be meaningful.
async fn role_bypasses_rls(pool: &PgPool) -> bool {
    let row: (bool,) = sqlx::query_as("SELECT current_setting('is_superuser')::bool")
        .fetch_one(pool)
        .await
        .unwrap_or((true,));
    row.0
}

async fn seed_tenant(pool: &PgPool) -> TenantId {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
        .bind(id)
        .bind("rls-tenant")
        .bind(format!("rls-{id}"))
        .execute(pool)
        .await
        .expect("seed tenant");
    TenantId(id)
}

async fn seed_workflow_definition(pool: &PgPool, tenant: TenantId) -> WorkflowId {
    let id = Uuid::now_v7();
    let now = Utc::now();
    sqlx::query(
        r#"
        INSERT INTO workflow_definitions (id, tenant_id, name, version, trigger, steps, created_at, updated_at)
        VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb, $7, $8)
        "#,
    )
    .bind(id)
    .bind(tenant.0)
    .bind("rls-wf")
    .bind("v1")
    .bind(serde_json::json!({"kind": "manual"}))
    .bind(serde_json::json!([]))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed workflow_definition");
    WorkflowId(id)
}

/// Runs unconditionally (when `DATABASE_URL` is provided) and proves the
/// `tenant_isolation_*` policies attached in `001_initial.sql` are still
/// attached on the workflow tables. This catches "future migration drops
/// the policy" regressions even on a superuser connection.
#[tokio::test]
async fn rls_policies_attached_to_workflow_tables() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping ADR-0020 rls_smoke (policy presence)");
        return;
    };
    let policies: Vec<(String, String)> = sqlx::query_as(
        "SELECT tablename::TEXT, policyname::TEXT FROM pg_policies \
         WHERE tablename IN ('workflow_runs', 'workflow_definitions') \
         ORDER BY tablename, policyname",
    )
    .fetch_all(&pool)
    .await
    .expect("query pg_policies");
    let names: Vec<&str> = policies.iter().map(|r| r.1.as_str()).collect();
    assert!(
        names.contains(&"tenant_isolation_definitions"),
        "tenant_isolation_definitions policy missing — ADR-0020 RLS regression. Got: {policies:?}"
    );
    assert!(
        names.contains(&"tenant_isolation_runs"),
        "tenant_isolation_runs policy missing — ADR-0020 RLS regression. Got: {policies:?}"
    );
}
// TODO: Run with database in testconatiners
#[tokio::test]
async fn workflow_run_written_under_tenant_a_invisible_under_tenant_b() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping ADR-0020 rls_smoke");
        return;
    };
    if role_bypasses_rls(&pool).await {
        eprintln!(
            "ADR-0020 rls_smoke: connection role bypasses RLS (is_superuser=on); \
             skipping the cross-tenant isolation assertion. The companion test \
             `rls_policies_attached_to_workflow_tables` still proves the policies \
             exist. Re-run as a non-superuser role to make the isolation assertion \
             load-bearing."
        );
        return;
    }

    // Two tenants and a workflow definition under tenant A.
    let tenant_a = seed_tenant(&pool).await;
    let tenant_b = seed_tenant(&pool).await;
    let workflow_a = seed_workflow_definition(&pool, tenant_a).await;

    // Write a workflow_run under tenant A via the helper-bound repository.
    // RLS policy `tenant_isolation_runs` (migrations/001_initial.sql) requires
    // tenant_id = current_setting('app.current_tenant_id')::UUID for visibility.
    let repo = PgWorkflowRepository::new(pool.clone());
    let run_id = WorkflowRunId(Uuid::now_v7());
    let run = WorkflowRun {
        id: run_id,
        workflow_id: workflow_a,
        tenant_id: tenant_a,
        status: WorkflowRunStatus::Pending,
        input: serde_json::Value::Null,
        output: None,
        step_results: Vec::new(),
        started_at: Utc::now(),
        completed_at: None,
        parent_run_id: None,
        parent_step_id: None,
        parent_task_id: None,
    };
    repo.create_run(&run).await.expect("create_run under A");

    // Direct read under tenant A's scope: row is visible.
    let mut tx_a = open_tenant_tx(&pool, tenant_a).await.expect("open tx A");
    let count_a: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::BIGINT FROM workflow_runs WHERE id = $1")
            .bind(run_id.0)
            .fetch_one(&mut *tx_a)
            .await
            .expect("count under A");
    assert_eq!(count_a.0, 1, "tenant A must see its own row");
    drop(tx_a);

    // Direct read under tenant B's scope: row must be hidden by RLS.
    let mut tx_b = open_tenant_tx(&pool, tenant_b).await.expect("open tx B");
    let count_b: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::BIGINT FROM workflow_runs WHERE id = $1")
            .bind(run_id.0)
            .fetch_one(&mut *tx_b)
            .await
            .expect("count under B");
    assert_eq!(
        count_b.0, 0,
        "tenant B must NOT see tenant A's row (RLS regression: ADR-0020)"
    );
}
