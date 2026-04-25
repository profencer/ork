//! Postgres-backed [`A2aPushDeadLetterRepository`] (ADR-0009 push notification
//! dead-letter ledger). Schema lives in `migrations/005_push_notifications.sql`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::a2a_push_dead_letter_repo::{
    A2aPushDeadLetterRepository, A2aPushDeadLetterRow,
};
use sqlx::PgPool;

pub struct PgA2aPushDeadLetterRepository {
    pool: PgPool,
}

impl PgA2aPushDeadLetterRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl A2aPushDeadLetterRepository for PgA2aPushDeadLetterRepository {
    async fn insert(&self, row: &A2aPushDeadLetterRow) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            INSERT INTO a2a_push_dead_letter (
                id, task_id, tenant_id, config_id, url,
                last_status, last_error, attempts, payload, failed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(row.id)
        .bind(row.task_id.0)
        .bind(row.tenant_id.0)
        .bind(row.config_id)
        .bind(&row.url)
        .bind(row.last_status)
        .bind(row.last_error.as_deref())
        .bind(row.attempts)
        .bind(&row.payload)
        .bind(row.failed_at)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("insert a2a_push_dead_letter: {e}")))?;
        Ok(())
    }

    async fn list_for_tenant(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<A2aPushDeadLetterRow>, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        let rows = sqlx::query_as::<_, A2aPushDeadLetterRowSql>(
            r#"
            SELECT id, task_id, tenant_id, config_id, url,
                   last_status, last_error, attempts, payload, failed_at
            FROM a2a_push_dead_letter
            WHERE tenant_id = $1
            ORDER BY failed_at DESC
            LIMIT $2
            "#,
        )
        .bind(tenant_id.0)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list a2a_push_dead_letter: {e}")))?;

        Ok(rows
            .into_iter()
            .map(A2aPushDeadLetterRowSql::into_row)
            .collect())
    }
}

#[derive(sqlx::FromRow)]
struct A2aPushDeadLetterRowSql {
    id: uuid::Uuid,
    task_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    config_id: Option<uuid::Uuid>,
    url: String,
    last_status: Option<i32>,
    last_error: Option<String>,
    attempts: i32,
    payload: serde_json::Value,
    failed_at: DateTime<Utc>,
}

impl A2aPushDeadLetterRowSql {
    fn into_row(self) -> A2aPushDeadLetterRow {
        A2aPushDeadLetterRow {
            id: self.id,
            task_id: TaskId(self.task_id),
            tenant_id: TenantId(self.tenant_id),
            config_id: self.config_id,
            url: self.url,
            last_status: self.last_status,
            last_error: self.last_error,
            attempts: self.attempts,
            payload: self.payload,
            failed_at: self.failed_at,
        }
    }
}
