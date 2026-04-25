//! Postgres-backed [`A2aTaskRepository`] for the ADR-0008 widening.
//!
//! Schema: `a2a_tasks` (extended with `context_id`, `metadata`, `completed_at` in
//! `004_a2a_endpoints.sql`) plus the new `a2a_messages` table. Every read/mutate
//! filters explicitly on `tenant_id` until ADR-0020 wires
//! `SET LOCAL app.current_tenant_id` per request — sites are tagged with
//! `// ADR-0020:` so the follow-up plan can find them.

use async_trait::async_trait;
use chrono::Utc;
use ork_a2a::{ContextId, MessageId, TaskId, TaskState};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowRunId};
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use sqlx::PgPool;

pub struct PgA2aTaskRepository {
    pool: PgPool,
}

impl PgA2aTaskRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn state_to_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::InputRequired => "input_required",
        TaskState::AuthRequired => "auth_required",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::Rejected => "rejected",
    }
}

fn parse_state(raw: &str) -> Result<TaskState, OrkError> {
    Ok(match raw {
        "submitted" => TaskState::Submitted,
        "working" => TaskState::Working,
        "input_required" => TaskState::InputRequired,
        "auth_required" => TaskState::AuthRequired,
        "completed" => TaskState::Completed,
        "failed" => TaskState::Failed,
        "canceled" => TaskState::Canceled,
        "rejected" => TaskState::Rejected,
        other => {
            return Err(OrkError::Internal(format!(
                "unknown a2a_tasks.state: {other}"
            )));
        }
    })
}

const fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
    )
}

#[async_trait]
impl A2aTaskRepository for PgA2aTaskRepository {
    async fn create_task(&self, row: &A2aTaskRow) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            INSERT INTO a2a_tasks (
                id, context_id, tenant_id, agent_id, parent_task_id, workflow_run_id,
                state, metadata, created_at, updated_at, completed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(row.id.0)
        .bind(row.context_id.0)
        .bind(row.tenant_id.0)
        .bind(&row.agent_id)
        .bind(row.parent_task_id.map(|id| id.0))
        .bind(row.workflow_run_id.map(|id| id.0))
        .bind(state_to_str(row.state))
        .bind(&row.metadata)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.completed_at)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("create a2a_task: {e}")))?;
        Ok(())
    }

    async fn update_state(
        &self,
        tenant_id: TenantId,
        id: TaskId,
        state: TaskState,
    ) -> Result<(), OrkError> {
        let now = Utc::now();
        let completed_at = if is_terminal(state) { Some(now) } else { None };

        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        sqlx::query(
            r#"
            UPDATE a2a_tasks
            SET state = $1,
                updated_at = $2,
                completed_at = COALESCE($3, completed_at)
            WHERE id = $4 AND tenant_id = $5
            "#,
        )
        .bind(state_to_str(state))
        .bind(now)
        .bind(completed_at)
        .bind(id.0)
        .bind(tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("update a2a_task state: {e}")))?;
        Ok(())
    }

    async fn get_task(
        &self,
        tenant_id: TenantId,
        id: TaskId,
    ) -> Result<Option<A2aTaskRow>, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        let row = sqlx::query_as::<_, A2aTaskRowSql>(
            r#"
            SELECT id, context_id, tenant_id, agent_id, parent_task_id, workflow_run_id,
                   state, metadata, created_at, updated_at, completed_at
            FROM a2a_tasks
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("get a2a_task: {e}")))?;

        row.map(A2aTaskRowSql::into_row).transpose()
    }

    async fn append_message(&self, row: &A2aMessageRow) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            INSERT INTO a2a_messages (id, task_id, role, parts, metadata, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(row.id.0)
        .bind(row.task_id.0)
        .bind(&row.role)
        .bind(&row.parts)
        .bind(&row.metadata)
        .bind(row.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("append a2a_message: {e}")))?;
        Ok(())
    }

    async fn list_messages(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
        history_length: Option<u32>,
    ) -> Result<Vec<A2aMessageRow>, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` join once SET LOCAL is wired.
        let limit = i64::from(history_length.unwrap_or(10_000));
        let rows = sqlx::query_as::<_, A2aMessageRowSql>(
            r#"
            SELECT m.id, m.task_id, m.role, m.parts, m.metadata, m.created_at
            FROM a2a_messages m
            JOIN a2a_tasks t ON t.id = m.task_id
            WHERE t.id = $1 AND t.tenant_id = $2
            ORDER BY m.seq ASC
            LIMIT $3
            "#,
        )
        .bind(task_id.0)
        .bind(tenant_id.0)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list a2a_messages: {e}")))?;
        Ok(rows.into_iter().map(A2aMessageRowSql::into_row).collect())
    }

    async fn list_tasks_in_tenant(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<A2aTaskRow>, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        let rows = sqlx::query_as::<_, A2aTaskRowSql>(
            r#"
            SELECT id, context_id, tenant_id, agent_id, parent_task_id, workflow_run_id,
                   state, metadata, created_at, updated_at, completed_at
            FROM a2a_tasks
            WHERE tenant_id = $1
            ORDER BY created_at DESC
            LIMIT $2
            "#,
        )
        .bind(tenant_id.0)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list a2a_tasks: {e}")))?;

        rows.into_iter().map(A2aTaskRowSql::into_row).collect()
    }
}

#[derive(sqlx::FromRow)]
struct A2aTaskRowSql {
    id: uuid::Uuid,
    context_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    agent_id: String,
    parent_task_id: Option<uuid::Uuid>,
    workflow_run_id: Option<uuid::Uuid>,
    state: String,
    metadata: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl A2aTaskRowSql {
    fn into_row(self) -> Result<A2aTaskRow, OrkError> {
        Ok(A2aTaskRow {
            id: TaskId(self.id),
            context_id: ContextId(self.context_id),
            tenant_id: TenantId(self.tenant_id),
            agent_id: self.agent_id,
            parent_task_id: self.parent_task_id.map(TaskId),
            workflow_run_id: self.workflow_run_id.map(WorkflowRunId),
            state: parse_state(&self.state)?,
            metadata: self.metadata,
            created_at: self.created_at,
            updated_at: self.updated_at,
            completed_at: self.completed_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct A2aMessageRowSql {
    id: uuid::Uuid,
    task_id: uuid::Uuid,
    role: String,
    parts: serde_json::Value,
    metadata: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl A2aMessageRowSql {
    fn into_row(self) -> A2aMessageRow {
        A2aMessageRow {
            id: MessageId(self.id),
            task_id: TaskId(self.task_id),
            role: self.role,
            parts: self.parts,
            metadata: self.metadata,
            created_at: self.created_at,
        }
    }
}
