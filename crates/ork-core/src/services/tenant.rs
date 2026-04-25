use std::sync::Arc;

use ork_common::error::OrkError;
use ork_common::types::TenantId;

use crate::models::tenant::{CreateTenantRequest, Tenant, UpdateTenantSettingsRequest};
use crate::ports::repository::TenantRepository;

pub struct TenantService {
    repo: Arc<dyn TenantRepository>,
}

impl TenantService {
    pub fn new(repo: Arc<dyn TenantRepository>) -> Self {
        Self { repo }
    }

    pub async fn create_tenant(&self, req: &CreateTenantRequest) -> Result<Tenant, OrkError> {
        if req.name.trim().is_empty() {
            return Err(OrkError::Validation("tenant name cannot be empty".into()));
        }
        if req.slug.trim().is_empty() {
            return Err(OrkError::Validation("tenant slug cannot be empty".into()));
        }
        self.repo.create(req).await
    }

    pub async fn get_tenant(&self, id: TenantId) -> Result<Tenant, OrkError> {
        self.repo.get_by_id(id).await
    }

    pub async fn list_tenants(&self) -> Result<Vec<Tenant>, OrkError> {
        self.repo.list().await
    }

    pub async fn update_settings(
        &self,
        id: TenantId,
        req: &UpdateTenantSettingsRequest,
    ) -> Result<Tenant, OrkError> {
        self.repo.update_settings(id, req).await
    }

    pub async fn delete_tenant(&self, id: TenantId) -> Result<(), OrkError> {
        self.repo.delete(id).await
    }
}
