//! ADR-0020 §`Secrets handling`: integration test against a real
//! Postgres for the tenant secrets envelope-encryption flow.
//!
//! The test exercises three things:
//!
//!   1. After `update_settings` writes a PAT, the row's `settings` JSONB
//!      stores ciphertext (the on-disk value is the `enc:v1:` marker
//!      shape, NOT the plaintext token).
//!   2. A subsequent `get_by_id` returns the plaintext PAT (decrypt-on-read).
//!   3. Pre-migration rows (NULL `dek_wrapped`) read back unchanged —
//!      the cipher passes through plaintext when the marker is absent.
//!
//! Skipped when `DATABASE_URL` is unset.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use ork_common::types::TenantId;
use ork_core::models::tenant::{CreateTenantRequest, UpdateTenantSettingsRequest};
use ork_core::ports::repository::TenantRepository;
use ork_persistence::postgres::{create_pool, tenant_repo::PgTenantRepository};
use ork_security::{JwtSecretKekKms, TenantSecretsCipher};
use secrecy::SecretString;
use sqlx::PgPool;
use sqlx::Row;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    create_pool(&url, 2).await.ok()
}

fn cipher() -> Arc<TenantSecretsCipher> {
    Arc::new(TenantSecretsCipher::new(Arc::new(JwtSecretKekKms::new(
        SecretString::from("rls-test-secret"),
    ))))
}

#[tokio::test]
async fn update_settings_persists_sealed_value_and_get_returns_plaintext() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping ADR-0020 tenant secrets smoke");
        return;
    };
    let repo = PgTenantRepository::new(pool.clone()).with_cipher(cipher());

    let slug = format!("ts-{}", Uuid::now_v7());
    let created = repo
        .create(&CreateTenantRequest {
            name: "secrets test".into(),
            slug: slug.clone(),
        })
        .await
        .expect("create");

    // Confirm DEK columns are populated on create when cipher is wired.
    let row = sqlx::query("SELECT dek_wrapped, dek_version FROM tenants WHERE id = $1")
        .bind(created.id.0)
        .fetch_one(&pool)
        .await
        .expect("read dek cols");
    let dek_wrapped: Option<Vec<u8>> = row.try_get("dek_wrapped").expect("dek_wrapped");
    let dek_version: Option<String> = row.try_get("dek_version").expect("dek_version");
    assert!(dek_wrapped.is_some(), "DEK must be minted on tenant create");
    assert_eq!(dek_version.as_deref(), Some("v1"));

    // Persist a fake PAT.
    let updated = repo
        .update_settings(
            created.id,
            &UpdateTenantSettingsRequest {
                github_token: Some("ghp_test_plaintext_value".into()),
                gitlab_token: None,
                gitlab_base_url: None,
                default_repos: None,
                mcp_servers: None,
                llm_providers: None,
                default_provider: None,
                default_model: None,
                artifact_retention_days: None,
                scope_allowlist: None,
            },
        )
        .await
        .expect("update_settings");
    // Returned `Tenant` carries plaintext for caller convenience.
    assert_eq!(
        updated.settings.github_token_encrypted.as_deref(),
        Some("ghp_test_plaintext_value")
    );

    // Inspect the on-disk JSONB directly: must be the sealed marker form.
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT settings FROM tenants WHERE id = $1")
            .bind(created.id.0)
            .fetch_one(&pool)
            .await
            .expect("read settings json");
    let on_disk = stored
        .get("github_token_encrypted")
        .and_then(|v| v.as_str())
        .expect("github_token_encrypted must be a string");
    assert!(
        on_disk.starts_with("enc:v1:"),
        "expected sealed marker on disk, got: {on_disk}"
    );
    assert!(
        !on_disk.contains("ghp_test_plaintext_value"),
        "plaintext PAT must not appear on disk (got: {on_disk})"
    );

    // Read-back: get_by_id returns plaintext.
    let fetched = repo.get_by_id(created.id).await.expect("get_by_id");
    assert_eq!(
        fetched.settings.github_token_encrypted.as_deref(),
        Some("ghp_test_plaintext_value")
    );

    // Cleanup.
    repo.delete(created.id).await.expect("delete");
}

/// Pre-migration rows (i.e. tenants that existed before
/// `migrations/012_tenant_security.sql` ran *and* before any
/// `update_settings` call after the cipher was wired in) carry plaintext
/// `*_encrypted` values and `dek_wrapped IS NULL`. The repo must read
/// them back without choking — the cipher's `try_open_field` returns
/// `Ok(None)` for non-marker values and the field passes through.
#[tokio::test]
async fn pre_migration_plaintext_row_reads_unchanged() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping ADR-0020 pre-migration smoke");
        return;
    };
    // Insert a row with plaintext PAT and NULL DEK columns to simulate
    // a pre-ADR-0020 row.
    let id = TenantId::new();
    let now = chrono::Utc::now();
    let slug = format!("ts-pre-{}", Uuid::now_v7());
    sqlx::query(
        r#"
        INSERT INTO tenants (id, name, slug, settings, created_at, updated_at)
        VALUES ($1, $2, $3, $4::jsonb, $5, $6)
        "#,
    )
    .bind(id.0)
    .bind("pre-migration tenant")
    .bind(&slug)
    .bind(serde_json::json!({
        "github_token_encrypted": "ghp_legacy_plaintext",
        "gitlab_token_encrypted": null,
        "gitlab_base_url": null,
        "default_repos": [],
    }))
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .expect("seed pre-migration tenant");

    let repo = PgTenantRepository::new(pool.clone()).with_cipher(cipher());
    let fetched = repo.get_by_id(id).await.expect("get_by_id");
    assert_eq!(
        fetched.settings.github_token_encrypted.as_deref(),
        Some("ghp_legacy_plaintext"),
        "pre-migration plaintext must read back unchanged"
    );

    // Cleanup.
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(id.0)
        .execute(&pool)
        .await
        .expect("delete pre-migration tenant");
}

/// When the repo is constructed without `with_cipher`, the legacy
/// behaviour is preserved — `*_encrypted` fields are stored verbatim.
/// This is the dev / test default, and protects deployments that have
/// not yet enabled the ADR-0020 cipher path.
#[tokio::test]
async fn legacy_mode_without_cipher_keeps_plaintext_on_disk() {
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset; skipping ADR-0020 legacy mode smoke");
        return;
    };
    let repo = PgTenantRepository::new(pool.clone());
    let slug = format!("ts-leg-{}", Uuid::now_v7());
    let created = repo
        .create(&CreateTenantRequest {
            name: "legacy".into(),
            slug,
        })
        .await
        .expect("create");

    let _ = repo
        .update_settings(
            created.id,
            &UpdateTenantSettingsRequest {
                github_token: Some("ghp_in_the_clear".into()),
                gitlab_token: None,
                gitlab_base_url: None,
                default_repos: None,
                mcp_servers: None,
                llm_providers: None,
                default_provider: None,
                default_model: None,
                artifact_retention_days: None,
                scope_allowlist: None,
            },
        )
        .await
        .expect("update_settings");

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT settings FROM tenants WHERE id = $1")
            .bind(created.id.0)
            .fetch_one(&pool)
            .await
            .expect("read settings");
    let on_disk = stored
        .get("github_token_encrypted")
        .and_then(|v| v.as_str())
        .expect("string");
    assert_eq!(
        on_disk, "ghp_in_the_clear",
        "without the cipher, the field must stay plaintext on disk"
    );

    repo.delete(created.id).await.expect("delete");
}
