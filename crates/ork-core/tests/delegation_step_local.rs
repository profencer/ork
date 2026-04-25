//! ADR-0006 §`b) delegate workflow step` — synchronous local delegation path.
//!
//! Asserts that a `delegate_to: { agent: <local>, await: true }` hop runs the
//! child agent, the parent step's primary output stays its own, and the child
//! reply is exposed to a downstream step via `{{<step_id>.delegated.output}}`.

mod common;

use std::sync::Arc;

use chrono::Utc;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::workflow::{
    DelegationSpec, WorkflowDefinition, WorkflowRunStatus, WorkflowStep, WorkflowTrigger,
};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;

use crate::common::{echo_agent_with_prefix, empty_run};

#[tokio::test]
async fn sync_local_delegation_exposes_child_output_under_delegated_key() {
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

    // Two-step workflow: step `first` delegates to `child`, step `second` consumes
    // the child reply via `{{first.delegated.output}}`. This is the only way to
    // observe the per-engine `<step>.delegated` map without poking at internals.
    let now = Utc::now();
    let def = WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: "delegation-local".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![
            WorkflowStep {
                id: "first".into(),
                agent: "parent".into(),
                tools: vec![],
                prompt_template: "ping".into(),
                provider: None,
                model: None,
                depends_on: vec![],
                condition: None,
                for_each: None,
                iteration_var: None,
                delegate_to: Some(spec),
            },
            WorkflowStep {
                id: "second".into(),
                agent: "consumer".into(),
                tools: vec![],
                prompt_template: "saw <{{first.delegated.output}}>".into(),
                provider: None,
                model: None,
                depends_on: vec!["first".into()],
                condition: None,
                for_each: None,
                iteration_var: None,
                delegate_to: None,
            },
        ],
        created_at: now,
        updated_at: now,
    };
    let graph = compiler::compile(&def).expect("compile");

    let registry = AgentRegistry::from_agents(vec![
        echo_agent_with_prefix("parent", "parent-said:"),
        echo_agent_with_prefix("child", "child-said:"),
        echo_agent_with_prefix("consumer", "consumer-saw:"),
    ]);
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry));

    let mut run = empty_run(workflow_id, tenant_id);
    run.id = WorkflowRunId::new();

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("execute");

    assert_eq!(run.status, WorkflowRunStatus::Completed);
    let first = run
        .step_results
        .iter()
        .find(|r| r.step_id == "first")
        .expect("first step recorded");
    assert_eq!(
        first.output.as_deref(),
        Some("parent-said:ping"),
        "parent step's primary output is its own reply, not the child's"
    );
    assert!(first.error.is_none());

    let second = run
        .step_results
        .iter()
        .find(|r| r.step_id == "second")
        .expect("second step recorded");
    let second_out = second.output.as_deref().expect("second step output");
    assert!(
        second_out.contains("child-said:child-prompt"),
        "downstream step must see child reply via {{{{first.delegated.output}}}}; got: {second_out}"
    );
}
