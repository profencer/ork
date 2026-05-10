//! ADR-0056 §`Request/response shape`: the public DTOs of the
//! auto-generated REST surface. All structs derive
//! [`schemars::JsonSchema`] so [`crate::openapi::openapi_spec`] can lift
//! them into `components/schemas/...`.
//!
//! A2A-typed payloads (`Message`, `Part`) are exposed as
//! [`serde_json::Value`] in the schema layer to avoid dragging
//! `JsonSchema` derives into [`ork_a2a`]; runtime parsing still resolves
//! through `serde::Deserialize` on the proper types when the handler
//! reads the body. ADR-0056 explicitly accepts this trade-off in the
//! "Alternatives considered" section.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------- Manifest passthroughs ----------

/// `GET /api/agents` row.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentSummary {
    pub id: String,
    pub description: String,
    pub card_name: String,
}

/// `GET /api/agents/:id` body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentDetail {
    pub id: String,
    pub description: String,
    pub card_name: String,
    /// Skill summaries lifted from the registered [`AgentCard`](ork_a2a::AgentCard).
    /// Embedded as opaque JSON; ADR-0005 owns the schema.
    pub skills: Value,
    /// Optional JSON Schema validating the per-call `request_context`
    /// (currently the OrkApp-level schema; see ADR-0056 for v1 scope).
    pub request_context_schema: Option<Value>,
}

/// `GET /api/workflows` row.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSummary {
    pub id: String,
    pub description: String,
}

/// `GET /api/workflows/:id` body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowDetail {
    pub id: String,
    pub description: String,
    /// `[(tool_id, _)]` — IDs the workflow `referenced_tool_ids()` exposes.
    pub referenced_tools: Vec<String>,
    pub referenced_agents: Vec<String>,
    pub cron_trigger: Option<CronTriggerDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronTriggerDetail {
    pub expression: String,
    pub timezone: String,
}

/// `GET /api/tools` row.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolSummary {
    pub id: String,
    pub description: String,
}

/// `GET /api/tools/:id` body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolDetail {
    pub id: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

// ---------- Agent generate ----------

/// `POST /api/agents/:id/generate` body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentGenerateInput {
    /// A2A [`Message`](ork_a2a::Message) (`role: "user"`, typed parts).
    pub message: Value,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub resource_id: Option<String>,
    /// Validated against the OrkApp-level `request_context_schema` if
    /// configured (ADR-0056 §`Request/response shape`).
    #[serde(default)]
    pub request_context: Option<Value>,
    #[serde(default)]
    pub options: AgentRunOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunOptions {
    pub temperature: Option<f32>,
    pub max_steps: Option<u32>,
}

/// `POST /api/agents/:id/generate` response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentGenerateOutput {
    pub run_id: String,
    /// Final assistant [`Message`](ork_a2a::Message).
    pub message: Value,
    pub structured_output: Option<Value>,
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Cancelled,
    Error,
    Unknown,
}

// ---------- Workflows ----------

/// `POST /api/workflows/:id/run` body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunInput {
    pub input: Value,
    #[serde(default)]
    pub options: AgentRunOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunStarted {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunSummary {
    pub run_id: String,
    pub workflow_id: String,
    pub status: RunStatus,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunState {
    pub run_id: String,
    pub workflow_id: String,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub output: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

// ---------- Tool invoke ----------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolInvokeInput {
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolInvokeOutput {
    pub output: Value,
}

// ---------- Memory ----------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadSummaryDto {
    pub thread_id: String,
    pub last_message_at: String,
    pub message_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppendMessageInput {
    /// A2A [`Message`](ork_a2a::Message).
    pub message: Value,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppendMessageOutput {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkingMemoryRead {
    pub value: Option<Value>,
    pub schema: Option<Value>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkingMemoryWrite {
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OkResponse {
    pub ok: bool,
}

// ---------- Scorers ----------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScorerBindingSummary {
    pub target: Value,
    pub scorer_id: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScorerRow {
    pub run_id: String,
    pub scorer_id: String,
    pub target: Value,
    pub score: f64,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScorerRowList {
    pub rows: Vec<ScorerRow>,
}
