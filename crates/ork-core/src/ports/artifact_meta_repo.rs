//! ADR-0016 — Postgres index over artifact metadata (fast `list_artifacts`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::{ContextId, TaskId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};

use super::artifact_store::{ArtifactRef, ArtifactScope, ArtifactSummary};

/// Row in the `artifacts` table; mirrors
/// [`migrations/007_artifacts.sql`](../../migrations/007_artifacts.sql).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRow {
    pub tenant_id: TenantId,
    pub context_id: Option<ContextId>,
    pub name: String,
    pub version: u32,
    pub scheme: String,
    /// Backend-specific object key (path, S3 key, etc.).
    pub storage_key: String,
    pub mime: Option<String>,
    pub size: i64,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<String>,
    pub task_id: Option<TaskId>,
    pub labels: serde_json::Value,
    pub etag: String,
}

#[async_trait]
pub trait ArtifactMetaRepo: Send + Sync {
    async fn upsert(&self, row: &ArtifactRow) -> Result<(), OrkError>;
    async fn latest_version(
        &self,
        tenant: TenantId,
        context: Option<ContextId>,
        name: &str,
    ) -> Result<Option<u32>, OrkError>;
    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
        label_eq: Option<(&str, &str)>,
    ) -> Result<Vec<ArtifactSummary>, OrkError>;
    async fn delete_version(&self, r#ref: &ArtifactRef) -> Result<(), OrkError>;
    async fn delete_all_versions(&self, scope: &ArtifactScope, name: &str)
    -> Result<u32, OrkError>;
    async fn eligible_for_sweep(
        &self,
        now: DateTime<Utc>,
        default_days: u32,
        task_days: u32,
    ) -> Result<Vec<ArtifactRef>, OrkError>;
    async fn add_label(&self, r#ref: &ArtifactRef, k: &str, v: &str) -> Result<(), OrkError>;
}
