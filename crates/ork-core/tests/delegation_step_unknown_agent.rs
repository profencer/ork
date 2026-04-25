//! ADR-0006 §`Decision` — delegating to an agent the registry doesn't know
//! must surface as a failed step ("rejected"), never a panic.

mod common;

use std::sync::Arc;

use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::workflow::{DelegationSpec, StepStatus, WorkflowRunStatus};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;

use crate::common::{echo_agent_with_prefix, empty_run, one_step_def};

#[tokio::test]
async fn delegate_to_unknown_agent_marks_step_failed_with_rejected_reason() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();

    let spec = DelegationSpec {
        agent: "ghost".into(),
        prompt_template: "irrelevant".into(),
        await_: true,
        push_url: None,
        child_workflow: None,
        timeout: None,
    };
    let def = one_step_def(workflow_id, tenant_id, "parent", Some(spec));
    let graph = compiler::compile(&def).expect("compile");

    let registry = AgentRegistry::from_agents(vec![echo_agent_with_prefix("parent", "p:")]);
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry));

    let mut run = empty_run(workflow_id, tenant_id);
    run.id = WorkflowRunId::new();

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("engine execute returns Ok even when a step fails");

    assert_eq!(run.status, WorkflowRunStatus::Failed);
    let step = run
        .step_results
        .iter()
        .find(|r| r.step_id == "only")
        .expect("step recorded");
    assert_eq!(step.status, StepStatus::Failed);
    let err = step
        .error
        .as_deref()
        .expect("failed step must record an error message");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("unknown") || lower.contains("rejected"),
        "expected 'unknown' or 'rejected' in error message; got: {err}"
    );
}
