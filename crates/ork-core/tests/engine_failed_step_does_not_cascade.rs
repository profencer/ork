//! Regression for `docs/incidents/2026-04-25-workflow-cascades-past-failed-step.md`.
//!
//! When a step fails and its only outgoing edge is a `depends_on`-derived
//! one (no explicit `condition.on_fail`), the engine must terminate the
//! run with `Failed` rather than walking the edge into a downstream step
//! that has no chance of producing meaningful output.
//!
//! The "cascade" path was: `depends_on` was compiled to
//! `EdgeCondition::Always`, so a failed parent advanced into its
//! children, which then ran with unsubstituted prompt templates against
//! the LLM and stretched the polling timeout for no reason.
//!
//! These tests are the contract for the compiler-side fix in
//! `crates/ork-core/src/workflow/compiler.rs`: `depends_on` becomes
//! `EdgeCondition::OnPass`, while explicit `condition.on_fail` edges
//! continue to be honoured untouched.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentSkill, Message as AgentMessage, MessageId, Part, Role,
    TaskEvent as AgentEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::a2a::{AgentContext, AgentId};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::workflow::{
    StepCondition, StepStatus, WorkflowDefinition, WorkflowRun, WorkflowRunStatus, WorkflowStep,
    WorkflowTrigger,
};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;

fn card(id: &str) -> AgentCard {
    AgentCard {
        name: id.into(),
        description: "test stub".into(),
        version: "0.0.0".into(),
        url: None,
        provider: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "stub".into(),
            name: "stub".into(),
            description: "stub".into(),
            tags: vec![],
            examples: vec![],
            input_modes: None,
            output_modes: None,
        }],
        security_schemes: None,
        security: None,
        extensions: None,
    }
}

struct OkAgent {
    id: AgentId,
    card: AgentCard,
    output: String,
}

impl OkAgent {
    fn new(id: &str, output: &str) -> Self {
        Self {
            id: id.into(),
            card: card(id),
            output: output.into(),
        }
    }
}

#[async_trait]
impl Agent for OkAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    async fn send_stream(
        &self,
        ctx: AgentContext,
        _msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let task_id = ctx.task_id;
        let out = AgentMessage {
            role: Role::Agent,
            parts: vec![Part::Text {
                text: self.output.clone(),
                metadata: None,
            }],
            message_id: MessageId::new(),
            task_id: Some(task_id),
            context_id: ctx.context_id,
            metadata: None,
        };
        Ok(Box::pin(stream::iter(vec![
            Ok(AgentEvent::Message(out)),
            Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                },
                is_final: true,
            })),
        ])))
    }
}

/// Always errors out of `send_stream` — simulates a hard LLM/transport
/// failure on the parent step in the regression scenario.
struct FailingAgent {
    id: AgentId,
    card: AgentCard,
}

impl FailingAgent {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            card: card(id),
        }
    }
}

#[async_trait]
impl Agent for FailingAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    async fn send_stream(
        &self,
        _ctx: AgentContext,
        _msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        Err(OrkError::LlmProvider(
            "stream read failed: error decoding response body".into(),
        ))
    }
}

fn run_skeleton(workflow_id: WorkflowId, tenant_id: TenantId) -> WorkflowRun {
    WorkflowRun {
        id: WorkflowRunId::new(),
        workflow_id,
        tenant_id,
        status: WorkflowRunStatus::Pending,
        input: serde_json::json!({}),
        output: None,
        step_results: vec![],
        started_at: Utc::now(),
        completed_at: None,
        parent_run_id: None,
        parent_step_id: None,
        parent_task_id: None,
    }
}

fn step(
    id: &str,
    agent: &str,
    depends_on: Vec<&str>,
    condition: Option<StepCondition>,
) -> WorkflowStep {
    WorkflowStep {
        id: id.into(),
        agent: agent.into(),
        tools: vec![],
        prompt_template: format!("step {id}"),
        provider: None,
        model: None,
        depends_on: depends_on.into_iter().map(String::from).collect(),
        condition,
        for_each: None,
        iteration_var: None,
        delegate_to: None,
    }
}

#[tokio::test]
async fn linear_chain_terminates_when_parent_step_fails() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();
    let now = Utc::now();
    let def = WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: "fails-then-stops".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![
            step("a", "failing", vec![], None),
            step("b", "ok", vec!["a"], None),
        ],
        created_at: now,
        updated_at: now,
    };
    let graph = compiler::compile(&def).expect("compile");
    let registry = AgentRegistry::from_agents(vec![
        Arc::new(FailingAgent::new("failing")) as Arc<dyn Agent>,
        Arc::new(OkAgent::new("ok", "should-not-run")) as Arc<dyn Agent>,
    ]);
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry));
    let mut run = run_skeleton(workflow_id, tenant_id);

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("engine returns Ok even when a step fails (the run carries the Failed status)");

    assert_eq!(
        run.status,
        WorkflowRunStatus::Failed,
        "run must finish Failed when its only step fails and there is no on_fail edge"
    );
    assert_eq!(
        run.step_results.len(),
        1,
        "engine must NOT advance to step `b` after step `a` failed via a depends_on-only edge; \
         got step_results: {:?}",
        run.step_results
            .iter()
            .map(|r| (r.step_id.clone(), r.status))
            .collect::<Vec<_>>()
    );
    let only = &run.step_results[0];
    assert_eq!(only.step_id, "a");
    assert_eq!(only.status, StepStatus::Failed);
    assert!(
        only.error
            .as_ref()
            .is_some_and(|e| e.contains("stream read failed")),
        "step `a`'s error should preserve the upstream LLM message; got {:?}",
        only.error
    );
}

#[tokio::test]
async fn explicit_on_fail_edge_still_routes_to_fallback_step() {
    // A second-axis test: the OnPass-by-default change for `depends_on`
    // must NOT regress workflows that opt into a fallback path via
    // `condition.on_fail`. Here `a` fails and routes to `c` while `b`
    // (which would have been the OnPass target) is skipped.
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();
    let now = Utc::now();
    let def = WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: "on-fail-fallback".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![
            step(
                "a",
                "failing",
                vec![],
                Some(StepCondition {
                    on_pass: "b".into(),
                    on_fail: "c".into(),
                }),
            ),
            step("b", "ok", vec!["a"], None),
            step("c", "ok", vec![], None),
        ],
        created_at: now,
        updated_at: now,
    };
    let graph = compiler::compile(&def).expect("compile");
    let registry = AgentRegistry::from_agents(vec![
        Arc::new(FailingAgent::new("failing")) as Arc<dyn Agent>,
        Arc::new(OkAgent::new("ok", "fallback-output")) as Arc<dyn Agent>,
    ]);
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry));
    let mut run = run_skeleton(workflow_id, tenant_id);

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("execute");

    let visited: Vec<_> = run
        .step_results
        .iter()
        .map(|r| (r.step_id.clone(), r.status))
        .collect();
    assert!(
        visited
            .iter()
            .any(|(id, st)| id == "a" && *st == StepStatus::Failed),
        "expected step `a` to be failed in step_results, got {visited:?}"
    );
    assert!(
        visited
            .iter()
            .any(|(id, st)| id == "c" && *st == StepStatus::Completed),
        "engine must take the explicit on_fail edge to `c` when `a` fails, got {visited:?}"
    );
    assert!(
        !visited.iter().any(|(id, _)| id == "b"),
        "engine must NOT visit `b` (the on_pass target) when `a` failed, got {visited:?}"
    );
}
