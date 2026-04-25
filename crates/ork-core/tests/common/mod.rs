//! Shared fixtures for the ADR-0006 delegation integration tests.
//!
//! Lives under `tests/common/` so each test binary can `mod common;` it without
//! exposing them as part of the public crate surface.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentSkill, Message as AgentMessage, MessageId, Part, Role,
    TaskEvent as AgentEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_core::a2a::{AgentContext, AgentId};
use ork_core::ports::agent::{Agent, AgentEventStream};

/// Minimal local agent that echoes the user prompt back as its message text. Used
/// across the delegation tests as both the parent and child target.
pub struct EchoAgent {
    id: AgentId,
    card: AgentCard,
    /// Prefix prepended to the echoed text. Lets a single test distinguish
    /// "parent ran" from "child ran" from the run output.
    prefix: String,
}

impl EchoAgent {
    pub fn new(id: &str) -> Self {
        Self::with_prefix(id, "echo:")
    }

    pub fn with_prefix(id: &str, prefix: &str) -> Self {
        Self {
            id: id.into(),
            card: card(id),
            prefix: prefix.into(),
        }
    }
}

fn card(id: &str) -> AgentCard {
    AgentCard {
        name: id.to_string(),
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

fn user_prompt_text(msg: &AgentMessage) -> Result<String, OrkError> {
    let mut s = String::new();
    for p in &msg.parts {
        if let Part::Text { text, .. } = p {
            s.push_str(text);
        }
    }
    if s.is_empty() {
        return Err(OrkError::Validation("no text in message".into()));
    }
    Ok(s)
}

#[async_trait]
impl Agent for EchoAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn card(&self) -> &AgentCard {
        &self.card
    }

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let text = user_prompt_text(&msg)?;
        let task_id = ctx.task_id;
        let context_id = ctx.context_id;
        let reply = format!("{}{text}", self.prefix);
        let out_msg = AgentMessage {
            role: Role::Agent,
            parts: vec![Part::Text {
                text: reply,
                metadata: None,
            }],
            message_id: MessageId::new(),
            task_id: Some(task_id),
            context_id,
            metadata: None,
        };
        let events: Vec<Result<AgentEvent, OrkError>> = vec![
            Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                },
                is_final: false,
            })),
            Ok(AgentEvent::Message(out_msg)),
            Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                },
                is_final: true,
            })),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Build a one-step `WorkflowDefinition` used by most delegation tests. The
/// caller fills in `delegate_to` and the agent id.
pub fn one_step_def(
    workflow_id: ork_common::types::WorkflowId,
    tenant_id: ork_common::types::TenantId,
    agent: &str,
    delegate_to: Option<ork_core::models::workflow::DelegationSpec>,
) -> ork_core::models::workflow::WorkflowDefinition {
    use chrono::Utc;
    use ork_core::models::workflow::{WorkflowDefinition, WorkflowStep, WorkflowTrigger};
    let now = Utc::now();
    WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: "delegation-test".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![WorkflowStep {
            id: "only".into(),
            agent: agent.into(),
            tools: vec![],
            prompt_template: "ping".into(),
            depends_on: vec![],
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to,
        }],
        created_at: now,
        updated_at: now,
    }
}

/// Fresh `WorkflowRun` shell; tests fill in `input` if they need a custom one.
pub fn empty_run(
    workflow_id: ork_common::types::WorkflowId,
    tenant_id: ork_common::types::TenantId,
) -> ork_core::models::workflow::WorkflowRun {
    use chrono::Utc;
    use ork_core::models::workflow::{WorkflowRun, WorkflowRunStatus};
    WorkflowRun {
        id: ork_common::types::WorkflowRunId::new(),
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

/// Convenience: wrap an [`EchoAgent`] in `Arc<dyn Agent>` with the right cast.
pub fn echo_agent(id: &str) -> Arc<dyn Agent> {
    Arc::new(EchoAgent::new(id)) as Arc<dyn Agent>
}

pub fn echo_agent_with_prefix(id: &str, prefix: &str) -> Arc<dyn Agent> {
    Arc::new(EchoAgent::with_prefix(id, prefix)) as Arc<dyn Agent>
}
