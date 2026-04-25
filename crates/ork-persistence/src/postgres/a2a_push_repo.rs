//! Postgres-backed [`A2aPushConfigRepository`] (ADR-0009 push-notification config
//! slice, pulled forward by the ADR-0008 plan so the JSON-RPC dispatcher can serve
//! `tasks/pushNotificationConfig/{set,get}` end-to-end).
//!
//! Schema lives in `migrations/004_a2a_endpoints.sql`. Tenant filter is explicit
//! (`WHERE tenant_id = $1`) until ADR-0020 wires `SET LOCAL`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::a2a_push_repo::{A2aPushConfigRepository, A2aPushConfigRow};
use sqlx::PgPool;
use url::Url;

pub struct PgA2aPushConfigRepository {
    pool: PgPool,
}

impl PgA2aPushConfigRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl A2aPushConfigRepository for PgA2aPushConfigRepository {
    async fn upsert(&self, row: &A2aPushConfigRow) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            INSERT INTO a2a_push_configs
                (id, task_id, tenant_id, url, token, authentication, metadata, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO UPDATE
              SET url = EXCLUDED.url,
                  token = EXCLUDED.token,
                  authentication = EXCLUDED.authentication,
                  metadata = EXCLUDED.metadata
            "#,
        )
        .bind(row.id)
        .bind(row.task_id.0)
        .bind(row.tenant_id.0)
        .bind(row.url.as_str())
        .bind(row.token.as_deref())
        .bind(row.authentication.as_ref())
        .bind(&row.metadata)
        .bind(row.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("upsert a2a_push_config: {e}")))?;
        Ok(())
    }

    async fn get(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
    ) -> Result<Option<A2aPushConfigRow>, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        let row = sqlx::query_as::<_, A2aPushConfigRowSql>(
            r#"
            SELECT id, task_id, tenant_id, url, token, authentication, metadata, created_at
            FROM a2a_push_configs
            WHERE task_id = $1 AND tenant_id = $2
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(task_id.0)
        .bind(tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("get a2a_push_config: {e}")))?;

        row.map(A2aPushConfigRowSql::into_row).transpose()
    }

    async fn list_for_task(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
    ) -> Result<Vec<A2aPushConfigRow>, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        let rows = sqlx::query_as::<_, A2aPushConfigRowSql>(
            r#"
            SELECT id, task_id, tenant_id, url, token, authentication, metadata, created_at
            FROM a2a_push_configs
            WHERE task_id = $1 AND tenant_id = $2
            ORDER BY created_at ASC
            "#,
        )
        .bind(task_id.0)
        .bind(tenant_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list a2a_push_configs: {e}")))?;

        rows.into_iter()
            .map(A2aPushConfigRowSql::into_row)
            .collect()
    }

    async fn count_active_for_tenant(&self, tenant_id: TenantId) -> Result<u64, OrkError> {
        // ADR-0020: drop the explicit `tenant_id` filter once SET LOCAL is wired.
        let count: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM a2a_push_configs WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("count a2a_push_configs: {e}")))?;

        u64::try_from(count.0)
            .map_err(|e| OrkError::Database(format!("negative count from a2a_push_configs: {e}")))
    }

    async fn delete_terminal_after(&self, older_than: DateTime<Utc>) -> Result<u64, OrkError> {
        let result = sqlx::query(
            r#"
            DELETE FROM a2a_push_configs
            WHERE task_id IN (
                SELECT id FROM a2a_tasks
                WHERE completed_at IS NOT NULL AND completed_at < $1
            )
            "#,
        )
        .bind(older_than)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("janitor delete a2a_push_configs: {e}")))?;

        Ok(result.rows_affected())
    }
}

#[derive(sqlx::FromRow)]
struct A2aPushConfigRowSql {
    id: uuid::Uuid,
    task_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    url: String,
    token: Option<String>,
    authentication: Option<serde_json::Value>,
    metadata: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl A2aPushConfigRowSql {
    fn into_row(self) -> Result<A2aPushConfigRow, OrkError> {
        Ok(A2aPushConfigRow {
            id: self.id,
            task_id: TaskId(self.task_id),
            tenant_id: TenantId(self.tenant_id),
            url: Url::parse(&self.url)
                .map_err(|e| OrkError::Database(format!("invalid push url stored: {e}")))?,
            token: self.token,
            authentication: self.authentication,
            metadata: self.metadata,
            created_at: self.created_at,
        })
    }
}
