//! ADR-0006 §`b) child_workflow` — `delegate_to: { child_workflow: <id> }` forks
//! a child [`WorkflowRun`] and links it back to the parent.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::workflow::{
    DelegationSpec, StepResult, WorkflowDefinition, WorkflowRun, WorkflowRunStatus,
};
use ork_core::ports::repository::WorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::common::{echo_agent_with_prefix, empty_run, one_step_def};

/// Minimal in-memory repo: stores definitions for `get_definition`, captures
/// every `create_run` call so the test can assert parent-linkage.
#[derive(Default)]
struct MemRepo {
    definitions: Mutex<HashMap<WorkflowId, WorkflowDefinition>>,
    runs: Mutex<HashMap<WorkflowRunId, WorkflowRun>>,
    captured_create_runs: Mutex<Vec<WorkflowRun>>,
}

impl MemRepo {
    fn put_def(&self, def: WorkflowDefinition) {
        let id = def.id;
        let mut g = self.definitions.try_lock().expect("uncontended in test");
        g.insert(id, def);
    }
}

#[async_trait]
impl WorkflowRepository for MemRepo {
    async fn create_definition(
        &self,
        _tenant_id: TenantId,
        def: &WorkflowDefinition,
    ) -> Result<WorkflowDefinition, OrkError> {
        self.definitions.lock().await.insert(def.id, def.clone());
        Ok(def.clone())
    }

    async fn get_definition(
        &self,
        _tenant_id: TenantId,
        id: WorkflowId,
    ) -> Result<WorkflowDefinition, OrkError> {
        self.definitions
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| OrkError::NotFound(format!("no def {id}")))
    }

    async fn list_definitions(
        &self,
        _tenant_id: TenantId,
    ) -> Result<Vec<WorkflowDefinition>, OrkError> {
        Ok(self.definitions.lock().await.values().cloned().collect())
    }

    async fn delete_definition(
        &self,
        _tenant_id: TenantId,
        id: WorkflowId,
    ) -> Result<(), OrkError> {
        self.definitions.lock().await.remove(&id);
        Ok(())
    }

    async fn create_run(&self, run: &WorkflowRun) -> Result<WorkflowRun, OrkError> {
        self.captured_create_runs.lock().await.push(run.clone());
        self.runs.lock().await.insert(run.id, run.clone());
        Ok(run.clone())
    }

    async fn get_run(
        &self,
        _tenant_id: TenantId,
        id: WorkflowRunId,
    ) -> Result<WorkflowRun, OrkError> {
        self.runs
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| OrkError::NotFound(format!("no run {id}")))
    }

    async fn list_runs(
        &self,
        _tenant_id: TenantId,
        _workflow_id: Option<WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, OrkError> {
        Ok(self.runs.lock().await.values().cloned().collect())
    }

    async fn update_run_status(
        &self,
        _tenant_id: TenantId,
        id: WorkflowRunId,
        status: WorkflowRunStatus,
        output: Option<serde_json::Value>,
    ) -> Result<(), OrkError> {
        let mut g = self.runs.lock().await;
        if let Some(r) = g.get_mut(&id) {
            r.status = status;
            if output.is_some() {
                r.output = output;
            }
        }
        Ok(())
    }

    async fn append_step_result(
        &self,
        _tenant_id: TenantId,
        run_id: WorkflowRunId,
        step_result: &StepResult,
    ) -> Result<(), OrkError> {
        let mut g = self.runs.lock().await;
        if let Some(r) = g.get_mut(&run_id) {
            r.step_results.push(step_result.clone());
        }
        Ok(())
    }
}

#[tokio::test]
async fn child_workflow_run_is_forked_with_parent_linkage() {
    let tenant_id = TenantId::new();

    // Define the child workflow first so we can reference it.
    let child_workflow_id = WorkflowId::new();
    let child_def = one_step_def(child_workflow_id, tenant_id, "child", None);

    let parent_workflow_id = WorkflowId::new();
    let spec = DelegationSpec {
        agent: format!("workflow:{child_workflow_id}"),
        prompt_template: String::new(),
        await_: true,
        push_url: None,
        child_workflow: Some(child_workflow_id),
        timeout: None,
    };
    let parent_def = one_step_def(parent_workflow_id, tenant_id, "parent", Some(spec));

    let repo = Arc::new(MemRepo::default());
    repo.put_def(child_def);
    repo.put_def(parent_def.clone());

    let registry = AgentRegistry::from_agents(vec![
        echo_agent_with_prefix("parent", "p:"),
        echo_agent_with_prefix("child", "c:"),
    ]);
    let engine = WorkflowEngine::new(repo.clone(), Arc::new(registry)).with_delegation(
        None,
        None,
        CancellationToken::new(),
    );

    let graph = compiler::compile(&parent_def).expect("compile parent");
    let mut run = empty_run(parent_workflow_id, tenant_id);
    let parent_run_id = run.id;

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("execute parent run");

    assert_eq!(run.status, WorkflowRunStatus::Completed);

    let captured = repo.captured_create_runs.lock().await;
    let child_run = captured
        .iter()
        .find(|r| r.parent_run_id == Some(parent_run_id))
        .expect("child run was created and linked back to the parent");
    assert_eq!(child_run.workflow_id, child_workflow_id);
    assert_eq!(child_run.parent_step_id.as_deref(), Some("only"));
    assert!(
        child_run.parent_task_id.is_some(),
        "child run must carry the synthetic parent_task_id (used as the workflow's a2a task)"
    );
}
