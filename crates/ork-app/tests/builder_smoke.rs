//! Builder validation and manifest introspection smoke tests.

use async_trait::async_trait;
use futures::stream;
use ork_a2a::{AgentCapabilities, AgentCard};
use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_common::error::OrkError;
use ork_core::a2a::{AgentContext, AgentEvent, AgentMessage};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::tool_def::ToolDef;
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_run::{ImmediateWorkflowRunHandle, WorkflowRunDeps};
use serde_json::{Value, json};

struct MockTool {
    id: String,
    description: String,
    schema_in: Value,
    schema_out: Value,
}

#[async_trait]
impl ToolDef for MockTool {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn description(&self) -> &str {
        self.description.as_str()
    }

    fn input_schema(&self) -> &Value {
        &self.schema_in
    }

    fn output_schema(&self) -> &Value {
        &self.schema_out
    }

    async fn invoke(&self, _ctx: &AgentContext, input: &Value) -> Result<Value, OrkError> {
        Ok(input.clone())
    }
}

struct MockWorkflow {
    id: String,
    description: String,
    tool_refs: Vec<String>,
    agent_refs: Vec<String>,
    cron: Option<(String, String)>,
}

impl WorkflowDef for MockWorkflow {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn description(&self) -> &str {
        self.description.as_str()
    }

    fn referenced_tool_ids(&self) -> &[String] {
        &self.tool_refs
    }

    fn referenced_agent_ids(&self) -> &[String] {
        &self.agent_refs
    }

    fn run<'a>(
        &'a self,
        _ctx: AgentContext,
        input: Value,
        _deps: WorkflowRunDeps,
    ) -> futures::future::BoxFuture<
        'a,
        Result<ork_core::ports::workflow_run::WorkflowRunHandle, OrkError>,
    > {
        Box::pin(async move { Ok(ImmediateWorkflowRunHandle::completed(input)) })
    }

    fn cron_trigger(&self) -> Option<(String, String)> {
        self.cron.clone()
    }
}

fn sample_card(name: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: "test agent".into(),
        version: "0.0.1".into(),
        url: None,
        provider: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![],
        security_schemes: None,
        security: None,
        extensions: None,
    }
}

struct MockAgent {
    id: String,
    card: AgentCard,
}

#[async_trait::async_trait]
impl Agent for MockAgent {
    fn id(&self) -> &ork_core::a2a::AgentId {
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
        let ev = AgentEvent::Message(AgentMessage::agent_text("ack"));
        Ok(Box::pin(stream::once(async move { Ok(ev) })))
    }
}

#[test]
fn build_lists_agents_workflows_tools_in_manifest() {
    let tool = MockTool {
        id: "echo".into(),
        description: "echo tool".into(),
        schema_in: json!({}),
        schema_out: json!({}),
    };
    let wf = MockWorkflow {
        id: "demo-flow".into(),
        description: "demo".into(),
        tool_refs: vec!["echo".into()],
        agent_refs: vec!["alpha".into(), "beta".into()],
        cron: None,
    };

    let app = OrkApp::builder()
        .agent(MockAgent {
            id: "alpha".into(),
            card: sample_card("alpha"),
        })
        .agent(MockAgent {
            id: "beta".into(),
            card: sample_card("beta"),
        })
        .tool(tool)
        .workflow(wf)
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            tls: None,
            auth: None,
            resume_on_startup: false,
        })
        .build()
        .expect("build ok");

    let m = app.manifest();
    assert_eq!(m.agents.len(), 2);
    assert_eq!(m.workflows.len(), 1);
    assert_eq!(m.tools.len(), 1);
    let ids: Vec<_> = m.agents.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&"alpha"));
    assert!(ids.contains(&"beta"));
}

#[test]
fn duplicate_agent_id_rejected() {
    let err = OrkApp::builder()
        .agent(MockAgent {
            id: "dup".into(),
            card: sample_card("dup"),
        })
        .agent(MockAgent {
            id: "dup".into(),
            card: sample_card("dup-a"),
        })
        .build();

    assert!(matches!(err, Err(OrkError::Configuration { .. })));
}

#[test]
fn malformed_ids_rejected() {
    assert!(
        OrkApp::builder()
            .agent(MockAgent {
                id: "Bad".into(),
                card: sample_card("Bad"),
            })
            .build()
            .is_err()
    );

    assert!(
        OrkApp::builder()
            .tool(MockTool {
                id: "-bad".into(),
                description: "".into(),
                schema_in: json!({}),
                schema_out: json!({}),
            })
            .build()
            .is_err()
    );

    let long_str = format!("x{}", "a".repeat(64));
    assert!(
        OrkApp::builder()
            .agent(MockAgent {
                id: long_str,
                card: sample_card("x"),
            })
            .build()
            .is_err()
    );
}

#[test]
fn workflow_unknown_tool_rejected() {
    let wf = MockWorkflow {
        id: "wf".into(),
        description: "".into(),
        tool_refs: vec!["ghost-tool".into()],
        agent_refs: vec![],
        cron: None,
    };
    assert!(OrkApp::builder().workflow(wf).build().is_err());
}

// ADR-0020 §A0: a cron-bearing workflow without a configured `system_tenant`
// would have no real tenant id at fire time; once `TenantTxScope` enforces RLS
// (later in Phase A), such a run would orphan rows or fail FK constraints. The
// builder must reject this misconfiguration up front.
#[test]
fn cron_workflow_without_system_tenant_rejected() {
    let wf = MockWorkflow {
        id: "scheduled".into(),
        description: "".into(),
        tool_refs: vec![],
        agent_refs: vec![],
        cron: Some(("0 0 * * * *".into(), "UTC".into())),
    };
    let err = match OrkApp::builder().workflow(wf).build() {
        Ok(_) => panic!("missing system_tenant must fail"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("scheduled") && msg.contains("system_tenant"),
        "unexpected error: {msg}"
    );
}

#[test]
fn cron_workflow_with_system_tenant_builds() {
    use ork_common::types::TenantId;
    let wf = MockWorkflow {
        id: "scheduled".into(),
        description: "".into(),
        tool_refs: vec![],
        agent_refs: vec![],
        cron: Some(("0 0 * * * *".into(), "UTC".into())),
    };
    OrkApp::builder()
        .workflow(wf)
        .system_tenant(TenantId::new())
        .build()
        .expect("cron workflow with system_tenant must build");
}
