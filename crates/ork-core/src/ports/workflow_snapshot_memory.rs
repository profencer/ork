//! In-memory [`WorkflowSnapshotStore`](super::workflow_snapshot::WorkflowSnapshotStore) for tests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use ork_common::error::OrkError;
use serde_json::Value;

use super::workflow_snapshot::{RunStateBlob, SnapshotKey, SnapshotRow, WorkflowSnapshotStore};

#[derive(Debug, Default)]
pub struct MemoryWorkflowSnapshotStore {
    inner: Mutex<HashMap<SnapshotKey, SnapshotRow>>,
}

#[async_trait]
impl WorkflowSnapshotStore for MemoryWorkflowSnapshotStore {
    async fn save(
        &self,
        key: SnapshotKey,
        payload: Value,
        resume_schema: Value,
        run_state: RunStateBlob,
    ) -> Result<(), OrkError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| OrkError::Internal(format!("memory snapshot lock poisoned: {e}")))?;
        g.insert(
            key.clone(),
            SnapshotRow {
                key,
                payload,
                resume_schema,
                run_state,
                created_at: Utc::now(),
                consumed_at: None,
            },
        );
        Ok(())
    }

    async fn take(&self, key: SnapshotKey) -> Result<Option<SnapshotRow>, OrkError> {
        let g = self
            .inner
            .lock()
            .map_err(|e| OrkError::Internal(format!("memory snapshot lock poisoned: {e}")))?;
        Ok(g.get(&key).cloned())
    }

    async fn list_pending(&self) -> Result<Vec<SnapshotRow>, OrkError> {
        let g = self
            .inner
            .lock()
            .map_err(|e| OrkError::Internal(format!("memory snapshot lock poisoned: {e}")))?;
        Ok(g.values()
            .filter(|r| r.consumed_at.is_none())
            .cloned()
            .collect())
    }

    async fn mark_consumed(&self, key: SnapshotKey) -> Result<(), OrkError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| OrkError::Internal(format!("memory snapshot lock poisoned: {e}")))?;
        if let Some(r) = g.get_mut(&key) {
            r.consumed_at = Some(Utc::now());
        }
        Ok(())
    }
}
