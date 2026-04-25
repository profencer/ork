use std::sync::{Arc, Weak};

use glob::Pattern;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, AgentId};
use ork_core::agent_registry::{AgentRegistry, PeerToolDescription};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::llm::ToolDescriptor;
use ork_integrations::code_tools::CodeToolExecutor;
use ork_integrations::tools::IntegrationToolExecutor;
use serde_json::json;

#[derive(Clone, Debug, PartialEq)]
pub struct McpToolCatalogEntry {
    pub server_id: String,
    pub tool_name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

pub trait McpToolCatalog: Send + Sync {
    fn list_for_tenant(&self, tenant: TenantId) -> Vec<McpToolCatalogEntry>;
}

#[derive(Clone, Default)]
pub struct ToolCatalogBuilder {
    registry: Option<Weak<AgentRegistry>>,
    mcp: Option<Arc<dyn McpToolCatalog>>,
}

impl ToolCatalogBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_registry(mut self, registry: Weak<AgentRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    #[must_use]
    pub fn with_mcp(mut self, mcp: Arc<dyn McpToolCatalog>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    pub async fn for_agent(
        &self,
        ctx: &AgentContext,
        config: &AgentConfig,
    ) -> Result<Vec<ToolDescriptor>, OrkError> {
        let mut out = Vec::new();

        out.extend(self.builtin_descriptors(&config.id).await?);
        out.extend(artifact_descriptors());

        for descriptor in CodeToolExecutor::descriptors() {
            if allow_list_matches(&config.tools, &descriptor.name) {
                out.push(descriptor);
            }
        }

        for name in &config.tools {
            if let Some(descriptor) = IntegrationToolExecutor::descriptor(name) {
                out.push(descriptor);
            }
        }

        if let Some(mcp) = &self.mcp {
            for descriptor in mcp.list_for_tenant(ctx.tenant_id) {
                let name = format!("mcp:{}.{}", descriptor.server_id, descriptor.tool_name);
                if allow_list_matches(&config.tools, &name) {
                    out.push(ToolDescriptor {
                        name,
                        description: descriptor.description.unwrap_or_else(|| {
                            format!("MCP tool {}.{}", descriptor.server_id, descriptor.tool_name)
                        }),
                        parameters: descriptor.input_schema,
                    });
                }
            }
        }

        Ok(out)
    }

    /// Built-in tool descriptors handed to every agent: the generic
    /// `agent_call` plus per-skill `peer_<agent_id>_<skill_id>` entries.
    ///
    /// The calling agent's own `peer_<self>_*` entries are filtered out —
    /// "delegate to yourself" is never useful and tripped the cycle
    /// detector mid-step in the stage-4 demo (see
    /// `builder_does_not_advertise_self_peer_tools` regression test).
    /// Inline regression details: ADR-0006 §`Cycle detection` would still
    /// catch a runaway self-delegation, but only after burning a tool
    /// iteration and producing a confusing error.
    async fn builtin_descriptors(
        &self,
        self_agent_id: &AgentId,
    ) -> Result<Vec<ToolDescriptor>, OrkError> {
        let Some(registry) = self.registry.as_ref().and_then(Weak::upgrade) else {
            return Ok(vec![agent_call_descriptor()]);
        };
        let self_prefix = format!("peer_{self_agent_id}_");
        Ok(registry
            .peer_tool_descriptions()
            .await
            .into_iter()
            .filter(|peer| !peer.name.starts_with(&self_prefix))
            .map(peer_descriptor)
            .collect())
    }
}

fn peer_descriptor(peer: PeerToolDescription) -> ToolDescriptor {
    if peer.name == "agent_call" {
        return agent_call_descriptor();
    }
    ToolDescriptor {
        name: peer.name,
        description: peer.description,
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"},
                "data": {"type": "object"}
            },
            "required": ["prompt"]
        }),
    }
}

fn agent_call_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: "agent_call".into(),
        description: "Delegate work to another agent. Pass `agent` and `prompt`; set `await` false for fire-and-forget.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string"},
                "prompt": {"type": "string"},
                "data": {"type": "object"},
                "await": {"type": "boolean"},
                "stream": {"type": "boolean"}
            },
            "required": ["agent", "prompt"]
        }),
    }
}

fn artifact_descriptors() -> Vec<ToolDescriptor> {
    // TODO(ADR-0016): expose artifact_put/artifact_get once artifact storage is
    // part of the agent runtime. ADR 0011 only reserves this catalog seam.
    Vec::new()
}

