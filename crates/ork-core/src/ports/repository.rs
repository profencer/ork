use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};

use crate::models::tenant::{CreateTenantRequest, Tenant, UpdateTenantSettingsRequest};
use crate::models::workflow::{WorkflowDefinition, WorkflowRun, WorkflowRunStatus};

#[async_trait]
pub trait TenantRepository: Send + Sync {
    async fn create(&self, req: &CreateTenantRequest) -> Result<Tenant, OrkError>;
    async fn get_by_id(&self, id: TenantId) -> Result<Tenant, OrkError>;
    async fn get_by_slug(&self, slug: &str) -> Result<Tenant, OrkError>;
    async fn list(&self) -> Result<Vec<Tenant>, OrkError>;
    async fn update_settings(
        &self,
        id: TenantId,
        req: &UpdateTenantSettingsRequest,
    ) -> Result<Tenant, OrkError>;
    async fn delete(&self, id: TenantId) -> Result<(), OrkError>;
}

#[async_trait]
pub trait WorkflowRepository: Send + Sync {
    async fn create_definition(
        &self,
        tenant_id: TenantId,
        def: &WorkflowDefinition,
    ) -> Result<WorkflowDefinition, OrkError>;

    async fn get_definition(
        &self,
        tenant_id: TenantId,
        id: WorkflowId,
    ) -> Result<WorkflowDefinition, OrkError>;

    async fn list_definitions(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<WorkflowDefinition>, OrkError>;

    async fn delete_definition(&self, tenant_id: TenantId, id: WorkflowId) -> Result<(), OrkError>;

    async fn create_run(&self, run: &WorkflowRun) -> Result<WorkflowRun, OrkError>;

    async fn get_run(
        &self,
        tenant_id: TenantId,
        id: WorkflowRunId,
    ) -> Result<WorkflowRun, OrkError>;

    async fn list_runs(
        &self,
        tenant_id: TenantId,
        workflow_id: Option<WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, OrkError>;

    async fn update_run_status(
        &self,
        tenant_id: TenantId,
        id: WorkflowRunId,
        status: WorkflowRunStatus,
        output: Option<serde_json::Value>,
    ) -> Result<(), OrkError>;

    async fn append_step_result(
        &self,
        tenant_id: TenantId,
        run_id: WorkflowRunId,
        step_result: &crate::models::workflow::StepResult,
    ) -> Result<(), OrkError>;
}
