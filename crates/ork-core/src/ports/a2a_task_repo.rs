//! Persistence port for the A2A task ledger introduced in ADR
//! [`0006`](../../../docs/adrs/0006-peer-delegation.md) (parent linkage subset) and
//! widened in ADR [`0008`](../../../docs/adrs/0008-a2a-server-endpoints.md) to cover
//! the full task lifecycle, the per-task message log, and tenant-scoped task listings
//! used by the convenience `GET /a2a/tasks/{task_id}` endpoint.
//!
//! Tenant scoping: every read/mutate that takes a `tenant_id` MUST add an explicit
//! `WHERE tenant_id = $1` filter until ADR-0020 (`SET LOCAL app.current_tenant_id`)
//! is wired. Callers that already established the per-tx GUC may pass any tenant id;
//! the manual filter is a defence-in-depth backstop.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::{ContextId, MessageId, TaskId, TaskState};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowRunId};
use serde::{Deserialize, Serialize};

use crate::a2a::AgentId;

/// Row in the `a2a_tasks` table after the ADR-0008 widening.
///
/// `context_id` and `metadata` were added in `004_a2a_endpoints.sql`; existing rows
/// inherit the SQL defaults (`gen_random_uuid()` and `'{}'::jsonb`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct A2aTaskRow {
    pub id: TaskId,
    pub context_id: ContextId,
    pub tenant_id: TenantId,
    pub agent_id: AgentId,
    pub parent_task_id: Option<TaskId>,
    pub workflow_run_id: Option<WorkflowRunId>,
    pub state: TaskState,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Row in the `a2a_messages` table (ADR-0008 message log).
///
/// `parts` is the raw JSON serialisation of `Vec<ork_a2a::Part>`; deserialise lazily
/// (e.g. only when a caller asks for `tasks/get` history) so the repo trait stays
/// agnostic of the wire types.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct A2aMessageRow {
    pub id: MessageId,
    pub task_id: TaskId,
    pub role: String,
    pub parts: serde_json::Value,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait A2aTaskRepository: Send + Sync {
    /// Insert a new task row. Caller fills every field including the new
    /// ADR-0008 columns (`context_id`, `metadata`, `completed_at`).
    async fn create_task(&self, row: &A2aTaskRow) -> Result<(), OrkError>;

    /// Update the lifecycle state of an existing task row. No-op (returns Ok) if no
    /// row matches; callers treat that as best-effort. Implementations SHOULD set
    /// `completed_at = NOW()` when `state` is terminal so `tasks/get` can surface it.
    async fn update_state(
        &self,
        tenant_id: TenantId,
        id: TaskId,
        state: TaskState,
    ) -> Result<(), OrkError>;

    /// Fetch a task by id (tenant-scoped).
    async fn get_task(
        &self,
        tenant_id: TenantId,
        id: TaskId,
    ) -> Result<Option<A2aTaskRow>, OrkError>;

    /// Append one message (user or agent turn) to an existing task. Implementations
    /// rely on the `BIGSERIAL` `seq` column for ordering; callers do not pass a
    /// sequence number.
    async fn append_message(&self, row: &A2aMessageRow) -> Result<(), OrkError>;

    /// List messages for a task in `seq` ascending order. `history_length`, when
    /// `Some`, caps the number of rows returned; callers default to "no cap" by
    /// passing `None`.
    async fn list_messages(
        &self,
        tenant_id: TenantId,
        task_id: TaskId,
        history_length: Option<u32>,
    ) -> Result<Vec<A2aMessageRow>, OrkError>;

    /// List the most-recent `limit` tasks for a tenant in `created_at` descending
    /// order. Backs the `GET /a2a/tasks/{task_id}` lookup endpoint and
    /// observability/admin views; callers paginate by clamping `limit`.
    async fn list_tasks_in_tenant(
        &self,
        tenant_id: TenantId,
        limit: u32,
    ) -> Result<Vec<A2aTaskRow>, OrkError>;
}
