//! Persistence port for failed push-notification deliveries (ADR
//! [`0009`](../../../docs/adrs/0009-push-notifications.md) §`Delivery worker`).
//!
//! The delivery worker writes one row here when a payload exhausts its retry
//! budget (`config.push.retry_schedule_minutes`, default `[1, 5, 30]`). The row
//! captures enough context to replay manually from the ADR-0022 dashboards.
//!
//! Tenant scoping uses an explicit `WHERE tenant_id = $1` filter for now;
//! ADR-0020 will switch to `SET LOCAL app.current_tenant_id` per request.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct A2aPushDeadLetterRow {
    pub id: Uuid,
    pub task_id: TaskId,
    pub tenant_id: TenantId,
    pub config_id: Option<Uuid>,
    pub url: String,
    pub last_status: Option<i32>,
    pub last_error: Option<String>,
    pub attempts: i32,
    pub payload: serde_json::Value,
    pub failed_at: DateTime<Utc>,
}

#[async_trait]
pub trait A2aPushDeadLetterRepository: Send + Sync {
    /// Append one dead-letter row.
    async fn insert(&self, row: &A2aPushDeadLetterRow) -> Result<(), OrkError>;

    /// Return the most recent `limit` dead-letter rows for `tenant_id`.
    /// Backs admin / dashboard surfaces; not used by the worker itself.
    async fn list_for_tenant(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<A2aPushDeadLetterRow>, OrkError>;
}
