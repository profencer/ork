//! Tool execution backed by the `OrkApp` tool registry (ADR [`0051`](../../docs/adrs/0051-code-first-tool-dsl.md)).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::tool_def::ToolDef;
use ork_core::workflow::engine::ToolExecutor;

/// Invokes tools by consulting the same `HashMap` stored on [`crate::OrkAppInner`](super::inner::OrkAppInner).
#[derive(Clone)]
pub struct OrkAppToolExecutor {
    tools: Arc<HashMap<String, Arc<dyn ToolDef>>>,
}

impl OrkAppToolExecutor {
    #[must_use]
    pub fn new(tools: Arc<HashMap<String, Arc<dyn ToolDef>>>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl ToolExecutor for OrkAppToolExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let t = self
            .tools
            .get(name)
            .ok_or_else(|| OrkError::NotFound(format!("tool `{name}`")))?;
        t.invoke(ctx, input).await
    }
}
