use std::collections::HashMap;
use std::sync::{Arc, Weak};

use glob::Pattern;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::AgentContext;
use ork_core::agent_registry::{AgentRegistry, PeerToolDescription};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::tool_def::ToolDef;
use ork_core::workflow::engine::ToolExecutor;
use ork_integrations::agent_call::AgentCallToolExecutor;
use ork_integrations::native_tool_defs::PeerSkillToolDef;
use ork_tool::DynToolInvoke;
use serde_json::{Value, json};

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
    native_tools: Option<Arc<HashMap<String, Arc<dyn ToolDef>>>>,
    agent_call: Option<Arc<AgentCallToolExecutor>>,
    mcp_catalog: Option<Arc<dyn McpToolCatalog>>,
    mcp_executor: Option<Arc<dyn ToolExecutor>>,
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

    /// Native tools (GitHub/GitLab/code/artifact/`agent_call`) as [`ToolDef`] values (ADR-0051).
    #[must_use]
    pub fn with_native_tools(mut self, tools: Arc<HashMap<String, Arc<dyn ToolDef>>>) -> Self {
        self.native_tools = Some(tools);
        self
    }

    /// Required for `peer_*` catalog entries (dispatch shares [`AgentCallToolExecutor`]).
    #[must_use]
    pub fn with_agent_call_for_peers(mut self, exec: Arc<AgentCallToolExecutor>) -> Self {
        self.agent_call = Some(exec);
        self
    }

    /// MCP tools: descriptor list + the same [`ToolExecutor`] used for `mcp:*` invocations (ADR-0010).
    #[must_use]
    pub fn with_mcp_plane(
        mut self,
        catalog: Arc<dyn McpToolCatalog>,
        executor: Arc<dyn ToolExecutor>,
    ) -> Self {
        self.mcp_catalog = Some(catalog);
        self.mcp_executor = Some(executor);
        self
    }

    pub async fn for_agent(
        &self,
        ctx: &AgentContext,
        config: &AgentConfig,
    ) -> Result<Vec<Arc<dyn ToolDef>>, OrkError> {
        let mut out = Vec::<Arc<dyn ToolDef>>::new();

        if let Some(map) = &self.native_tools {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            for name in keys {
                let Some(def) = map.get(&name) else {
                    continue;
                };
                if !allow_list_matches(&config.tools, &name) {
                    continue;
                }
                if !def.visible(ctx) {
                    continue;
                }
                out.push(def.clone());
            }
        }

        if let (Some(registry), Some(agent_call)) = (
            self.registry.as_ref().and_then(Weak::upgrade),
            self.agent_call.as_ref(),
        ) {
            let self_prefix = format!("peer_{}_", config.id);
            for peer in registry.peer_tool_descriptions().await {
                if peer.name == "agent_call" {
                    continue;
                }
                if peer.name.starts_with(&self_prefix) {
                    continue;
                }
                if !allow_list_matches(&config.tools, &peer.name) {
                    continue;
                }
                let def: Arc<dyn ToolDef> = Arc::new(PeerSkillToolDef::new(
                    peer.name.clone(),
                    peer.description.clone(),
                    peer_tool_parameters(&peer),
                    agent_call.clone(),
                ));
                if !def.visible(ctx) {
                    continue;
                }
                out.push(def);
            }
        }

        if let (Some(mcp_cat), Some(mcp_exec)) = (&self.mcp_catalog, &self.mcp_executor) {
            for entry in mcp_cat.list_for_tenant(ctx.tenant_id) {
                let name = format!("mcp:{}.{}", entry.server_id, entry.tool_name);
                if !allow_list_matches(&config.tools, &name) {
                    continue;
                }
                let description = entry
                    .description
                    .unwrap_or_else(|| format!("MCP tool {}.{}", entry.server_id, entry.tool_name));
                let exec = mcp_exec.clone();
                let name_for_closure = name.clone();
                let def: Arc<dyn ToolDef> = Arc::new(
                    DynToolInvoke::new(
                        name.clone(),
                        description,
                        entry.input_schema.clone(),
                        json!({"type": "object"}),
                        Arc::new(move |c, input| {
                            let exec = exec.clone();
                            let n = name_for_closure.clone();
                            Box::pin(async move { exec.execute(&c, &n, &input).await })
                        }),
                    )
                    .force_non_fatal(),
                );
                if !def.visible(ctx) {
                    continue;
                }
                out.push(def);
            }
        }

        out.sort_by(|a, b| a.id().cmp(b.id()));
        Ok(out)
    }
}

