//! Postgres implementation of [`ork_core::ports::webui_store::WebuiStore`] (ADR-0017).

use async_trait::async_trait;
use ork_a2a::ContextId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::webui_project_repo::WebuiProject;
use ork_core::ports::webui_store::{WebuiConversation, WebuiStore};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
    label: String,
}

#[derive(sqlx::FromRow)]
struct ConversationRow {
    id: Uuid,
    tenant_id: Uuid,
    project_id: Option<Uuid>,
    context_id: Uuid,
    label: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

pub struct PgWebuiStore {
    pool: PgPool,
}

impl PgWebuiStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WebuiStore for PgWebuiStore {
    async fn list_projects(&self, tenant: TenantId) -> Result<Vec<WebuiProject>, OrkError> {
        let rows: Vec<ProjectRow> = sqlx::query_as(
            "SELECT id, created_at, label FROM webui_projects WHERE tenant_id = $1 ORDER BY created_at DESC",
        )
        .bind(tenant.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| WebuiProject {
                id: r.id,
                tenant_id: tenant,
                label: r.label,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn create_project(
        &self,
        tenant: TenantId,
        label: &str,
    ) -> Result<WebuiProject, OrkError> {
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO webui_projects (id, tenant_id, label, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(tenant.0)
        .bind(label)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(WebuiProject {
            id,
            tenant_id: tenant,
            label: label.to_string(),
            created_at: now,
        })
    }

    async fn delete_project(&self, tenant: TenantId, id: Uuid) -> Result<(), OrkError> {
        let r = sqlx::query("DELETE FROM webui_projects WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(tenant.0)
            .execute(&self.pool)
            .await
            .map_err(|e| OrkError::Database(e.to_string()))?;
        if r.rows_affected() == 0 {
            return Err(OrkError::NotFound(format!("webui project {id}")));
        }
        Ok(())
    }

    async fn list_conversations(
        &self,
        tenant: TenantId,
        project_id: Option<Uuid>,
    ) -> Result<Vec<WebuiConversation>, OrkError> {
        let rows: Vec<ConversationRow> = if let Some(pid) = project_id {
            sqlx::query_as(
                "SELECT id, tenant_id, project_id, context_id, label, created_at
                 FROM webui_conversations WHERE tenant_id = $1 AND project_id = $2
                 ORDER BY created_at DESC",
            )
            .bind(tenant.0)
            .bind(pid)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT id, tenant_id, project_id, context_id, label, created_at
                 FROM webui_conversations WHERE tenant_id = $1
                 ORDER BY created_at DESC",
            )
            .bind(tenant.0)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| OrkError::Database(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|r| WebuiConversation {
                id: r.id,
                tenant_id: TenantId(r.tenant_id),
                project_id: r.project_id,
                context_id: ContextId(r.context_id),
                label: r.label,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn create_conversation(
        &self,
        tenant: TenantId,
        project_id: Option<Uuid>,
        context_id: ContextId,
        label: &str,
    ) -> Result<WebuiConversation, OrkError> {
        if let Some(pid) = project_id {
            let c: (i64,) = sqlx::query_as(
                "SELECT count(*)::bigint FROM webui_projects WHERE id = $1 AND tenant_id = $2",
            )
            .bind(pid)
            .bind(tenant.0)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| OrkError::Database(e.to_string()))?;
            if c.0 == 0 {
                return Err(OrkError::NotFound("webui project".to_string()));
            }
        }
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        sqlx::query("INSERT INTO webui_conversations (id, tenant_id, project_id, context_id, label, created_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(tenant.0)
        .bind(project_id)
        .bind(context_id.0)
        .bind(label)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(WebuiConversation {
            id,
            tenant_id: tenant,
            project_id,
            context_id,
            label: label.to_string(),
            created_at: now,
        })
    }

    async fn get_conversation(
        &self,
        tenant: TenantId,
        id: Uuid,
    ) -> Result<Option<WebuiConversation>, OrkError> {
        let row: Option<ConversationRow> = sqlx::query_as(
            "SELECT id, tenant_id, project_id, context_id, label, created_at
             FROM webui_conversations WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id)
        .bind(tenant.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(row.map(|r| WebuiConversation {
            id: r.id,
            tenant_id: TenantId(r.tenant_id),
            project_id: r.project_id,
            context_id: ContextId(r.context_id),
            label: r.label,
            created_at: r.created_at,
        }))
    }
}
