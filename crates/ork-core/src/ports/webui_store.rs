//! In-memory and Postgres-backed Web UI project + conversation state (ADR-0017).

use async_trait::async_trait;
use ork_a2a::ContextId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::webui_project_repo::WebuiProject;

/// One UI conversation tied to A2A `context_id` and optionally a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebuiConversation {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub project_id: Option<Uuid>,
    pub context_id: ContextId,
    pub label: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Projects + per-tenant conversation metadata for the Web UI.
#[async_trait]
pub trait WebuiStore: Send + Sync {
    /// Project CRUD
    async fn list_projects(&self, tenant: TenantId) -> Result<Vec<WebuiProject>, OrkError>;
    async fn create_project(&self, tenant: TenantId, label: &str)
    -> Result<WebuiProject, OrkError>;
    async fn delete_project(&self, tenant: TenantId, id: Uuid) -> Result<(), OrkError>;

    /// Conversations (each has an A2A `context_id`).
    async fn list_conversations(
        &self,
        tenant: TenantId,
        project_id: Option<Uuid>,
    ) -> Result<Vec<WebuiConversation>, OrkError>;
    async fn create_conversation(
        &self,
        tenant: TenantId,
        project_id: Option<Uuid>,
        context_id: ContextId,
        label: &str,
    ) -> Result<WebuiConversation, OrkError>;
    async fn get_conversation(
        &self,
        tenant: TenantId,
        id: Uuid,
    ) -> Result<Option<WebuiConversation>, OrkError>;
}
