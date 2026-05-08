//! Postgres adapter for the [`TenantRepository`] port.
//!
//! ADR-0020 §`Secrets handling`: when an [`Arc<TenantSecretsCipher>`] is
//! wired in, the `*_encrypted` fields on [`TenantSettings`] are sealed
//! at-rest under a per-tenant DEK, and the DEK is wrapped under the
//! KMS-managed KEK (the `tenants.dek_wrapped` / `dek_key_id` /
//! `dek_version` columns added by `migrations/012_tenant_security.sql`).
//!
//! On read the repo decrypts in place: callers see plaintext strings on
//! [`TenantSettings::github_token_encrypted`] / `gitlab_token_encrypted`
//! exactly as before. The encryption is invisible to consumers.
//!
//! Pre-migration shim: rows with `dek_wrapped IS NULL` (existing rows
//! from before this migration) are passed through unchanged on read —
//! `TenantSecretsCipher::try_open_field` recognises a missing
//! `enc:v1:` marker as plaintext. The next `update_settings` call mints
//! a fresh DEK, persists it, and writes any new sealed values; older
//! plaintext fields stay plaintext until the operator re-saves them or
//! runs `ork admin keys rotate --scope tenants` (deferred to a follow-up
//! commit).
//!
//! When the cipher is `None` (legacy mode — dev / tests without a
//! configured KMS), the repo behaves exactly as it did pre-ADR-0020:
//! all `*_encrypted` fields stay plaintext, no DEK columns are touched.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_security::{Ciphertext, TenantSecretsCipher};
use sqlx::PgPool;

use ork_core::models::tenant::{
    CreateTenantRequest, Tenant, TenantSettings, UpdateTenantSettingsRequest,
};
use ork_core::ports::repository::TenantRepository;

pub struct PgTenantRepository {
    pool: PgPool,
    /// `Some` activates ADR-0020 §`Secrets handling` envelope encryption.
    /// `None` keeps legacy plaintext behaviour for tests / dev.
    cipher: Option<Arc<TenantSecretsCipher>>,
}

