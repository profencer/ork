//! Minimal tool definition port (ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md)).
//! ADR [`0051`](../../../docs/adrs/0051-code-first-tool-dsl.md) will extend this surface.

use serde_json::Value;

/// Native or MCP-backed tool registered on `OrkApp` (crate `ork-app`).
pub trait ToolDef: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &Value;
    fn output_schema(&self) -> &Value;
}
