//! Routes tool invocations by name: registered [`ToolDef`]s, `peer_*`, then `mcp:*` (ADR-0051 / ADR-0010).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::tool_def::ToolDef;
use ork_core::workflow::engine::ToolExecutor;

use crate::agent_call::AgentCallToolExecutor;

/// Dispatches tool calls using a pre-built registry plus MCP and peer delegation fallbacks.
pub struct ToolPlaneExecutor {
    defs: Arc<HashMap<String, Arc<dyn ToolDef>>>,
    agent_call: Option<Arc<AgentCallToolExecutor>>,
    mcp: Option<Arc<dyn ToolExecutor>>,
}

impl ToolPlaneExecutor {
    #[must_use]
    pub fn new(
        defs: Arc<HashMap<String, Arc<dyn ToolDef>>>,
        agent_call: Option<Arc<AgentCallToolExecutor>>,
        mcp: Option<Arc<dyn ToolExecutor>>,
    ) -> Self {
        Self {
            defs,
            agent_call,
            mcp,
        }
    }

    #[must_use]
    pub fn native_defs(&self) -> Arc<HashMap<String, Arc<dyn ToolDef>>> {
        self.defs.clone()
    }

    #[must_use]
    pub fn agent_call(&self) -> Option<&Arc<AgentCallToolExecutor>> {
        self.agent_call.as_ref()
    }
}

#[async_trait]
impl ToolExecutor for ToolPlaneExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        if tool_name.starts_with("mcp:") {
            let mcp = self.mcp.as_ref().ok_or_else(|| {
                OrkError::Integration(format!(
                    "tool `{tool_name}` is mcp-prefixed but the MCP tool plane is not configured (ADR-0010: set [mcp] in config or per-tenant settings)"
                ))
            })?;
            return mcp.execute(ctx, tool_name, input).await;
        }

        if tool_name.starts_with("peer_") {
            let agent_call = self.agent_call.as_ref().ok_or_else(|| {
                OrkError::Integration(format!(
                    "peer tool `{tool_name}` was advertised by the catalog but agent_call is not configured (ADR-0006 not wired in this build)"
                ))
            })?;
            return agent_call.dispatch_peer_tool(ctx, tool_name, input).await;
        }

        let t = self
            .defs
            .get(tool_name)
            .ok_or_else(|| OrkError::NotFound(format!("unknown tool `{tool_name}`")))?;
        t.invoke(ctx, input).await
    }
}
