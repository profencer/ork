//! Regression: `WorkflowEngine` must insert a parent row into `a2a_tasks`
//! *before* invoking the step's agent — otherwise any child task the agent
//! creates (`agent_call` tool, `delegate_to:` hop, `peer_<id>_<skill>`)
//! violates the `a2a_tasks_parent_task_id_fkey` FK constraint and crashes
//! the step.
//!
//! This was a documented engine gap (`demo/README.md` "Known engine gaps")
//! that the demo's `change-plan.json` snapshot worked around by dropping
//! the `delegate_to:` block on the `review` step. After the ADR-0010
//! recovery fix landed, the underlying agent loop now retries instead of
//! aborting on tool errors, so the LLM is more likely to surface peer
//! delegations from inside vanilla steps too — re-exposing the exact FK
//! error in `synthesize` / `write_plan` / `review`:
//!
//! ```text
//! database error: create a2a_task: error returned from database:
//!   insert or update on table "a2a_tasks" violates foreign key constraint
//!   "a2a_tasks_parent_task_id_fkey"
//! ```

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use ork_a2a::{TaskId, TaskState};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::workflow::{DelegationSpec, WorkflowRunStatus};
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::common::{echo_agent, empty_run, one_step_def};

/// In-memory `A2aTaskRepository` that **enforces** the
/// `a2a_tasks_parent_task_id_fkey` FK that Postgres holds in production: a
/// child insert fails if the referenced parent row was never inserted.
/// Mirrors the wording of the production error so a failing test is
/// recognisable as the demo's failure.
#[derive(Default)]
struct FkEnforcingTaskRepo {
    rows: Mutex<Vec<A2aTaskRow>>,
}

#[async_trait]
impl A2aTaskRepository for FkEnforcingTaskRepo {
    async fn create_task(&self, row: &A2aTaskRow) -> Result<(), OrkError> {
        let mut rows = self.rows.lock().await;
        if let Some(parent) = row.parent_task_id
            && !rows.iter().any(|r| r.id == parent)
        {
            return Err(OrkError::Database(format!(
                "create a2a_task: error returned from database: insert or update on \
                 table \"a2a_tasks\" violates foreign key constraint \
                 \"a2a_tasks_parent_task_id_fkey\" (no parent row with id={parent})"
            )));
        }
        rows.push(row.clone());
        Ok(())
    }

    async fn update_state(
        &self,
        _: TenantId,
        id: TaskId,
        state: TaskState,
    ) -> Result<(), OrkError> {
        let mut rows = self.rows.lock().await;
        if let Some(r) = rows.iter_mut().find(|r| r.id == id) {
            r.state = state;
        }
        Ok(())
    }

    async fn get_task(&self, _: TenantId, id: TaskId) -> Result<Option<A2aTaskRow>, OrkError> {
        Ok(self.rows.lock().await.iter().find(|r| r.id == id).cloned())
    }

    async fn append_message(&self, _: &A2aMessageRow) -> Result<(), OrkError> {
        Ok(())
    }

    async fn list_messages(
        &self,
        _: TenantId,
        _: TaskId,
        _: Option<u32>,
    ) -> Result<Vec<A2aMessageRow>, OrkError> {
        Ok(vec![])
    }

    async fn list_tasks_in_tenant(
        &self,
        tenant_id: TenantId,
        _limit: u32,
    ) -> Result<Vec<A2aTaskRow>, OrkError> {
        Ok(self
            .rows
            .lock()
            .await
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
}

/// `delegate_to:` step: the existing engine never inserts a parent row for
/// the delegation hop, so the delegation helper's child insert fails the
/// FK. With the engine fix, the parent row exists by the time the helper
/// runs, the child insert succeeds, and the run completes.
#[tokio::test]
async fn engine_persists_parent_a2a_task_for_delegate_to_hop_so_child_fk_holds() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();

    let spec = DelegationSpec {
        agent: "child".into(),
        prompt_template: "child-prompt".into(),
        await_: true,
        push_url: None,
        child_workflow: None,
        timeout: None,
    };

    let def = one_step_def(workflow_id, tenant_id, "parent", Some(spec));
    let graph = compiler::compile(&def).expect("compile");

    let registry = AgentRegistry::from_agents(vec![echo_agent("parent"), echo_agent("child")]);

    let task_repo: Arc<dyn A2aTaskRepository> = Arc::new(FkEnforcingTaskRepo::default());
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry))
        .with_delegation(None, Some(task_repo.clone()), CancellationToken::new());

    let mut run = empty_run(workflow_id, tenant_id);
    run.id = WorkflowRunId::new();

    let result = engine.execute(tenant_id, &mut run, &graph).await;
    assert!(
        result.is_ok(),
        "engine must insert the parent a2a_tasks row before delegating, \
         else the FK on the child row fails (see demo/README.md \
         'Known engine gaps'); got: {result:?}"
    );
    assert_eq!(
        run.status,
        WorkflowRunStatus::Completed,
        "run must complete; got: {:?}",
        run.status
    );

    // We expect at least two rows: the engine-minted parent for the
    // delegation hop, and the child row that the helper inserted.
    let rows = task_repo
        .list_tasks_in_tenant(tenant_id, 100)
        .await
        .unwrap();
    assert!(
        rows.len() >= 2,
        "expected >= 2 a2a_tasks rows (parent + child); got {}: {rows:#?}",
        rows.len()
    );
    let parents = rows.iter().filter(|r| r.parent_task_id.is_none()).count();
    let children = rows.iter().filter(|r| r.parent_task_id.is_some()).count();
    assert!(
        parents >= 1 && children >= 1,
        "expected at least one parent (parent_task_id=None) and one child row; \
         got parents={parents} children={children}"
    );
}

/// Vanilla step (no `delegate_to:`): the engine must still persist a
/// parent row so any `agent_call` / peer-delegation the agent makes from
/// inside its tool loop can satisfy the FK. We assert the row exists,
/// which is the contract the FK-enforcing repo above relies on.
#[tokio::test]
async fn engine_persists_parent_a2a_task_for_vanilla_agent_step() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();

    let def = one_step_def(workflow_id, tenant_id, "parent", None);
    let graph = compiler::compile(&def).expect("compile");

    let registry = AgentRegistry::from_agents(vec![echo_agent("parent")]);

    let task_repo: Arc<dyn A2aTaskRepository> = Arc::new(FkEnforcingTaskRepo::default());
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry))
        .with_delegation(None, Some(task_repo.clone()), CancellationToken::new());

    let mut run = empty_run(workflow_id, tenant_id);
    run.id = WorkflowRunId::new();

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("execute");

    let rows = task_repo
        .list_tasks_in_tenant(tenant_id, 100)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "engine must persist exactly one a2a_tasks row per vanilla agent \
         step (parent_task_id=None); got: {rows:#?}"
    );
    let row = &rows[0];
    assert_eq!(
        row.parent_task_id, None,
        "the engine-minted row is a top-level parent; parent_task_id=None"
    );
    assert_eq!(
        row.agent_id, "parent",
        "agent_id must match the step's agent"
    );
    assert_eq!(
        row.workflow_run_id,
        Some(run.id),
        "workflow_run_id must be threaded through so a2a_tasks rows are \
         queryable by run"
    );
}
