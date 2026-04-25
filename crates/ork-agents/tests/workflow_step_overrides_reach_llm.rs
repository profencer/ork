//! Engine-level precedence test for ADR 0012 §`Selection`.
//!
//! Drives a real [`WorkflowDefinition`] through the [`WorkflowEngine`]
//! and a [`LocalAgent`] backed by a recording [`LlmProvider`] mock to
//! verify that a `WorkflowStep.provider` (and `WorkflowStep.model`)
//! actually reaches `ChatRequest.{provider, model}` — i.e. the
//! propagation chain
//!
//!   `WorkflowStep` → `WorkflowNode` → `execute_agent_step` →
//!   `AgentContext.step_llm_overrides` → `LocalAgent::send_stream` →
//!   `ChatRequest.{provider, model}`
//!
//! is end-to-end-correct.
//!
//! The remaining router-internal `request → tenant default → operator
//! default` precedence is verified by
//! [`crates/ork-llm/tests/router_smoke.rs`](../../ork-llm/tests/router_smoke.rs)
//! ::request_field_beats_tenant_default_beats_operator_default. Together
//! the two tests cover the full `step → agent → tenant → operator`
//! chain the ADR mandates.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ork_agents::local::LocalAgent;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::agent::AgentConfig;
use ork_core::models::workflow::{
    WorkflowDefinition, WorkflowRun, WorkflowRunStatus, WorkflowStep, WorkflowTrigger,
};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    TokenUsage,
};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::{ToolExecutor, WorkflowEngine};
use tokio::sync::Mutex;

/// Recording [`LlmProvider`] that captures every [`ChatRequest`] it
/// sees and replies with a tiny scripted stream so the agent loop
/// reaches its terminal state in one iteration.
struct RecordingLlm {
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl LlmProvider for RecordingLlm {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!("agent loop uses chat_stream only")
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        self.requests.lock().await.push(request);
        let events = vec![
            ChatStreamEvent::Delta("ok".into()),
            ChatStreamEvent::Done {
                usage: TokenUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                },
                model: "stub".into(),
                finish_reason: FinishReason::Stop,
            },
        ];
        Ok(Box::pin(async_stream::stream! {
            for ev in events {
                yield Ok(ev);
            }
        }))
    }

    fn provider_name(&self) -> &str {
        "recording"
    }
}

struct NoopTools;

#[async_trait]
impl ToolExecutor for NoopTools {
    async fn execute(
        &self,
        _ctx: &ork_core::a2a::AgentContext,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        unreachable!("the test workflow declares no tools")
    }
}

fn agent_config_with_provider_and_model(provider: &str, model: &str) -> AgentConfig {
    AgentConfig {
        id: "writer".into(),
        name: "Writer".into(),
        description: "test".into(),
        system_prompt: "sys".into(),
        tools: vec![],
        provider: Some(provider.into()),
        model: Some(model.into()),
        temperature: 0.0,
        max_tokens: 16,
        max_tool_iterations: 1,
        max_parallel_tool_calls: 1,
        max_tool_result_bytes: 1024,
        expose_reasoning: false,
    }
}

fn workflow_with_step_provider(step_provider: &str, step_model: &str) -> WorkflowDefinition {
    let now = Utc::now();
    WorkflowDefinition {
        id: WorkflowId::new(),
        tenant_id: TenantId::new(),
        name: "step-overrides".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![WorkflowStep {
            id: "only".into(),
            agent: "writer".into(),
            tools: vec![],
            prompt_template: "ping".into(),
            provider: Some(step_provider.into()),
            model: Some(step_model.into()),
            depends_on: vec![],
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to: None,
        }],
        created_at: now,
        updated_at: now,
    }
}

/// `WorkflowStep.provider` and `WorkflowStep.model` reach
/// `ChatRequest.{provider, model}` and shadow the lower-precedence
/// `AgentConfig` defaults — proving Critical #2 from ADR 0012's review
/// is fixed.
#[tokio::test]
async fn workflow_step_overrides_shadow_agent_config() {
    let llm = Arc::new(RecordingLlm {
        requests: Mutex::new(Vec::new()),
    });

    // Agent config carries its own (lower-precedence) provider + model
    // so a regression that drops the step-level override would still
    // produce a non-`None` request, but with the wrong values — making
    // the test specifically diagnostic.
    let agent = LocalAgent::new(
        agent_config_with_provider_and_model("agent-only", "agent-model"),
        &CardEnrichmentContext::minimal(),
        llm.clone() as Arc<dyn LlmProvider>,
        Arc::new(NoopTools),
    );
    let registry = AgentRegistry::from_agents(vec![Arc::new(agent) as Arc<dyn Agent>]);
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry));

    let def = workflow_with_step_provider("step-only", "step-model");
    let workflow_id = def.id;
    let tenant_id = def.tenant_id;
    let graph = compiler::compile(&def).expect("compile");

    let mut run = WorkflowRun {
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
    };

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("workflow runs to completion");

    assert_eq!(run.status, WorkflowRunStatus::Completed);

    let requests = llm.requests.lock().await;
    assert_eq!(requests.len(), 1, "exactly one LLM call");
    let req = &requests[0];
    assert_eq!(
        req.provider.as_deref(),
        Some("step-only"),
        "WorkflowStep.provider must override AgentConfig.provider on the request the LLM receives \
         (ADR 0012 §`Selection`)"
    );
    assert_eq!(
        req.model.as_deref(),
        Some("step-model"),
        "WorkflowStep.model must override AgentConfig.model on the request the LLM receives \
         (ADR 0012 §`Selection`)"
    );
}

/// Sanity check: when `WorkflowStep.{provider, model}` are `None`, the
/// `AgentConfig` defaults still flow through — i.e. removing the
/// override does not regress the agent-config precedence layer.
#[tokio::test]
async fn agent_config_provider_used_when_step_has_none() {
    let llm = Arc::new(RecordingLlm {
        requests: Mutex::new(Vec::new()),
    });
    let agent = LocalAgent::new(
        agent_config_with_provider_and_model("agent-only", "agent-model"),
        &CardEnrichmentContext::minimal(),
        llm.clone() as Arc<dyn LlmProvider>,
        Arc::new(NoopTools),
    );
    let registry = AgentRegistry::from_agents(vec![Arc::new(agent) as Arc<dyn Agent>]);
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry));

    let now = Utc::now();
    let def = WorkflowDefinition {
        id: WorkflowId::new(),
        tenant_id: TenantId::new(),
        name: "no-step-overrides".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![WorkflowStep {
            id: "only".into(),
            agent: "writer".into(),
            tools: vec![],
            prompt_template: "ping".into(),
            provider: None,
            model: None,
            depends_on: vec![],
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to: None,
        }],
        created_at: now,
        updated_at: now,
    };
    let workflow_id = def.id;
    let tenant_id = def.tenant_id;
    let graph = compiler::compile(&def).expect("compile");

    let mut run = WorkflowRun {
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
    };

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("workflow runs to completion");

    let requests = llm.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].provider.as_deref(), Some("agent-only"));
    assert_eq!(requests[0].model.as_deref(), Some("agent-model"));
}
