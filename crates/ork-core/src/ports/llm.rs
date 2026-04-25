//! LLM provider port (ADR 0011 native LLM tool-calling).
//!
//! Extends the original Phase-1 chat surface with the type machinery the
//! agent loop in [`crate::ports::agent::Agent::send_stream`] needs:
//!
//! - [`MessageRole::Tool`] and [`ToolCall`] / [`ToolDescriptor`] — the OpenAI
//!   wire shape lifted into ork's domain types so non-OpenAI providers
//!   (Anthropic, Bedrock — see ADR 0012) can re-render the same call graph
//!   without leaking provider-specific JSON into ork-core.
//! - [`ChatRequest::tools`] / [`ChatRequest::tool_choice`] — tool catalog
//!   handed to the model on every iteration of the loop.
//! - [`ChatStreamEvent::ToolCall`] + [`ChatStreamEvent::ToolCallDelta`] —
//!   streaming variant emits aggregated calls for the loop **and** raw
//!   per-chunk deltas for SSE clients that want to render partial JSON.
//! - [`FinishReason`] — surfaced on `Done` so [`crate::ports::agent::Agent`]
//!   knows whether to dispatch tools or fall through to the final message.
//! - [`LlmProvider::capabilities`] — guards against asking a model that
//!   doesn't support tool calls to do so (see ADR 0012 capability negotiation).
//!
//! The new fields are additive and serialise under `skip_serializing_if`
//! defaults, so persisted [`ChatMessage`] history from Phase 1 deserialises
//! into the new shape with empty tool slots.

use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;
use ork_a2a::Part;
use ork_common::error::OrkError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    /// Assistant-emitted tool calls (set on `role = Assistant`). Empty for
    /// system/user/tool messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Required when `role = Tool`; references the [`ToolCall::id`] this
    /// message responds to. `None` for any other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// A2A-aligned multimodal parts (ADR 0003). The OpenAI wire only
    /// emits the textual `content` field today; non-text parts are
    /// reserved for ADR 0016's artifact pipeline. `TODO(ADR-0003/0016)`:
    /// flow `Part::File` into the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
}

impl ChatMessage {
    /// System message constructor; preserves the v1 ergonomics call sites
    /// already use.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            parts: Vec::new(),
        }
    }

    /// User message constructor.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            parts: Vec::new(),
        }
    }

    /// Assistant message constructor — used by the tool loop to append the
    /// LLM's reply (text + any emitted tool calls) back into history before
    /// the next iteration.
    #[must_use]
    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            parts: Vec::new(),
        }
    }

    /// Tool-result message constructor. `tool_call_id` MUST match the
    /// originating [`ToolCall::id`] so the model can correlate the result.
    #[must_use]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            parts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    /// ADR 0011 §`LlmProvider extensions`. Carries a tool result back to
    /// the model after the runtime executed an [`Assistant`](Self::Assistant)
    /// message's [`ToolCall`].
    Tool,
}

