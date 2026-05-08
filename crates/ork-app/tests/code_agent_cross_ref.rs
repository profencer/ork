//! ADR-0052 §`Acceptance criteria` #5 — agent-on-agent / agent-on-workflow /
//! agent-on-MCP-server cross-references are validated at `OrkApp::build()`,
//! independent of the order in which components were registered.
//!
//! Uses a stand-alone `MockAgent` so the test stays out of the `ork-agents`
//! dep graph (the cross-ref logic lives on the `Agent` port in `ork-core`).

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream;
use ork_a2a::{AgentCapabilities, AgentCard};
use ork_app::OrkApp;
use ork_app::types::{McpServerSpec, ServerConfig};
use ork_common::error::OrkError;
use ork_core::a2a::{AgentContext, AgentEvent, AgentMessage};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_run::{ImmediateWorkflowRunHandle, WorkflowRunDeps};
use serde_json::Value;

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
    agent_refs: Vec<String>,
    workflow_refs: Vec<String>,
    mcp_refs: Vec<String>,
}

impl MockAgent {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            card: sample_card(id),
            agent_refs: Vec::new(),
            workflow_refs: Vec::new(),
            mcp_refs: Vec::new(),
        }
    }
    fn with_agent_refs(mut self, refs: &[&str]) -> Self {
        self.agent_refs = refs.iter().map(|s| (*s).into()).collect();
        self
    }
    fn with_workflow_refs(mut self, refs: &[&str]) -> Self {
        self.workflow_refs = refs.iter().map(|s| (*s).into()).collect();
        self
    }
    fn with_mcp_refs(mut self, refs: &[&str]) -> Self {
        self.mcp_refs = refs.iter().map(|s| (*s).into()).collect();
        self
    }
}

#[async_trait]
impl Agent for MockAgent {
    fn id(&self) -> &ork_core::a2a::AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    fn referenced_agent_ids(&self) -> &[String] {
        &self.agent_refs
    }
    fn referenced_workflow_ids(&self) -> &[String] {
        &self.workflow_refs
    }
    fn referenced_mcp_server_ids(&self) -> &[String] {
        &self.mcp_refs
    }
    async fn send_stream(
        &self,
        _ctx: AgentContext,
        _msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        Ok(Box::pin(stream::empty::<Result<AgentEvent, OrkError>>()))
    }
}

struct MockWorkflow {
    id: String,
}

impl WorkflowDef for MockWorkflow {
    fn id(&self) -> &str {
        self.id.as_str()
    }
    fn description(&self) -> &str {
        "stub workflow"
    }
    fn referenced_tool_ids(&self) -> &[String] {
        &[]
    }
    fn referenced_agent_ids(&self) -> &[String] {
        &[]
    }
    fn run<'a>(
        &'a self,
        _ctx: AgentContext,
        input: Value,
        _deps: WorkflowRunDeps,
    ) -> BoxFuture<'a, Result<ork_core::ports::workflow_run::WorkflowRunHandle, OrkError>> {
        Box::pin(async move { Ok(ImmediateWorkflowRunHandle::completed(input)) })
    }
}

fn cfg() -> ServerConfig {
    ServerConfig::default()
}

fn assert_cfg_err<T>(result: Result<T, OrkError>, must_contain: &[&str]) {
    let err = match result {
        Ok(_) => panic!("expected OrkError::Configuration, got Ok"),
        Err(e) => e,
    };
    let msg = match err {
        OrkError::Configuration { message } => message,
        other => panic!("expected OrkError::Configuration, got {other:?}"),
    };
    for fragment in must_contain {
        assert!(
            msg.contains(fragment),
            "expected message {msg:?} to contain {fragment:?}"
        );
    }
}

#[test]
fn agent_as_tool_is_validated_at_orkapp_build_regardless_of_order() {
    // Forward ref: register the consumer first, the peer second.
    let app_forward = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("triage").with_agent_refs(&["forecaster"]))
        .agent(MockAgent::new("forecaster"))
        .build()
        .expect("forward ref must validate after both agents registered");
    assert!(app_forward.agent("triage").is_some());
    assert!(app_forward.agent("forecaster").is_some());

    // Backward ref: peer first, then consumer.
    let app_backward = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("forecaster"))
        .agent(MockAgent::new("triage").with_agent_refs(&["forecaster"]))
        .build()
        .expect("backward ref must validate symmetrically");
    assert!(app_backward.agent("triage").is_some());
}

#[test]
fn agent_as_tool_unknown_peer_is_rejected() {
    let r = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("triage").with_agent_refs(&["forecaster"]))
        .build();
    assert_cfg_err(
        r,
        &["agent `triage`", "agent `forecaster`", "not registered"],
    );
}

#[test]
fn agent_as_tool_self_reference_is_rejected() {
    let r = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("loop").with_agent_refs(&["loop"]))
        .build();
    assert_cfg_err(r, &["agent `loop`", "self-delegation"]);
}

#[test]
fn workflow_as_tool_is_validated_at_orkapp_build() {
    let app = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("scheduler").with_workflow_refs(&["nightly"]))
        .workflow(MockWorkflow {
            id: "nightly".into(),
        })
        .build()
        .expect("workflow ref must validate when both registered");
    assert!(app.agent("scheduler").is_some());

    let r = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("scheduler").with_workflow_refs(&["nightly"]))
        .build();
    assert_cfg_err(
        r,
        &["agent `scheduler`", "workflow `nightly`", "not registered"],
    );
}

#[test]
fn tool_server_is_validated_at_orkapp_build() {
    let app = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("docs-bot").with_mcp_refs(&["docs"]))
        .mcp_server("docs", McpServerSpec::default())
        .build()
        .expect("MCP server ref must validate when registered");
    assert!(app.agent("docs-bot").is_some());

    let r = OrkApp::builder()
        .server(cfg())
        .agent(MockAgent::new("docs-bot").with_mcp_refs(&["docs"]))
        .build();
    assert_cfg_err(
        r,
        &["agent `docs-bot`", "MCP server `docs`", "not registered"],
    );
}
