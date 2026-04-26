//! Postgres implementation of [`ork_core::ports::artifact_meta_repo::ArtifactMetaRepo`]. ADR-0016.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::ContextId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::artifact_meta_repo::{ArtifactMetaRepo, ArtifactRow};
use ork_core::ports::artifact_store::{ArtifactRef, ArtifactScope, ArtifactSummary, NO_CONTEXT_ID};
use sqlx::PgPool;
use uuid::Uuid;

fn ctx_uuid(c: Option<ContextId>) -> Uuid {
    c.map(|x| x.0).unwrap_or(NO_CONTEXT_ID)
}

fn from_ctx_u(u: Uuid) -> Option<ContextId> {
    if u == NO_CONTEXT_ID {
        None
    } else {
        Some(ContextId(u))
    }
}

pub struct PgArtifactMetaRepo {
    pool: PgPool,
}

impl PgArtifactMetaRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ArtifactMetaRepo for PgArtifactMetaRepo {
    async fn upsert(&self, row: &ArtifactRow) -> Result<(), OrkError> {
        let ctx = ctx_uuid(row.context_id);
        sqlx::query(
            r#"
            INSERT INTO artifacts (
              tenant_id, context_id, name, version, scheme, storage_key, mime, size,
              created_at, created_by, task_id, labels, etag
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            ON CONFLICT (tenant_id, context_id, name, version) DO UPDATE SET
              scheme = EXCLUDED.scheme,
              storage_key = EXCLUDED.storage_key,
              mime = EXCLUDED.mime,
              size = EXCLUDED.size,
              created_by = EXCLUDED.created_by,
              task_id = EXCLUDED.task_id,
              labels = EXCLUDED.labels,
              etag = EXCLUDED.etag
            "#,
        )
        .bind(row.tenant_id.0)
        .bind(ctx)
        .bind(&row.name)
        .bind(
            i32::try_from(row.version)
                .map_err(|e| OrkError::Internal(format!("artifact version: {e}")))?,
        )
        .bind(&row.scheme)
        .bind(&row.storage_key)
        .bind(&row.mime)
        .bind(row.size)
        .bind(row.created_at)
        .bind(&row.created_by)
        .bind(row.task_id.map(|t| t.0))
        .bind(&row.labels)
        .bind(&row.etag)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(())
    }

    async fn latest_version(
        &self,
        tenant: TenantId,
        context: Option<ContextId>,
        name: &str,
    ) -> Result<Option<u32>, OrkError> {
        let ctx = ctx_uuid(context);
        let n: Option<i32> = sqlx::query_scalar(
            r#"
            SELECT max(version) FROM artifacts
            WHERE tenant_id = $1 AND context_id = $2 AND name = $3
            "#,
        )
        .bind(tenant.0)
        .bind(ctx)
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(n.map(|v| v as u32))
    }

    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
        label_eq: Option<(&str, &str)>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        let ctx = ctx_uuid(scope.context_id);
        let rows: Vec<(
            String,
            String,
            i32,
            Option<String>,
            i64,
            DateTime<Utc>,
            serde_json::Value,
        )> = match (prefix, label_eq) {
            (None, None) => sqlx::query_as(
                r#"
                SELECT scheme, name, version, mime, size, created_at, labels
                FROM artifacts
                WHERE tenant_id = $1 AND context_id = $2
                ORDER BY name, version DESC
                "#,
            )
            .bind(scope.tenant_id.0)
            .bind(ctx)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| OrkError::Database(e.to_string()))?,
            (Some(p), None) => {
                let like = format!("{p}%");
                sqlx::query_as(
                    r#"
                    SELECT scheme, name, version, mime, size, created_at, labels
                    FROM artifacts
                    WHERE tenant_id = $1 AND context_id = $2 AND name LIKE $3
                    ORDER BY name, version DESC
                    "#,
                )
                .bind(scope.tenant_id.0)
                .bind(ctx)
                .bind(like)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| OrkError::Database(e.to_string()))?
            }
            (None, Some((k, v))) => sqlx::query_as(
                r#"
                SELECT scheme, name, version, mime, size, created_at, labels
                FROM artifacts
                WHERE tenant_id = $1 AND context_id = $2 AND labels->>$3 = $4
                ORDER BY name, version DESC
                "#,
            )
            .bind(scope.tenant_id.0)
            .bind(ctx)
            .bind(k)
            .bind(v)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| OrkError::Database(e.to_string()))?,
            (Some(p), Some((k, v))) => {
                let like = format!("{p}%");
                sqlx::query_as(
                    r#"
                    SELECT scheme, name, version, mime, size, created_at, labels
                    FROM artifacts
                    WHERE tenant_id = $1 AND context_id = $2 AND name LIKE $3
                        AND labels->>$4 = $5
                    ORDER BY name, version DESC
                    "#,
                )
                .bind(scope.tenant_id.0)
                .bind(ctx)
                .bind(like)
                .bind(k)
                .bind(v)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| OrkError::Database(e.to_string()))?
            }
        };
        let mut out: Vec<ArtifactSummary> = Vec::with_capacity(rows.len());
        for (scheme, name, version, mime, size, created_at, labels) in rows {
            let lmap: std::collections::BTreeMap<String, String> = match labels.as_object() {
                Some(o) => o.iter().map(|(k, v)| (k.clone(), v.to_string())).collect(),
                None => std::collections::BTreeMap::new(),
            };
            out.push(ArtifactSummary {
                scheme,
                name,
                version: version as u32,
                mime,
                size: size as u64,
                created_at,
                labels: lmap,
            });
        }
        Ok(out)
    }

    async fn delete_version(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        let v = if r#ref.version == 0 {
            return Err(OrkError::Validation(
                "delete_version requires a concrete version".into(),
            ));
        } else {
            r#ref.version
        } as i32;
        let ctx = ctx_uuid(r#ref.context_id);
        sqlx::query(
            r#"
            DELETE FROM artifacts
            WHERE tenant_id = $1 AND context_id = $2 AND name = $3 AND version = $4
            "#,
        )
        .bind(r#ref.tenant_id.0)
        .bind(ctx)
        .bind(&r#ref.name)
        .bind(v)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(())
    }

    async fn delete_all_versions(
        &self,
        scope: &ArtifactScope,
        name: &str,
    ) -> Result<u32, OrkError> {
        let ctx = ctx_uuid(scope.context_id);
        let r = sqlx::query(
            r#"
            DELETE FROM artifacts
            WHERE tenant_id = $1 AND context_id = $2 AND name = $3
            "#,
        )
        .bind(scope.tenant_id.0)
        .bind(ctx)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(r.rows_affected() as u32)
    }

    async fn eligible_for_sweep(
        &self,
        now: DateTime<Utc>,
        default_days: u32,
        task_days: u32,
    ) -> Result<Vec<ArtifactRef>, OrkError> {
        // ADR-0020: RLS; until then explicit tenant_id filter in worker per tenant or global query for ops
        let rows: Vec<(Uuid, Uuid, String, i32, String, String, String)> = sqlx::query_as(
            r#"
            SELECT a.tenant_id, a.context_id, a.name, a.version, a.scheme, a.storage_key, a.etag
            FROM artifacts a
            LEFT JOIN a2a_tasks t ON t.id = a.task_id
            INNER JOIN tenants ten ON ten.id = a.tenant_id
            WHERE coalesce(a.labels->>'pinned', '') <> 'true'
              AND (
                a.created_at < $1 - (
                  COALESCE((ten.settings->>'artifact_retention_days')::int, $2::int)
                  * interval '1 day'
                )
                OR (
                  a.task_id IS NOT NULL
                  AND t.state IN ('completed', 'failed', 'canceled', 'rejected')
                  AND t.completed_at IS NOT NULL
                  AND t.completed_at < $1 - ($3::bigint * interval '1 day')
                )
              )
            "#,
        )
        .bind(now)
        .bind(i64::from(default_days))
        .bind(i64::from(task_days))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for (tid, ctx_id, name, version, scheme, _sk, etag) in rows {
            out.push(ArtifactRef {
                scheme,
                tenant_id: TenantId(tid),
                context_id: from_ctx_u(ctx_id),
                name,
                version: version as u32,
                etag,
            });
        }
        Ok(out)
    }

    async fn add_label(&self, r#ref: &ArtifactRef, k: &str, v: &str) -> Result<(), OrkError> {
        let ver = r#ref.version as i32;
        let ctx = ctx_uuid(r#ref.context_id);
        sqlx::query(
            r#"
            UPDATE artifacts SET labels = jsonb_set(
                coalesce(labels, '{}'::jsonb), ARRAY[$4::text], to_jsonb($5::text), true
            )
            WHERE tenant_id = $1 AND context_id = $2 AND name = $3 AND version = $6
            "#,
        )
        .bind(r#ref.tenant_id.0)
        .bind(ctx)
        .bind(&r#ref.name)
        .bind(k)
        .bind(v)
        .bind(ver)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(e.to_string()))?;
        Ok(())
    }
}
