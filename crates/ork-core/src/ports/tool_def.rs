//! Tool definition port (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md),
//! [`0051`](../../../docs/adrs/0051-code-first-tool-dsl.md)).

use async_trait::async_trait;
use serde_json::Value;

use crate::a2a::AgentContext;

use ork_common::error::OrkError;

/// Default fatal classification for tool errors in the rig loop (ADR [`0047`](../../../docs/adrs/0047-rig-as-local-agent-engine.md)).
/// Kept as a free function so [`RigEngine`](../../agents/rig_engine.rs) and [`ToolDef::is_fatal`] can share one definition.
#[must_use]
pub fn default_fatal_tool_error(err: &OrkError) -> bool {
    matches!(
        err,
        OrkError::Workflow(_)
            | OrkError::Internal(_)
            | OrkError::Database(_)
            | OrkError::Configuration { .. },
    )
}

/// Native or MCP-backed tool registered on `OrkApp` (crate `ork-app`).
#[async_trait]
pub trait ToolDef: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &Value;
    fn output_schema(&self) -> &Value;

    async fn invoke(&self, ctx: &AgentContext, input: &Value) -> Result<Value, OrkError>;

    /// ADR [`0051`](../../../docs/adrs/0051-code-first-tool-dsl.md) §`Failure model`: when `true`, the rig loop aborts
    /// the run (`FatalSlot` in `ork-agents`). Default matches ADR [`0047`](../../../docs/adrs/0047-rig-as-local-agent-engine.md) /
    /// ADR [`0010`](../../../docs/adrs/0010-mcp-tool-plane.md) pre-0051 classifier.
    fn is_fatal(&self, err: &OrkError) -> bool {
        default_fatal_tool_error(err)
    }

    /// When `false`, the tool is omitted from the LLM-visible catalog for this request (ADR-0051 §`dynamic_tools`).
    fn visible(&self, _ctx: &AgentContext) -> bool {
        true
    }
}
