use serde::{Deserialize, Serialize};

use crate::a2a::AgentId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: AgentId,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// LLM-facing allow-list for tools this agent may call. Builtins such as
    /// `agent_call` can be exposed in addition to these entries.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Optional per-agent provider override (ADR 0012 §`Selection`). When
    /// set, requests routed through this agent prefer the named entry in
    /// the operator/tenant LLM provider catalog. `None` falls through to
    /// the workflow step, then tenant default, then operator default.
    #[serde(default)]
    pub provider: Option<String>,
    /// Optional per-agent model override. Resolved after the provider
    /// (ADR 0012 §`Selection`); when both are `None` the router uses the
    /// resolved provider's `default_model`.
    #[serde(default)]
    pub model: Option<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    #[serde(default = "default_max_parallel_tool_calls")]
    pub max_parallel_tool_calls: usize,
    #[serde(default = "default_max_tool_result_bytes")]
    pub max_tool_result_bytes: usize,
    #[serde(default)]
    pub expose_reasoning: bool,
}

#[must_use]
pub const fn default_max_tool_iterations() -> usize {
    16
}

#[must_use]
pub const fn default_max_parallel_tool_calls() -> usize {
    4
}

#[must_use]
pub const fn default_max_tool_result_bytes() -> usize {
    65_536
}
