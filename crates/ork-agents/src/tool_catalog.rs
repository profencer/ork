use std::sync::{Arc, Weak};

use glob::Pattern;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::AgentContext;
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

        out.extend(self.builtin_descriptors().await?);
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

    async fn builtin_descriptors(&self) -> Result<Vec<ToolDescriptor>, OrkError> {
        let Some(registry) = self.registry.as_ref().and_then(Weak::upgrade) else {
            return Ok(vec![agent_call_descriptor()]);
        };
        Ok(registry
            .peer_tool_descriptions()
            .await
            .into_iter()
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
        let ctx = AgentContext {
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
        };
        let config = AgentConfig {
            id: "a".into(),
            name: "a".into(),
            description: "test".into(),
            system_prompt: "sys".into(),
            tools: Vec::new(),
            model: None,
            temperature: 0.0,
            max_tokens: 1,
            max_tool_iterations: 16,
            max_parallel_tool_calls: 4,
            max_tool_result_bytes: 65_536,
            expose_reasoning: false,
        };

        let descriptors = ToolCatalogBuilder::new()
            .for_agent(&ctx, &config)
            .await
            .expect("catalog");
        assert!(descriptors.iter().any(|d| d.name == "agent_call"));
    }
}
