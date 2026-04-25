//! Postgres-backed [`A2aSigningKeyRepository`] (ADR-0009 push notification
//! signing). Schema lives in `migrations/005_push_notifications.sql`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use ork_core::ports::a2a_signing_key_repo::{A2aSigningKeyRepository, A2aSigningKeyRow};
use sqlx::PgPool;
use uuid::Uuid;

pub struct PgA2aSigningKeyRepository {
    pool: PgPool,
}

impl PgA2aSigningKeyRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl A2aSigningKeyRepository for PgA2aSigningKeyRepository {
    async fn insert(&self, row: &A2aSigningKeyRow) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            INSERT INTO a2a_signing_keys (
                id, kid, alg, public_key_jwk, private_key_pem_encrypted,
                private_key_nonce, created_at, activates_at, expires_at, rotated_out_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(row.id)
        .bind(&row.kid)
        .bind(&row.alg)
        .bind(&row.public_key_jwk)
        .bind(&row.private_key_pem_encrypted)
        .bind(&row.private_key_nonce)
        .bind(row.created_at)
        .bind(row.activates_at)
        .bind(row.expires_at)
        .bind(row.rotated_out_at)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("insert a2a_signing_key: {e}")))?;
        Ok(())
    }

    async fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<A2aSigningKeyRow>, OrkError> {
        let rows = sqlx::query_as::<_, A2aSigningKeyRowSql>(
            r#"
            SELECT id, kid, alg, public_key_jwk, private_key_pem_encrypted,
                   private_key_nonce, created_at, activates_at, expires_at, rotated_out_at
            FROM a2a_signing_keys
            WHERE expires_at > $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list active a2a_signing_keys: {e}")))?;

        Ok(rows
            .into_iter()
            .map(A2aSigningKeyRowSql::into_row)
            .collect())
    }

    async fn mark_rotated(&self, id: Uuid, at: DateTime<Utc>) -> Result<(), OrkError> {
        sqlx::query(
            r#"
            UPDATE a2a_signing_keys SET rotated_out_at = $1 WHERE id = $2
            "#,
        )
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("mark a2a_signing_key rotated: {e}")))?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct A2aSigningKeyRowSql {
    id: Uuid,
    kid: String,
    alg: String,
    public_key_jwk: serde_json::Value,
    private_key_pem_encrypted: Vec<u8>,
    private_key_nonce: Vec<u8>,
    created_at: DateTime<Utc>,
    activates_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    rotated_out_at: Option<DateTime<Utc>>,
}

impl A2aSigningKeyRowSql {
    fn into_row(self) -> A2aSigningKeyRow {
        A2aSigningKeyRow {
            id: self.id,
            kid: self.kid,
            alg: self.alg,
            public_key_jwk: self.public_key_jwk,
            private_key_pem_encrypted: self.private_key_pem_encrypted,
            private_key_nonce: self.private_key_nonce,
            created_at: self.created_at,
            activates_at: self.activates_at,
            expires_at: self.expires_at,
            rotated_out_at: self.rotated_out_at,
        }
    }
}
