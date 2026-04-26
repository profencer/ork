//! Test / dev in-memory [`WebuiStore`]; production uses `PgWebuiStore` in `ork-persistence`.

use async_trait::async_trait;
use ork_a2a::ContextId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::webui_project_repo::WebuiProject;
use ork_core::ports::webui_store::{WebuiConversation, WebuiStore};
use std::collections::HashMap;
use tokio::sync::Mutex;
use uuid::Uuid;

struct Inner {
    projects: HashMap<(Uuid, Uuid), WebuiProject>,
    convs: HashMap<Uuid, WebuiConversation>,
}

/// Dev/test store (ADR-0017) when no Postgres is wired in the webui factory.
pub struct InMemoryWebuiStore {
    inner: Mutex<Inner>,
}

impl InMemoryWebuiStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                projects: HashMap::new(),
                convs: HashMap::new(),
            }),
        }
    }
}

impl Default for InMemoryWebuiStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebuiStore for InMemoryWebuiStore {
    async fn list_projects(&self, tenant: TenantId) -> Result<Vec<WebuiProject>, OrkError> {
        let g = self.inner.lock().await;
        let mut v: Vec<WebuiProject> = g
            .projects
            .values()
            .filter(|p| p.tenant_id == tenant)
            .cloned()
            .collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(v)
    }

    async fn create_project(
        &self,
        tenant: TenantId,
        label: &str,
    ) -> Result<WebuiProject, OrkError> {
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let p = WebuiProject {
            id,
            tenant_id: tenant,
            label: label.to_string(),
            created_at: now,
        };
        self.inner
            .lock()
            .await
            .projects
            .insert((tenant.0, id), p.clone());
        Ok(p)
    }

    async fn delete_project(&self, tenant: TenantId, id: Uuid) -> Result<(), OrkError> {
        let mut g = self.inner.lock().await;
        if g.projects.remove(&(tenant.0, id)).is_none() {
            return Err(OrkError::NotFound(format!("webui project {id}")));
        }
        g.convs.retain(|_, c| c.project_id != Some(id));
        Ok(())
    }

    async fn list_conversations(
        &self,
        tenant: TenantId,
        project_id: Option<Uuid>,
    ) -> Result<Vec<WebuiConversation>, OrkError> {
        let g = self.inner.lock().await;
        let mut v: Vec<WebuiConversation> = g
            .convs
            .values()
            .filter(|c| c.tenant_id == tenant)
            .filter(|c| match project_id {
                Some(p) => c.project_id == Some(p),
                None => true,
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(v)
    }

    async fn create_conversation(
        &self,
        tenant: TenantId,
        project_id: Option<Uuid>,
        context_id: ContextId,
        label: &str,
    ) -> Result<WebuiConversation, OrkError> {
        if let Some(pid) = project_id {
            let g = self.inner.lock().await;
            if !g
                .projects
                .values()
                .any(|p| p.id == pid && p.tenant_id == tenant)
            {
                return Err(OrkError::NotFound("webui project".to_string()));
            }
        }
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let c = WebuiConversation {
            id,
            tenant_id: tenant,
            project_id,
            context_id,
            label: label.to_string(),
            created_at: now,
        };
        self.inner.lock().await.convs.insert(id, c.clone());
        Ok(c)
    }

    async fn get_conversation(
        &self,
        tenant: TenantId,
        id: Uuid,
    ) -> Result<Option<WebuiConversation>, OrkError> {
        let g = self.inner.lock().await;
        Ok(g.convs.get(&id).filter(|c| c.tenant_id == tenant).cloned())
    }
}
