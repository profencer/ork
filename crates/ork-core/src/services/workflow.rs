use std::sync::Arc;

use chrono::Utc;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use tracing::info;

use crate::models::workflow::{WorkflowDefinition, WorkflowRun, WorkflowRunStatus};
use crate::ports::repository::WorkflowRepository;
use crate::workflow::compiler;
use crate::workflow::engine::WorkflowEngine;

pub struct WorkflowService {
    repo: Arc<dyn WorkflowRepository>,
}

impl WorkflowService {
    pub fn new(repo: Arc<dyn WorkflowRepository>) -> Self {
        Self { repo }
    }

    pub async fn create_definition(
        &self,
        tenant_id: TenantId,
        def: &WorkflowDefinition,
    ) -> Result<WorkflowDefinition, OrkError> {
        if def.steps.is_empty() {
            return Err(OrkError::Validation(
                "workflow must have at least one step".into(),
            ));
        }
        self.repo.create_definition(tenant_id, def).await
    }

    pub async fn get_definition(
        &self,
        tenant_id: TenantId,
        id: WorkflowId,
    ) -> Result<WorkflowDefinition, OrkError> {
        self.repo.get_definition(tenant_id, id).await
    }

    pub async fn list_definitions(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<WorkflowDefinition>, OrkError> {
        self.repo.list_definitions(tenant_id).await
    }

    pub async fn start_run(
        &self,
        tenant_id: TenantId,
        workflow_id: WorkflowId,
        input: serde_json::Value,
    ) -> Result<WorkflowRun, OrkError> {
        let _def = self.repo.get_definition(tenant_id, workflow_id).await?;

        let run = WorkflowRun {
            id: WorkflowRunId::new(),
            workflow_id,
            tenant_id,
            status: WorkflowRunStatus::Pending,
            input,
            output: None,
            step_results: Vec::new(),
            started_at: Utc::now(),
            completed_at: None,
            parent_run_id: None,
            parent_step_id: None,
            parent_task_id: None,
        };

        let run = self.repo.create_run(&run).await?;
        info!(run_id = %run.id, workflow_id = %workflow_id, "workflow run created");
        Ok(run)
    }

    pub async fn get_run(
        &self,
        tenant_id: TenantId,
        run_id: WorkflowRunId,
    ) -> Result<WorkflowRun, OrkError> {
        self.repo.get_run(tenant_id, run_id).await
    }

    pub async fn list_runs(
        &self,
        tenant_id: TenantId,
        workflow_id: Option<WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, OrkError> {
        self.repo.list_runs(tenant_id, workflow_id).await
    }

    /// Load the workflow definition, compile the graph, and execute until completion or failure.
    pub async fn run_workflow(
        &self,
        engine: Arc<WorkflowEngine>,
        tenant_id: TenantId,
        mut run: WorkflowRun,
    ) -> Result<(), OrkError> {
        let def = self.get_definition(tenant_id, run.workflow_id).await?;
        let graph = compiler::compile(&def)?;
        engine.execute(tenant_id, &mut run, &graph).await
    }
}
