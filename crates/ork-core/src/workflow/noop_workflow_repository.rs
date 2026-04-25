use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};

use crate::models::workflow::{WorkflowDefinition, WorkflowRun, WorkflowRunStatus};
use crate::ports::repository::WorkflowRepository;

/// Minimal [`WorkflowRepository`] for offline runs (e.g. CLI): persists nothing.
///
/// [`crate::workflow::engine::WorkflowEngine`] only calls [`WorkflowRepository::update_run_status`]
/// and [`WorkflowRepository::append_step_result`]; those are no-ops. Other methods return an error.
#[derive(Clone, Default)]
pub struct NoopWorkflowRepository;

#[async_trait]
impl WorkflowRepository for NoopWorkflowRepository {
    async fn create_definition(
        &self,
        _tenant_id: TenantId,
        _def: &WorkflowDefinition,
    ) -> Result<WorkflowDefinition, OrkError> {
        Err(OrkError::Internal(
            "NoopWorkflowRepository: create_definition not supported".into(),
        ))
    }

    async fn get_definition(
        &self,
        _tenant_id: TenantId,
        _id: WorkflowId,
    ) -> Result<WorkflowDefinition, OrkError> {
        Err(OrkError::NotFound(
            "NoopWorkflowRepository: no definitions stored".into(),
        ))
    }

    async fn list_definitions(
        &self,
        _tenant_id: TenantId,
    ) -> Result<Vec<WorkflowDefinition>, OrkError> {
        Ok(Vec::new())
    }

    async fn delete_definition(
        &self,
        _tenant_id: TenantId,
        _id: WorkflowId,
    ) -> Result<(), OrkError> {
        Ok(())
    }

    async fn create_run(&self, _run: &WorkflowRun) -> Result<WorkflowRun, OrkError> {
        Err(OrkError::Internal(
            "NoopWorkflowRepository: create_run not supported".into(),
        ))
    }

    async fn get_run(
        &self,
        _tenant_id: TenantId,
        _id: WorkflowRunId,
    ) -> Result<WorkflowRun, OrkError> {
        Err(OrkError::NotFound(
            "NoopWorkflowRepository: no runs stored".into(),
        ))
    }

    async fn list_runs(
        &self,
        _tenant_id: TenantId,
        _workflow_id: Option<WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, OrkError> {
        Ok(Vec::new())
    }

    async fn update_run_status(
        &self,
        _tenant_id: TenantId,
        _id: WorkflowRunId,
        _status: WorkflowRunStatus,
        _output: Option<serde_json::Value>,
    ) -> Result<(), OrkError> {
        Ok(())
    }

    async fn append_step_result(
        &self,
        _tenant_id: TenantId,
        _run_id: WorkflowRunId,
        _step_result: &crate::models::workflow::StepResult,
    ) -> Result<(), OrkError> {
        Ok(())
    }
}
