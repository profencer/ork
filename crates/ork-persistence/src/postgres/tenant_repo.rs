use async_trait::async_trait;
use chrono::Utc;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use sqlx::PgPool;

use ork_core::models::tenant::{
    CreateTenantRequest, Tenant, TenantSettings, UpdateTenantSettingsRequest,
};
use ork_core::ports::repository::TenantRepository;

pub struct PgTenantRepository {
    pool: PgPool,
}

impl PgTenantRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TenantRepository for PgTenantRepository {
    async fn create(&self, req: &CreateTenantRequest) -> Result<Tenant, OrkError> {
        let id = TenantId::new();
        let now = Utc::now();
        let settings = TenantSettings::default();
        let settings_json = serde_json::to_value(&settings)
            .map_err(|e| OrkError::Internal(format!("serialize settings: {e}")))?;

        sqlx::query(
            r#"
            INSERT INTO tenants (id, name, slug, settings, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(id.0)
        .bind(&req.name)
        .bind(&req.slug)
        .bind(&settings_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
                OrkError::Conflict(format!("tenant with slug '{}' already exists", req.slug))
            }
            _ => OrkError::Database(format!("create tenant: {e}")),
        })?;

        Ok(Tenant {
            id,
            name: req.name.clone(),
            slug: req.slug.clone(),
            settings,
            created_at: now,
            updated_at: now,
        })
    }

    async fn get_by_id(&self, id: TenantId) -> Result<Tenant, OrkError> {
        let row = sqlx::query_as::<_, TenantRow>(
            "SELECT id, name, slug, settings, created_at, updated_at FROM tenants WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("get tenant: {e}")))?
        .ok_or_else(|| OrkError::NotFound(format!("tenant {id}")))?;

        row.into_tenant()
    }

    async fn get_by_slug(&self, slug: &str) -> Result<Tenant, OrkError> {
        let row = sqlx::query_as::<_, TenantRow>(
            "SELECT id, name, slug, settings, created_at, updated_at FROM tenants WHERE slug = $1",
        )
        .bind(slug)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("get tenant by slug: {e}")))?
        .ok_or_else(|| OrkError::NotFound(format!("tenant with slug '{slug}'")))?;

        row.into_tenant()
    }

    async fn list(&self) -> Result<Vec<Tenant>, OrkError> {
        let rows = sqlx::query_as::<_, TenantRow>(
            "SELECT id, name, slug, settings, created_at, updated_at FROM tenants ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list tenants: {e}")))?;

        rows.into_iter().map(|r| r.into_tenant()).collect()
    }

    async fn update_settings(
        &self,
        id: TenantId,
        req: &UpdateTenantSettingsRequest,
    ) -> Result<Tenant, OrkError> {
        let mut tenant = self.get_by_id(id).await?;
        let now = Utc::now();

        if let Some(token) = &req.github_token {
            tenant.settings.github_token_encrypted = Some(token.clone());
        }
        if let Some(token) = &req.gitlab_token {
            tenant.settings.gitlab_token_encrypted = Some(token.clone());
        }
        if let Some(url) = &req.gitlab_base_url {
            tenant.settings.gitlab_base_url = Some(url.clone());
        }
        if let Some(repos) = &req.default_repos {
            tenant.settings.default_repos = repos.clone();
        }
        if let Some(servers) = &req.mcp_servers {
            tenant.settings.mcp_servers = servers.clone();
        }
        // ADR 0012: tenant LLM catalog overrides. `None` leaves the
        // existing list/value untouched; `Some(...)` replaces it
        // outright (an explicit empty `Vec` clears the catalog).
        if let Some(providers) = &req.llm_providers {
            tenant.settings.llm_providers = providers.clone();
        }
        if let Some(provider) = &req.default_provider {
            tenant.settings.default_provider = Some(provider.clone());
        }
        if let Some(model) = &req.default_model {
            tenant.settings.default_model = Some(model.clone());
        }

        let settings_json = serde_json::to_value(&tenant.settings)
            .map_err(|e| OrkError::Internal(format!("serialize settings: {e}")))?;

        sqlx::query("UPDATE tenants SET settings = $1, updated_at = $2 WHERE id = $3")
            .bind(&settings_json)
            .bind(now)
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| OrkError::Database(format!("update tenant settings: {e}")))?;

        tenant.updated_at = now;
        Ok(tenant)
    }

    async fn delete(&self, id: TenantId) -> Result<(), OrkError> {
        let result = sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| OrkError::Database(format!("delete tenant: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(OrkError::NotFound(format!("tenant {id}")));
        }
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct TenantRow {
    id: uuid::Uuid,
    name: String,
    slug: String,
    settings: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl TenantRow {
    fn into_tenant(self) -> Result<Tenant, OrkError> {
        let settings: TenantSettings = serde_json::from_value(self.settings)
            .map_err(|e| OrkError::Internal(format!("deserialize settings: {e}")))?;
        Ok(Tenant {
            id: TenantId(self.id),
            name: self.name,
            slug: self.slug,
            settings,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}
