//! Per-invocation context for native tools (ADR [`0051`](../../../docs/adrs/0051-code-first-tool-dsl.md)).

use ork_common::types::WorkflowRunId;
use ork_core::a2a::AgentContext;

/// Carries task-local state into a tool closure.
#[derive(Clone)]
pub struct ToolContext {
    pub agent_context: AgentContext,
    /// Present when the tool is invoked from a code-first workflow step (`ork-workflow`).
    pub run_id: Option<WorkflowRunId>,
    /// ADR-0053 memory surface — placeholder until that ADR lands.
    pub memory: (),
}

impl ToolContext {
    #[must_use]
    pub fn from_agent_context(agent_context: AgentContext) -> Self {
        Self {
            agent_context,
            run_id: None,
            memory: (),
        }
    }
}
