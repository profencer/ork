//! Postgres implementation of [`WorkflowSnapshotStore`](ork_core::ports::workflow_snapshot::WorkflowSnapshotStore).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use ork_core::ports::workflow_snapshot::{
    RunStateBlob, SnapshotKey, SnapshotRow, WorkflowSnapshotStore,
};

pub struct PgWorkflowSnapshotRepository {
    pool: PgPool,
}

impl PgWorkflowSnapshotRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WorkflowSnapshotStore for PgWorkflowSnapshotRepository {
    async fn save(
        &self,
        key: SnapshotKey,
        payload: Value,
        resume_schema: Value,
        run_state: RunStateBlob,
    ) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            INSERT INTO workflow_snapshots (
                workflow_id, run_id, step_id, attempt,
                payload, resume_schema, run_state, created_at, consumed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, now(), NULL)
            ON CONFLICT (workflow_id, run_id, step_id, attempt) DO UPDATE SET
                payload = EXCLUDED.payload,
                resume_schema = EXCLUDED.resume_schema,
                run_state = EXCLUDED.run_state,
                created_at = EXCLUDED.created_at,
                consumed_at = NULL
            "#,
        )
        .bind(&key.workflow_id)
        .bind(key.run_id)
        .bind(&key.step_id)
        .bind(key.attempt as i32)
        .bind(payload)
        .bind(resume_schema)
        .bind(run_state.0)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("workflow_snapshots save: {e}")))?;
        Ok(())
    }

    async fn take(&self, key: SnapshotKey) -> Result<Option<SnapshotRow>, OrkError> {
        let row = sqlx::query_as::<_, SnapshotDbRow>(
            r#"
            SELECT workflow_id, run_id, step_id, attempt, payload, resume_schema,
                   run_state, created_at, consumed_at
            FROM workflow_snapshots
            WHERE workflow_id = $1 AND run_id = $2 AND step_id = $3 AND attempt = $4
            "#,
        )
        .bind(&key.workflow_id)
        .bind(key.run_id)
        .bind(&key.step_id)
        .bind(key.attempt as i32)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("workflow_snapshots take: {e}")))?;

        Ok(row.map(|r| r.into()))
    }

    async fn list_pending(&self) -> Result<Vec<SnapshotRow>, OrkError> {
        let rows = sqlx::query_as::<_, SnapshotDbRow>(
            r#"
            SELECT workflow_id, run_id, step_id, attempt, payload, resume_schema,
                   run_state, created_at, consumed_at
            FROM workflow_snapshots
            WHERE consumed_at IS NULL
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("workflow_snapshots list_pending: {e}")))?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn mark_consumed(&self, key: SnapshotKey) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            UPDATE workflow_snapshots
            SET consumed_at = now()
            WHERE workflow_id = $1 AND run_id = $2 AND step_id = $3 AND attempt = $4
            "#,
        )
        .bind(&key.workflow_id)
        .bind(key.run_id)
        .bind(&key.step_id)
        .bind(key.attempt as i32)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("workflow_snapshots mark_consumed: {e}")))?;
        Ok(())
    }
}

#[derive(Debug, sqlx::FromRow)]
struct SnapshotDbRow {
    workflow_id: String,
    run_id: Uuid,
    step_id: String,
    attempt: i32,
    payload: Value,
    resume_schema: Value,
    run_state: Value,
    created_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
}

impl From<SnapshotDbRow> for SnapshotRow {
    fn from(r: SnapshotDbRow) -> Self {
        Self {
            key: SnapshotKey {
                workflow_id: r.workflow_id,
                run_id: r.run_id,
                step_id: r.step_id,
                attempt: r.attempt.clamp(0, i32::MAX) as u32,
            },
            payload: r.payload,
            resume_schema: r.resume_schema,
            run_state: RunStateBlob(r.run_state),
            created_at: r.created_at,
            consumed_at: r.consumed_at,
        }
    }
}
