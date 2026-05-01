//! `AppManifest` JSON round-trip (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md)).

use ork_a2a::{AgentCapabilities, AgentCard};
use ork_app::OrkApp;
use ork_app::types::{
    Environment, McpServerSpec, McpTransportStub, ObservabilityConfig, ScorerSpec, ScorerTarget,
    ServerConfig,
};
use ork_common::error::OrkError;
use ork_core::ports::memory_store::MemoryStore;
use ork_core::ports::tool_def::ToolDef;
use ork_core::ports::vector_store::VectorStore;
use ork_core::ports::workflow_def::WorkflowDef;
use proptest::prelude::*;
use serde_json::{Value, json};

struct MockTool {
    id: String,
    description: String,
    si: Value,
    so: Value,
}

impl ToolDef for MockTool {
    fn id(&self) -> &str {
        &self.id
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> &Value {
        &self.si
    }
    fn output_schema(&self) -> &Value {
        &self.so
    }
}

struct MockWf {
    id: String,
}

impl WorkflowDef for MockWf {
    fn id(&self) -> &str {
        &self.id
    }
    fn description(&self) -> &str {
        "w"
    }
    fn referenced_tool_ids(&self) -> &[String] {
        &[]
    }
    fn referenced_agent_ids(&self) -> &[String] {
        &[]
    }
    fn run<'a>(
        &'a self,
        _ctx: ork_core::a2a::AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, OrkError>> {
        Box::pin(async move { Ok(input) })
    }
}

struct MockAgent {
    id: String,
    card: AgentCard,
}

#[async_trait::async_trait]
impl ork_core::ports::agent::Agent for MockAgent {
    fn id(&self) -> &ork_core::a2a::AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    async fn send_stream(
        &self,
        _ctx: ork_core::a2a::AgentContext,
        _msg: ork_core::a2a::AgentMessage,
    ) -> Result<ork_core::ports::agent::AgentEventStream, OrkError> {
        unimplemented!()
    }
}

fn card(name: &str) -> AgentCard {
    AgentCard {
        name: name.into(),
        description: "d".into(),
        version: "1".into(),
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

struct Mem;
impl MemoryStore for Mem {
    fn name(&self) -> &str {
        "mem1"
    }
}

struct Vecs;
impl VectorStore for Vecs {
    fn name(&self) -> &str {
        "vec1"
    }
}

fn roundtrip(m: &ork_app::manifest::AppManifest) {
    let v = serde_json::to_value(m).expect("to_value");
    let back: ork_app::manifest::AppManifest = serde_json::from_value(v).expect("from_value");
    assert_eq!(*m, back);
}

#[test]
fn manifest_roundtrip_minimal_registry() {
    let app = OrkApp::builder()
        .server(ServerConfig::default())
        .build()
        .expect("build");
    let m = app.manifest();
    roundtrip(&m);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Property-style randomized manifest shapes; ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md) § Acceptance.
    #[test]
    fn manifest_json_roundtrip_property(
        tag in any::<u32>(),
        port in any::<u16>(),
        toggles in any::<u8>(),
        env_roll in any::<u8>(),
    ) {
        let agent_id = format!("ag-{tag:x}");
        let wf_id = format!("wf-{tag:x}");
        let tool_id = format!("tl-{tag:x}");

        let mut b = OrkApp::builder().server(ServerConfig {
            host: "127.0.0.1".into(),
            port,
            ..ServerConfig::default()
        });

        if toggles & 1 != 0 {
            b = b
                .agent(MockAgent {
                    id: agent_id.clone(),
                    card: card(&agent_id),
                })
                .tool(MockTool {
                    id: tool_id.clone(),
                    description: format!("tool-{tag:x}"),
                    si: json!({"type":"object"}),
                    so: json!({"type":"string"}),
                })
                .workflow(MockWf { id: wf_id.clone() })
                .mcp_server(
                    "docs",
                    McpServerSpec {
                        transport: McpTransportStub::Deferred,
                    },
                );
        }
        if toggles & 2 != 0 {
            b = b.memory(Mem);
        }
        if toggles & 4 != 0 {
            b = b.vectors(Vecs);
        }
        // Scorers reference an agent id; only attach when we registered that agent above.
        if toggles & 8 != 0 && (toggles & 1) != 0 {
            b = b.scorer(
                ScorerTarget::Agent {
                    id: agent_id.clone(),
                },
                ScorerSpec {
                    id: format!("sc-{tag:x}"),
                    label: Some(format!("lbl-{tag:x}")),
                },
            );
        }
        if toggles & 16 != 0 {
            b = b.observability(ObservabilityConfig {
                traces: (toggles & 32) == 0,
                metrics: (toggles & 32) != 0,
            });
        }
        if toggles & 64 != 0 {
            b = b.request_context_schema(json!({
                "type": "object",
                "additionalProperties": { "type": "string" },
            }));
        }

        let env_variant = env_roll % 3;
        b = match env_variant {
            0 => b.environment(Environment::Development),
            1 => b.environment(Environment::Staging),
            _ => b.environment(Environment::Production),
        };

        let app = b
            .build()
            .expect("builder must succeed — deterministic valid ids and MCP dedup enforced");
        let m = app.manifest();
        let v = serde_json::to_value(&m).expect("serde_json manifest");
        let back: ork_app::manifest::AppManifest =
            serde_json::from_value(v).expect("round-trip JSON");
        prop_assert_eq!(m, back);
    }
}

#[test]
fn manifest_roundtrip_with_optionals() {
    let app = OrkApp::builder()
        .agent(MockAgent {
            id: "alice".into(),
            card: card("alice"),
        })
        .tool(MockTool {
            id: "t1".into(),
            description: "td".into(),
            si: json!({"type":"object"}),
            so: json!({"type":"string"}),
        })
        .workflow(MockWf { id: "w1".into() })
        .mcp_server(
            "docs",
            McpServerSpec {
                transport: McpTransportStub::Deferred,
            },
        )
        .memory(Mem)
        .vectors(Vecs)
        .scorer(
            ScorerTarget::Agent { id: "alice".into() },
            ScorerSpec {
                id: "s1".into(),
                label: Some("l".into()),
            },
        )
        .observability(ObservabilityConfig {
            traces: true,
            metrics: false,
        })
        .request_context_schema(json!({"type":"object"}))
        .environment(Environment::Staging)
        .server(ServerConfig {
            host: "0.0.0.0".into(),
            port: 3000,
            tls: None,
            auth: Some(ork_app::types::AuthConfig { mode: "jwt".into() }),
        })
        .build()
        .expect("build");
    roundtrip(&app.manifest());
}
