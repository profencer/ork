//! Snapshot persistence for suspend/resume (ADR [`0050`](../../../docs/adrs/0050-code-first-workflow-dsl.md)).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Key for a single suspend point within a workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotKey {
    pub workflow_id: String,
    pub run_id: Uuid,
    pub step_id: String,
    pub attempt: u32,
}

/// Opaque serialised engine cursor + intermediate values for resume after restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStateBlob(pub Value);

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub key: SnapshotKey,
    pub payload: Value,
    pub resume_schema: Value,
    pub run_state: RunStateBlob,
    pub created_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait WorkflowSnapshotStore: Send + Sync {
    async fn save(
        &self,
        key: SnapshotKey,
        payload: Value,
        resume_schema: Value,
        run_state: RunStateBlob,
    ) -> Result<(), OrkError>;

    async fn take(&self, key: SnapshotKey) -> Result<Option<SnapshotRow>, OrkError>;

    async fn list_pending(&self) -> Result<Vec<SnapshotRow>, OrkError>;

    async fn mark_consumed(&self, key: SnapshotKey) -> Result<(), OrkError>;
}