/// Single tool invocation emitted by the model. `arguments` is **already
/// parsed** JSON (not the OpenAI wire's stringified form) — providers are
/// responsible for parsing on the way in and stringifying on the way out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// LLM-facing tool descriptor. `parameters` is the raw JSON Schema as
/// published by the upstream tool source (MCP server, peer agent card,
/// integration tool spec) — we deliberately do not re-shape it so vendor
/// keywords like `enum`/`oneOf` survive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tool selection hint passed alongside the tool catalog. Mirrors the
/// OpenAI shape (`auto` / `none` / `required` / `{type:"function",
/// function:{name:"…"}}`) but stays provider-agnostic — Anthropic et al.
/// translate it on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call a tool (default).
    Auto,
    /// Model must not call any tool.
    None,
    /// Model must call at least one tool.
    Required,
    /// Model must call the named tool.
    Named { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub model: Option<String>,
    /// ADR 0012 §`Selection — separate provider + model fields`. Routes the
    /// request to a named entry in the [`crate::ports::llm::LlmProvider`]
    /// catalog (the `LlmRouter` instance backing this trait). `None` falls
    /// through to the tenant default and then the operator default; the
    /// actual resolution lives in `ork_llm::router::LlmRouter::resolve`.
    /// Held here so it survives serialisation through any persisted
    /// request shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Tool catalog the model may call. Empty disables tool calling for
    /// this request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDescriptor>,
    /// Optional [`ToolChoice`]; providers may treat `None` as "auto" when
    /// `tools` is non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

impl ChatRequest {
    /// Backwards-compatible constructor used by call sites that want the
    /// pre-ADR-0011 shape. Equivalent to a brace-init with empty tool
    /// fields.
    #[must_use]
    pub fn simple(
        messages: Vec<ChatMessage>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        model: Option<String>,
    ) -> Self {
        Self {
            messages,
            temperature,
            max_tokens,
            model,
            provider: None,
            tools: Vec::new(),
            tool_choice: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    pub model: String,
    pub usage: TokenUsage,
    /// Tool calls the model wants the runtime to execute. Empty when the
    /// model returned a final text answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Why generation stopped — drives the agent loop's branch on
    /// [`FinishReason::ToolCalls`] vs everything-else.
    #[serde(default = "FinishReason::default_stop")]
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Why the LLM stopped generating. Maps from each provider's wire string;
/// unknown values land in [`Self::Other`] so we never lose information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural stop — final assistant message ready to return to caller.
    Stop,
    /// Model emitted [`ToolCall`]s; runtime must execute them and feed
    /// the results back as [`MessageRole::Tool`] messages.
    ToolCalls,
    /// Hit `max_tokens`; treat as terminal but flag for the caller.
    Length,
    /// Provider-side content filter intervened.
    ContentFilter,
    /// Unknown reason string from the wire. Carrying it through keeps the
    /// loop terminal but preserves the original value for telemetry.
    Other(String),
}

impl FinishReason {
    /// Default for backwards-compat: pre-ADR-0011 callers serialised a
    /// `ChatResponse` without `finish_reason`; reading those back must not
    /// panic.
    #[must_use]
    pub fn default_stop() -> Self {
        Self::Stop
    }
}

impl Default for FinishReason {
    fn default() -> Self {
        Self::default_stop()
    }
}

/// Static capability snapshot for a model. Keeps tool-aware code paths
/// from sending tool catalogs to models that can't handle them, and lets
/// future ADR 0012 surface multimodal/context limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub supports_tools: bool,
    pub supports_streaming: bool,
    pub supports_vision: bool,
    /// `0` means "unknown / provider-defined". Real values land with ADR 0012.
    pub max_context: u32,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_streaming: true,
            supports_vision: false,
            max_context: 0,
        }
    }
}

/// One chunk from a streaming chat completion (OpenAI-style SSE).
///
/// Tool calls surface twice: once as raw [`Self::ToolCallDelta`]
/// fragments (for clients that want to render partial JSON), and once as
/// a fully-aggregated [`Self::ToolCall`] before [`Self::Done`]. The agent
/// loop only consumes the aggregated form.
#[derive(Debug, Clone)]
pub enum ChatStreamEvent {
    /// Plain assistant-text delta.
    Delta(String),
    /// Aggregated tool call ready for dispatch. Emitted exactly once per
    /// tool call, before [`Self::Done`].
    ToolCall(ToolCall),
    /// Raw per-chunk fragment of a tool call's name/arguments. `index`
    /// matches the tool-call slot in the response (multiple calls
    /// interleave).
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// Stream terminator; carries the canonical [`FinishReason`] and
    /// usage totals.
    Done {
        usage: TokenUsage,
        model: String,
        finish_reason: FinishReason,
    },
}

pub type LlmChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, OrkError>> + Send>>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, OrkError>;

    /// Token stream for chat completions. Implementations should end with [`ChatStreamEvent::Done`].
    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError>;

    fn provider_name(&self) -> &str;

    /// Static capabilities for `model`. Default impl returns
    /// [`ModelCapabilities::default`] (everything-on except vision); ADR 0012's
    /// per-provider impls (e.g. `OpenAiCompatibleProvider`) return real
    /// per-model data.
    ///
    /// **For routers, this method returns operator-default-only data and
    /// can be wrong for tenant-overridden providers.** Callers that have
    /// (or can build) a [`ChatRequest`] should prefer
    /// [`Self::capabilities_for`] which honours the full
    /// step → agent → tenant → operator resolution chain.
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    /// Resolve [`ModelCapabilities`] using the same `(provider, model)`
    /// resolution the corresponding `chat`/`chat_stream` would perform
    /// for `request`. Default impl just delegates to [`Self::capabilities`]
    /// using `request.model` (so single-provider impls keep working
    /// unchanged); routers like `ork_llm::router::LlmRouter` override this
    /// to consult the full step → agent → tenant → operator chain via the
    /// in-scope `ResolveContext`.
    async fn capabilities_for(&self, request: &ChatRequest) -> ModelCapabilities {
        let model = request.model.as_deref().unwrap_or("");
        self.capabilities(model)
    }
}