fn allow_list_matches(allow_list: &[String], name: &str) -> bool {
    allow_list.iter().any(|allowed| {
        allowed == name
            || Pattern::new(allowed)
                .map(|p| p.matches(name))
                .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use async_trait::async_trait;
    use ork_a2a::{AgentCapabilities, AgentCard, AgentSkill, Message as AgentMessage, TaskId};
    use ork_common::error::OrkError;
    use ork_core::a2a::AgentId;
    use ork_core::ports::agent::{Agent, AgentEventStream};

    /// Bare-bones [`Agent`] stub used by the catalog tests below. We only need
    /// `id()` + `card()`; the catalog never calls `send_stream`.
    struct StubAgent {
        id: AgentId,
        card: AgentCard,
    }

    impl StubAgent {
        fn new(id: &str, skill_id: &str) -> Self {
            Self {
                id: id.into(),
                card: AgentCard {
                    name: id.to_string(),
                    description: "stub".into(),
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
                        id: skill_id.into(),
                        name: skill_id.into(),
                        description: "stub skill".into(),
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
    impl Agent for StubAgent {
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
            unreachable!("catalog tests never invoke send_stream");
        }
        async fn cancel(&self, _ctx: AgentContext, _task_id: &TaskId) -> Result<(), OrkError> {
            unreachable!("catalog tests never cancel");
        }
    }

    fn ctx_for(tenant: TenantId) -> AgentContext {
        AgentContext {
            tenant_id: tenant,
            task_id: ork_a2a::TaskId::new(),
            parent_task_id: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            caller: ork_core::a2a::CallerIdentity {
                tenant_id: tenant,
                user_id: None,
                scopes: vec![],
            },
            push_notification_url: None,
            trace_ctx: None,
            context_id: None,
            workflow_input: serde_json::Value::Null,
            iteration: None,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
            step_llm_overrides: None,
        }
    }

    fn cfg(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            name: id.into(),
            description: "test".into(),
            system_prompt: "sys".into(),
            tools: Vec::new(),
            provider: None,
            model: None,
            temperature: 0.0,
            max_tokens: 1,
            max_tool_iterations: 16,
            max_parallel_tool_calls: 4,
            max_tool_result_bytes: 65_536,
            expose_reasoning: false,
        }
    }

    #[test]
    fn allow_list_supports_exact_and_glob_matches() {
        assert!(allow_list_matches(&["read_file".into()], "read_file"));
        assert!(allow_list_matches(
            &["mcp:atlassian.*".into()],
            "mcp:atlassian.search"
        ));
        assert!(!allow_list_matches(
            &["mcp:atlassian.*".into()],
            "mcp:github.search"
        ));
    }

    #[tokio::test]
    async fn builder_always_exposes_agent_call_builtin() {
        let tenant = TenantId::new();
        let ctx = ctx_for(tenant);
        let config = cfg("a");

        let descriptors = ToolCatalogBuilder::new()
            .for_agent(&ctx, &config)
            .await
            .expect("catalog");
        assert!(descriptors.iter().any(|d| d.name == "agent_call"));
    }

    /// Regression — `synthesize (failed): workflow error: delegation cycle
    /// detected: synthesizer already in chain ["synthesizer"]` from the
    /// stage-4 demo run. Root cause: the catalog handed every agent a
    /// `peer_<self>_<skill>` descriptor, so the LLM driving "synthesizer"
    /// happily picked `peer_synthesizer_default` and "delegated" to itself.
    /// First hop succeeded; the inner synthesizer then did the same and
    /// tripped the cycle detector. Filtering self out of the catalog keeps
    /// the LLM from ever seeing the option.
    #[tokio::test]
    async fn builder_does_not_advertise_self_peer_tools() {
        let tenant = TenantId::new();
        let registry = Arc::new(AgentRegistry::from_agents(vec![
            Arc::new(StubAgent::new("synthesizer", "synth")) as Arc<dyn Agent>,
            Arc::new(StubAgent::new("researcher", "research")) as Arc<dyn Agent>,
        ]));
        let builder = ToolCatalogBuilder::new().with_registry(Arc::downgrade(&registry));

        let descriptors = builder
            .for_agent(&ctx_for(tenant), &cfg("synthesizer"))
            .await
            .expect("catalog");

        assert!(
            descriptors.iter().any(|d| d.name == "agent_call"),
            "the generic agent_call must still be available so the LLM can \
             delegate to *other* peers"
        );
        assert!(
            descriptors
                .iter()
                .any(|d| d.name == "peer_researcher_research"),
            "peers other than self must remain in the catalog; got: {:?}",
            descriptors.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        assert!(
            !descriptors
                .iter()
                .any(|d| d.name.starts_with("peer_synthesizer_")),
            "self peer tools must be filtered so the LLM cannot self-delegate \
             into the cycle detector; got: {:?}",
            descriptors.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }
}
