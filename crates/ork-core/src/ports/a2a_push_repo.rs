//! Persistence port for A2A push-notification configs (ADR
//! [`0009`](../../../docs/adrs/0009-push-notifications.md), pulled forward into the
//! ADR-0008 plan so the JSON-RPC dispatcher can serve
//! `tasks/pushNotificationConfig/{set,get}` end-to-end).
//!
//! ADR-0009 widens this trait beyond the dispatcher's `set`/`get` slice with three
//! methods used by the delivery worker, the per-tenant cap enforcement in
//! `handle_push_set`, and the janitor that GCs configs after a task's terminal
//! state ages out. Tenant scoping uses an explicit `WHERE tenant_id = $1` filter
//! until ADR-0020 wires `SET LOCAL app.current_tenant_id` per request.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

/// Row in `a2a_push_configs`. `authentication` is kept as raw JSON because the A2A
/// `PushNotificationAuthenticationInfo` shape is open-ended (schemes + opaque
/// credentials). `metadata` is reserved for ADR-0009 tagging (e.g. delivery hints).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct A2aPushConfigRow {
    pub id: Uuid,
    pub task_id: TaskId,
    pub tenant_id: TenantId,
    pub url: Url,
    pub token: Option<String>,
    pub authentication: Option<serde_json::Value>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait A2aPushConfigRepository: Send + Sync {
    /// Upsert a push-notification config for a task. Re-`set` calls under the same
    /// `id` overwrite (`url`, `token`, `authentication`, `metadata`). Inserting a
    /// new row creates a new config — the latest by `created_at` wins for `get`.
    async fn upsert(&self, row: &A2aPushConfigRow) -> Result<(), OrkError>;

    /// Fetch the most-recent push-notification config for `task_id` in `tenant_id`.
    /// Returns `Ok(None)` if no config has been registered.
    async fn get(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
    ) -> Result<Option<A2aPushConfigRow>, OrkError>;

    /// List every push-notification config registered for `task_id` in
    /// `tenant_id`, ordered by `created_at` ascending. The delivery worker
    /// fans the same payload out to each subscriber URL, so multiple rows per
    /// task is a first-class case (ADR-0009 §`tasks/pushNotificationConfig/set`
    /// — "Multiple subscribers per task: Allowed").
    async fn list_for_task(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
    ) -> Result<Vec<A2aPushConfigRow>, OrkError>;

    /// Count the active push configs registered by `tenant_id`. Used by
    /// `handle_push_set` to enforce the per-tenant cap (`config.push.max_per_tenant`,
    /// default 100) before inserting a new row.
    async fn count_active_for_tenant(&self, tenant_id: TenantId) -> Result<u64, OrkError>;

    /// Delete all push configs whose backing `a2a_tasks.completed_at` is older
    /// than `older_than`. Returns the number of rows removed so the janitor can
    /// log the sweep volume. Implementations join on `a2a_tasks` so the cleanup
    /// only fires for terminal tasks.
    async fn delete_terminal_after(&self, older_than: DateTime<Utc>) -> Result<u64, OrkError>;
}
