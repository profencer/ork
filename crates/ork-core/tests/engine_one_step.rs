//! Smoke test: one-step workflow completes via a stub [`Agent`].

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
    WorkflowDefinition, WorkflowRun, WorkflowRunStatus, WorkflowStep, WorkflowTrigger,
};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;
use serde::Deserialize;

struct EchoAgent {
    id: AgentId,
    card: AgentCard,
}

struct FixedAgent {
    id: AgentId,
    card: AgentCard,
    output: String,
}

impl FixedAgent {
    fn new(id: &str, output: &str) -> Self {
        let echo = EchoAgent::new(id);
        Self {
            id: id.into(),
            card: echo.card,
            output: output.into(),
        }
    }
}

impl EchoAgent {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            card: AgentCard {
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
            },
        }
    }
}

#[async_trait]
impl Agent for FixedAgent {
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
        let out_msg = AgentMessage {
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
            Ok(AgentEvent::Message(out_msg)),
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

#[derive(Deserialize)]
struct WorkflowYaml {
    name: String,
    version: String,
    trigger: WorkflowTrigger,
    steps: Vec<WorkflowStep>,
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

#[tokio::test]
async fn change_plan_template_smoke_completes_with_stub_agents() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();
    let now = Utc::now();
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../workflow-templates/change-plan.yaml"
    ))
    .expect("template");
    let wf: WorkflowYaml = serde_yaml::from_str(&yaml).expect("parse template");
    let def = WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: wf.name,
        version: wf.version,
        trigger: wf.trigger,
        steps: wf.steps,
        created_at: now,
        updated_at: now,
    };
    let graph = compiler::compile(&def).expect("compile");
    let registry = AgentRegistry::from_agents(vec![
        Arc::new(FixedAgent::new(
            "planner",
            r#"{"repos":[{"name":"ork","reason":"test"}],"query":"AgentConfig"}"#,
        )) as Arc<dyn Agent>,
        Arc::new(FixedAgent::new(
            "researcher",
            r#"{"repository":"ork","impacted_files":[],"proposed_changes":[],"risks":[],"open_questions":[]}"#,
        )) as Arc<dyn Agent>,
        Arc::new(FixedAgent::new("synthesizer", r#"{"overview":"ok"}"#)) as Arc<dyn Agent>,
        Arc::new(FixedAgent::new("writer", "final markdown")) as Arc<dyn Agent>,
        Arc::new(FixedAgent::new("reviewer", "VERDICT: PASS")) as Arc<dyn Agent>,
    ]);
    let engine = WorkflowEngine::new(
        Arc::new(NoopWorkflowRepository::default()),
        Arc::new(registry),
    );
    let mut run = WorkflowRun {
        id: WorkflowRunId::new(),
        workflow_id,
        tenant_id,
        status: WorkflowRunStatus::Pending,
        input: serde_json::json!({ "task": "test" }),
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
        .expect("execute change-plan template");

    assert_eq!(run.status, WorkflowRunStatus::Completed);
    assert!(run.output.is_some());
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
        let reply = format!("echo:{text}");
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

#[tokio::test]
async fn one_step_workflow_completes_with_writer_agent() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();
    let now = Utc::now();
    let def = WorkflowDefinition {
        id: workflow_id,
        tenant_id,
        name: "smoke".into(),
        version: "1".into(),
        trigger: WorkflowTrigger::Manual,
        steps: vec![WorkflowStep {
            id: "only".into(),
            agent: "writer".into(),
            tools: vec![],
            prompt_template: "ping".into(),
            depends_on: vec![],
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to: None,
        }],
        created_at: now,
        updated_at: now,
    };

    let graph = compiler::compile(&def).expect("compile");
    let registry =
        AgentRegistry::from_agents(vec![Arc::new(EchoAgent::new("writer")) as Arc<dyn Agent>]);
    let engine = WorkflowEngine::new(
        Arc::new(NoopWorkflowRepository::default()),
        Arc::new(registry),
    );

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
        .expect("execute");

    assert_eq!(run.status, WorkflowRunStatus::Completed);
    assert_eq!(run.step_results.len(), 1);
    assert_eq!(run.step_results[0].output.as_deref(), Some("echo:ping"));
}