impl PgTenantRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool, cipher: None }
    }

    /// ADR-0020: enable per-tenant envelope encryption for the
    /// `*_encrypted` fields.
    #[must_use]
    pub fn with_cipher(mut self, cipher: Arc<TenantSecretsCipher>) -> Self {
        self.cipher = Some(cipher);
        self
    }

    /// Read the wrapped DEK columns from `tenants` for `id`. Returns
    /// `None` when `dek_wrapped IS NULL` (pre-migration row).
    async fn read_wrapped_dek(&self, id: TenantId) -> Result<Option<Ciphertext>, OrkError> {
        let row: Option<DekRow> = sqlx::query_as::<_, DekRow>(
            "SELECT dek_wrapped, dek_key_id, dek_version FROM tenants WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("read tenant dek: {e}")))?;
        Ok(row.and_then(|r| {
            r.dek_wrapped.map(|bytes| Ciphertext {
                bytes,
                key_id: r.dek_key_id,
                version: r.dek_version,
            })
        }))
    }

    /// Mint a fresh wrapped DEK for `id` and persist it. Returns the
    /// stored [`Ciphertext`] so the same call can immediately encrypt
    /// new field values without a re-read.
    async fn ensure_wrapped_dek(
        &self,
        id: TenantId,
        cipher: &TenantSecretsCipher,
    ) -> Result<Ciphertext, OrkError> {
        if let Some(existing) = self.read_wrapped_dek(id).await? {
            return Ok(existing);
        }
        let (_dek_bytes, wrapped) = cipher.mint_dek().await?;
        sqlx::query(
            "UPDATE tenants SET dek_wrapped = $1, dek_key_id = $2, dek_version = $3 \
             WHERE id = $4",
        )
        .bind(&wrapped.bytes)
        .bind(&wrapped.key_id)
        .bind(&wrapped.version)
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("persist tenant dek: {e}")))?;
        tracing::info!(
            tenant_id = %id,
            "ADR-0020: minted fresh DEK for tenant"
        );
        Ok(wrapped)
    }

    /// Decrypt-in-place every `*_encrypted` field on `settings`. Fields
    /// that lack the `enc:v1:` marker (pre-migration plaintext) pass
    /// through unchanged. The field-name argument to
    /// [`TenantSecretsCipher::try_open_field`] is bound into the AAD
    /// per ADR-0020 review-finding M2 so a hostile column swap is
    /// rejected at AEAD time.
    async fn decrypt_in_place(
        &self,
        id: TenantId,
        settings: &mut TenantSettings,
    ) -> Result<(), OrkError> {
        let Some(cipher) = self.cipher.as_ref() else {
            return Ok(()); // legacy mode
        };
        let Some(wrapped) = self.read_wrapped_dek(id).await? else {
            return Ok(()); // pre-migration row, all fields are already plaintext
        };
        for (name, field) in [
            ("github_token", &mut settings.github_token_encrypted),
            ("gitlab_token", &mut settings.gitlab_token_encrypted),
        ] {
            if let Some(value) = field.as_ref()
                && let Some(plain) = cipher.try_open_field(id, name, &wrapped, value).await?
            {
                *field = Some(plain);
            }
        }
        Ok(())
    }

    /// Encrypt every `*_encrypted` field on `settings` in place. Used
    /// before persisting an updated row when a cipher is configured.
    async fn encrypt_in_place(
        &self,
        id: TenantId,
        wrapped: &Ciphertext,
        cipher: &TenantSecretsCipher,
        settings: &mut TenantSettings,
    ) -> Result<(), OrkError> {
        for (name, field) in [
            ("github_token", &mut settings.github_token_encrypted),
            ("gitlab_token", &mut settings.gitlab_token_encrypted),
        ] {
            if let Some(value) = field.as_ref()
                && !value.starts_with("enc:v1:")
            {
                *field = Some(cipher.seal_field(id, name, wrapped, value).await?);
            }
        }
        Ok(())
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

        // Mint the wrapped DEK eagerly when a cipher is configured. The
        // alternative — lazily on first `*_encrypted` write — works but
        // muddies the migration story (a tenant can sit DEK-less for a
        // long time). Eager is one extra KMS round-trip per tenant
        // create, which is bounded.
        let (dek_wrapped, dek_key_id, dek_version) = match &self.cipher {
            Some(cipher) => {
                let (_dek_bytes, wrapped) = cipher.mint_dek().await?;
                (Some(wrapped.bytes), wrapped.key_id, wrapped.version)
            }
            None => (None, None, None),
        };

        sqlx::query(
            r#"
            INSERT INTO tenants (
                id, name, slug, settings, created_at, updated_at,
                dek_wrapped, dek_key_id, dek_version
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(id.0)
        .bind(&req.name)
        .bind(&req.slug)
        .bind(&settings_json)
        .bind(now)
        .bind(now)
        .bind(&dek_wrapped)
        .bind(&dek_key_id)
        .bind(&dek_version)
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

        let mut tenant = row.into_tenant()?;
        self.decrypt_in_place(tenant.id, &mut tenant.settings)
            .await?;
        Ok(tenant)
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

        let mut tenant = row.into_tenant()?;
        self.decrypt_in_place(tenant.id, &mut tenant.settings)
            .await?;
        Ok(tenant)
    }

    async fn list(&self) -> Result<Vec<Tenant>, OrkError> {
        let rows = sqlx::query_as::<_, TenantRow>(
            "SELECT id, name, slug, settings, created_at, updated_at FROM tenants ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list tenants: {e}")))?;

        let mut tenants: Vec<Tenant> = rows
            .into_iter()
            .map(|r| r.into_tenant())
            .collect::<Result<_, _>>()?;
        for t in &mut tenants {
            self.decrypt_in_place(t.id, &mut t.settings).await?;
        }
        Ok(tenants)
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

        // ADR-0020: encrypt the new `*_encrypted` field values before
        // persisting. Eagerly mints the DEK if the row was created
        // before this migration ran. After persisting, return the
        // tenant with plaintext fields (caller doesn't see the
        // ciphertext).
        let plaintext_for_return = (
            tenant.settings.github_token_encrypted.clone(),
            tenant.settings.gitlab_token_encrypted.clone(),
        );
        if let Some(cipher) = &self.cipher {
            let wrapped = self.ensure_wrapped_dek(id, cipher).await?;
            self.encrypt_in_place(id, &wrapped, cipher, &mut tenant.settings)
                .await?;
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
        // Restore plaintext on the returned `Tenant` so callers don't
        // accidentally see ciphertext from a fresh update.
        tenant.settings.github_token_encrypted = plaintext_for_return.0;
        tenant.settings.gitlab_token_encrypted = plaintext_for_return.1;
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

#[derive(sqlx::FromRow)]
struct DekRow {
    dek_wrapped: Option<Vec<u8>>,
    dek_key_id: Option<String>,
    dek_version: Option<String>,
}
