//! Web UI "project" row (ADR-0017). CRUD lives on [`super::webui_store::WebuiStore`].

use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One UI project: label for a set of conversations under a tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebuiProject {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub label: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