fn peer_tool_parameters(_peer: &PeerToolDescription) -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string"},
            "data": {"type": "object"}
        },
        "required": ["prompt"]
    })
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
            artifact_store: None,
            artifact_public_base: None,
        }
    }

    fn cfg(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            name: id.into(),
            description: "test".into(),
            system_prompt: "sys".into(),
            // Broad allow-list: tests filter visibility by builder wiring, not by empty allow-list.
            tools: vec!["*".into()],
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

    fn agent_call_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string"},
                "prompt": {"type": "string"},
                "data": {"type": "object"},
                "await": {"type": "boolean"},
                "stream": {"type": "boolean"}
            },
            "required": ["agent", "prompt"]
        })
    }

    fn catalog_with_agent_call_stub() -> Arc<HashMap<String, Arc<dyn ToolDef>>> {
        let mut m = HashMap::new();
        let def: Arc<dyn ToolDef> = Arc::new(DynToolInvoke::new(
            "agent_call",
            "Delegate work to another agent. Pass `agent` and `prompt`; set `await` false for fire-and-forget.",
            agent_call_schema(),
            json!({"type": "object"}),
            Arc::new(|_c, input| Box::pin(async move { Ok(input) })),
        ));
        m.insert("agent_call".into(), def);
        Arc::new(m)
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

        let tools = ToolCatalogBuilder::new()
            .with_native_tools(catalog_with_agent_call_stub())
            .for_agent(&ctx, &config)
            .await
            .expect("catalog");
        assert!(tools.iter().any(|d| d.id() == "agent_call"));
    }

    #[tokio::test]
    async fn builder_does_not_advertise_self_peer_tools() {
        let tenant = TenantId::new();
        let registry = Arc::new(AgentRegistry::from_agents(vec![
            Arc::new(StubAgent::new("synthesizer", "synth")) as Arc<dyn Agent>,
            Arc::new(StubAgent::new("researcher", "research")) as Arc<dyn Agent>,
        ]));
        let agent_call = Arc::new(AgentCallToolExecutor::new(
            Arc::downgrade(&registry),
            None,
            None,
        ));
        let mut native = HashMap::new();
        let body: Arc<dyn ToolDef> = Arc::new(DynToolInvoke::new(
            "agent_call",
            "Delegate work to another agent.",
            agent_call_schema(),
            json!({"type": "object"}),
            Arc::new({
                let ac = agent_call.clone();
                move |ctx, input| {
                    let ac = ac.clone();
                    Box::pin(async move { ac.execute(&ctx, "agent_call", &input).await })
                }
            }),
        ));
        native.insert("agent_call".into(), body);

        let builder = ToolCatalogBuilder::new()
            .with_registry(Arc::downgrade(&registry))
            .with_native_tools(Arc::new(native))
            .with_agent_call_for_peers(agent_call);

        let descriptors = builder
            .for_agent(&ctx_for(tenant), &cfg("synthesizer"))
            .await
            .expect("catalog");

        assert!(
            descriptors.iter().any(|d| d.id() == "agent_call"),
            "the generic agent_call must still be available so the LLM can \
             delegate to *other* peers"
        );
        assert!(
            descriptors
                .iter()
                .any(|d| d.id() == "peer_researcher_research"),
            "peers other than self must remain in the catalog; got: {:?}",
            descriptors.iter().map(|d| d.id()).collect::<Vec<_>>()
        );
        assert!(
            !descriptors
                .iter()
                .any(|d| d.id().starts_with("peer_synthesizer_")),
            "self peer tools must be filtered so the LLM cannot self-delegate \
             into the cycle detector; got: {:?}",
            descriptors.iter().map(|d| d.id()).collect::<Vec<_>>()
        );
    }
}
