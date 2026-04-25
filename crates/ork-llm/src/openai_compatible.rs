//! Generic OpenAI Chat Completions wire client (ADR 0012 §`Decision`).
//!
//! Drop-in replacement for the deleted `crates/ork-llm/src/minimax.rs` — every
//! byte of the SSE parser, [`ToolCallAggregator`], and OpenAI request/response
//! serde shapes was lifted across without behaviour change. The only thing
//! that moved is the constructor surface: instead of a hard-coded
//! `MINIMAX_API_KEY` env-var read we accept an arbitrary header map plus a
//! base URL, both pre-resolved by [`crate::router::LlmRouter`] from the
//! operator/tenant catalog. The provider id ("openai", "anthropic", "minimax",
//! …) is also caller-supplied so [`LlmProvider::provider_name`] reflects the
//! catalog entry, not a hard-coded vendor name.
//!
//! Tool-call wire shape (OpenAI compatible):
//!
//! - Request: `tools: [{type:"function", function:{name, description, parameters}}]`
//!   plus `tool_choice` (`auto` / `none` / `required` / named function).
//! - Non-stream response: `choices[0].message.tool_calls = [{id, type:"function",
//!   function:{name, arguments: STRING}}]` with `finish_reason = "tool_calls"`.
//! - Stream: `choices[0].delta.tool_calls = [{index, id?, function?:{name?,
//!   arguments?}}]` interleaved across chunks; we aggregate per-`index` slot
//!   and emit one [`ChatStreamEvent::ToolCall`] when the slot's `arguments`
//!   parses as JSON or when `finish_reason = "tool_calls"` arrives.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use ork_common::config::ModelCapabilitiesEntry;
use ork_common::error::OrkError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use ork_core::ports::llm::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream,
    LlmProvider, MessageRole, ModelCapabilities, TokenUsage, ToolCall, ToolChoice, ToolDescriptor,
};

/// OpenAI-compatible chat-completions client. The provider id is whatever
/// [`crate::router::LlmRouter`] passed in (the catalog entry's `id`); base
/// URL and headers are pre-resolved (env vars already looked up at boot per
/// the ADR's "fail loud at boot" rule).
#[derive(Debug)]
pub struct OpenAiCompatibleProvider {
    client: Client,
    /// Catalog id ("openai", "anthropic", …). Surfaced verbatim through
    /// [`LlmProvider::provider_name`].
    provider_id: String,
    base_url: String,
    /// Header map applied to every chat / chat_stream request. Each entry
    /// is fed straight into reqwest's `header()` builder; HTTP itself
    /// treats header names case-insensitively (and HTTP/2 lowercases on
    /// the wire), so the catalog YAML / TOML can spell `Authorization`,
    /// `authorization`, or `AUTHORIZATION` interchangeably.
    headers: HashMap<String, String>,
    default_model: Option<String>,
    /// Per-model capability table. Empty ⇒ fall back to
    /// [`ModelCapabilities::default`] for any lookup.
    capabilities: Vec<ModelCapabilitiesEntry>,
}

impl OpenAiCompatibleProvider {
    /// Construct a fully-resolved provider. Caller is expected to have
    /// already turned `{ env = "FOO" }` header values into literal strings
    /// (the router does this once at boot — see
    /// [`crate::router::LlmRouter::from_config`]). `default_model` is
    /// the per-provider fallback used when the request, agent, step and
    /// tenant defaults are all `None`.
    #[must_use]
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        default_model: Option<String>,
        headers: HashMap<String, String>,
        capabilities: Vec<ModelCapabilitiesEntry>,
    ) -> Self {
        Self {
            client: Client::new(),
            provider_id: provider_id.into(),
            base_url: base_url.into(),
            headers,
            default_model,
            capabilities,
        }
    }

    /// Apply this provider's resolved header set to a `reqwest::RequestBuilder`.
    fn apply_headers(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req
    }
}

/// Maximum number of HTTP attempts (initial + retries) for a streaming
/// chat completion. Set to **2** so a single transient mid-stream
/// failure (a closed connection from the upstream gateway before any
/// SSE event is yielded) is masked, while a second consecutive failure
/// surfaces promptly. Bumping this requires also bumping
/// [`STREAM_RETRY_BACKOFF`] to avoid hot-looping a flapping upstream.
///
/// See `docs/incidents/2026-04-25-workflow-cascades-past-failed-step.md`
/// and the regression in `tests/openai_compatible_stream_retry.rs`.
const STREAM_MAX_ATTEMPTS: usize = 2;

/// Backoff between HTTP attempts. Small enough that the demo doesn't
/// feel laggy on a one-off blip, large enough that a flapping endpoint
/// can recover before we re-fire.
const STREAM_RETRY_BACKOFF: Duration = Duration::from_millis(150);

/// Outcome of a single HTTP attempt at `chat/completions`. Splitting
/// transport / 5xx (retryable) from 4xx (auth, validation, quota —
/// hard failures the caller should see immediately) lets
/// [`OpenAiCompatibleProvider::chat_stream`] burn at most one extra
/// request on flapping connectivity while still surfacing genuinely
/// fatal responses on the first attempt.
enum AttemptError {
    /// Connection / TLS reset, body-decode failures during the
    /// response head, 5xx responses, etc. Worth one retry.
    Transient(OrkError),
    /// 4xx responses, body-serialise errors — anything where retrying
    /// can only make matters worse.
    Fatal(OrkError),
}

impl AttemptError {
    fn into_inner(self) -> OrkError {
        match self {
            Self::Transient(e) | Self::Fatal(e) => e,
        }
    }
}

/// One full HTTP attempt of the streaming chat-completions endpoint:
/// build + send the request, then classify the outcome. Pulled out of
/// [`OpenAiCompatibleProvider::chat_stream`] so initial sends and
/// in-stream retries share one implementation.
async fn send_chat_stream_attempt(
    client: &Client,
    url: &str,
    body_bytes: &[u8],
    headers: &HashMap<String, String>,
    provider_id: &str,
) -> Result<reqwest::Response, AttemptError> {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .body(body_bytes.to_vec());
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    // Anything `reqwest::send().await` returns is by definition a
    // transport-level failure (DNS, TCP/TLS reset, write timeout, …);
    // we treat them all as transient. The mirroring symmetric case —
    // body decode errors during the response stream — is handled
    // inside the `async_stream` block in `chat_stream`.
    let resp = req.send().await.map_err(|e| {
        AttemptError::Transient(OrkError::LlmProvider(format!("request failed: {e}")))
    })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "no body".into());
        let err = OrkError::LlmProvider(format!("{provider_id} API error {status}: {body}"));
        return if status.is_server_error() {
            // 5xx — gateway / model server hiccup; retryable.
            Err(AttemptError::Transient(err))
        } else {
            // 4xx — auth, validation, quota; replaying gains nothing.
            Err(AttemptError::Fatal(err))
        };
    }
    Ok(resp)
}

#[derive(Serialize)]
struct OpenAiStreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OpenAiStreamOptions>,
    /// OpenAI tool catalog. Omitted when empty so we never trip providers
    /// that reject the field on models without function-calling support.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
}

/// OpenAI `tools[]` entry. We only ever emit `type: "function"` — the
/// other types (`code_interpreter`, etc.) are vendor-specific and not part
/// of the ADR 0011 surface.
#[derive(Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionSpec,
}

#[derive(Serialize)]
struct OpenAiFunctionSpec {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct OpenAiMessage {
    role: String,
    /// OpenAI accepts `null` content when `tool_calls` is set; we always
    /// serialise an empty string instead — mirrors the wire shape Minimax
    /// returns and keeps the field non-optional.
    #[serde(default)]
    content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenAiToolCall>,
    /// Required when `role = "tool"`; references the originating tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    kind: String,
    function: OpenAiToolCallFunction,
}

fn default_tool_type() -> String {
    "function".into()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct OpenAiToolCallFunction {
    #[serde(default)]
    name: String,
    /// OpenAI arguments are a JSON-encoded string, NOT a JSON value.
    /// We serialise our [`ToolCall::arguments`] back to a string here and
    /// parse the inverse on the response side.
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    model: String,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Clone)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    model: Option<String>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCallDelta>,
}

/// Per-chunk tool-call delta. `index` slots into the aggregator; everything
/// else may arrive incrementally.
#[derive(Deserialize)]
struct OpenAiToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAiToolCallFunctionDelta>,
}

#[derive(Deserialize)]
struct OpenAiToolCallFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn role_to_wire(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn message_to_wire(msg: &ChatMessage) -> OpenAiMessage {
    OpenAiMessage {
        role: role_to_wire(msg.role).into(),
        content: msg.content.clone(),
        tool_calls: msg
            .tool_calls
            .iter()
            .map(|tc| OpenAiToolCall {
                id: tc.id.clone(),
                kind: "function".into(),
                function: OpenAiToolCallFunction {
                    name: tc.name.clone(),
                    arguments: serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
                },
            })
            .collect(),
        tool_call_id: msg.tool_call_id.clone(),
    }
}

fn tools_to_wire(tools: &[ToolDescriptor]) -> Vec<OpenAiTool> {
    tools
        .iter()
        .map(|t| OpenAiTool {
            kind: "function",
            function: OpenAiFunctionSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

fn tool_choice_to_wire(choice: &Option<ToolChoice>) -> Option<Value> {
    let c = choice.as_ref()?;
    Some(match c {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Named { name } => serde_json::json!({
            "type": "function",
            "function": { "name": name }
        }),
    })
}

fn parse_tool_calls(raw: &[OpenAiToolCall]) -> Vec<ToolCall> {
    raw.iter()
        .map(|c| ToolCall {
            id: c.id.clone(),
            name: c.function.name.clone(),
            arguments: serde_json::from_str(&c.function.arguments)
                .unwrap_or(Value::Object(serde_json::Map::new())),
        })
        .collect()
}

fn map_finish_reason(raw: Option<String>) -> FinishReason {
    match raw.as_deref() {
        Some("stop") | None => FinishReason::Stop,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.into()),
    }
}

fn usage_to_token(u: OpenAiUsage) -> TokenUsage {
    TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    }
}

fn zero_usage() -> TokenUsage {
    TokenUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    }
}

/// Per-streaming-call aggregator for tool calls. OpenAI streams emit one
/// `delta.tool_calls[]` per chunk; we accumulate per `index` slot until the
/// arguments parse as valid JSON (or `finish_reason = tool_calls` lands)
/// and then emit a single [`ChatStreamEvent::ToolCall`].
#[derive(Default)]
struct ToolCallAggregator {
    slots: Vec<ToolCallSlot>,
    /// Slots already flushed to a [`ChatStreamEvent::ToolCall`]. Idempotent —
    /// the late `finish_reason = tool_calls` chunk won't double-emit.
    emitted: Vec<bool>,
}

#[derive(Default)]
struct ToolCallSlot {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAggregator {
    fn ensure_slot(&mut self, index: usize) {
        while self.slots.len() <= index {
            self.slots.push(ToolCallSlot::default());
            self.emitted.push(false);
        }
    }

    /// Apply one delta. Returns `Some(ToolCall)` when the slot's arguments
    /// parse as JSON for the first time.
    fn apply(&mut self, delta: OpenAiToolCallDelta) -> Option<ToolCall> {
        let index = delta.index;
        self.ensure_slot(index);
        let slot = &mut self.slots[index];
        if let Some(id) = delta.id {
            slot.id = id;
        }
        if let Some(func) = delta.function {
            if let Some(name) = func.name {
                slot.name.push_str(&name);
            }
            if let Some(args) = func.arguments {
                slot.arguments.push_str(&args);
            }
        }
        if !self.emitted[index]
            && !slot.id.is_empty()
            && !slot.name.is_empty()
            && !slot.arguments.is_empty()
            && let Ok(parsed) = serde_json::from_str::<Value>(&slot.arguments)
        {
            self.emitted[index] = true;
            return Some(ToolCall {
                id: slot.id.clone(),
                name: slot.name.clone(),
                arguments: parsed,
            });
        }
        None
    }

    /// Flush any slot we never managed to emit (called when the model
    /// signals `finish_reason = tool_calls` so we don't lose calls whose
    /// arguments arrived after the last parse attempt — or were empty
    /// strings the model meant as `{}`).
    fn drain_remaining(&mut self) -> Vec<ToolCall> {
        let mut out = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if self.emitted[index] || slot.id.is_empty() || slot.name.is_empty() {
                continue;
            }
            let arguments: Value = if slot.arguments.trim().is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&slot.arguments)
                    .unwrap_or(Value::Object(serde_json::Map::new()))
            };
            self.emitted[index] = true;
            out.push(ToolCall {
                id: slot.id.clone(),
                name: slot.name.clone(),
                arguments,
            });
        }
        out
    }
}

#[derive(Default)]
struct StreamEvent {
    text_delta: Option<String>,
    tool_call_deltas: Vec<ChatStreamEvent>,
    aggregated_calls: Vec<ChatStreamEvent>,
    finish_reason: Option<String>,
}

fn apply_stream_chunk(
    chunk: OpenAiStreamChunk,
    last_model: &mut Option<String>,
    last_usage: &mut Option<OpenAiUsage>,
    aggregator: &mut ToolCallAggregator,
) -> StreamEvent {
    if let Some(m) = chunk.model {
        *last_model = Some(m);
    }
    if let Some(u) = chunk.usage {
        *last_usage = Some(u);
    }

    let mut event = StreamEvent::default();
    let Some(choice) = chunk.choices.into_iter().next() else {
        return event;
    };

    if let Some(d) = choice.delta.content
        && !d.is_empty()
    {
        event.text_delta = Some(d);
    }

    for delta in choice.delta.tool_calls {
        let index = delta.index;
        let id = delta.id.clone();
        let (name_delta, args_delta) = match delta.function.as_ref() {
            Some(f) => (f.name.clone(), f.arguments.clone()),
            None => (None, None),
        };
        event.tool_call_deltas.push(ChatStreamEvent::ToolCallDelta {
            index,
            id: id.clone(),
            name: name_delta.clone(),
            arguments_delta: args_delta.clone().unwrap_or_default(),
        });
        if let Some(call) = aggregator.apply(delta) {
            event.aggregated_calls.push(ChatStreamEvent::ToolCall(call));
        }
    }

    if let Some(reason) = choice.finish_reason {
        event.finish_reason = Some(reason);
    }
    event
}

fn process_sse_event_lines(
    raw_event: &str,
    last_model: &mut Option<String>,
    last_usage: &mut Option<OpenAiUsage>,
    aggregator: &mut ToolCallAggregator,
    out_events: &mut Vec<ChatStreamEvent>,
    finish_reason: &mut Option<String>,
) -> Result<(), OrkError> {
    for line in raw_event.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let payload = if let Some(rest) = line.strip_prefix("data:") {
            rest.trim_start()
        } else {
            continue;
        };
        if payload == "[DONE]" {
            continue;
        }
        let chunk: OpenAiStreamChunk = serde_json::from_str(payload).map_err(|e| {
            OrkError::LlmProvider(format!(
                "invalid SSE JSON: {e}; payload: {}",
                payload.chars().take(200).collect::<String>()
            ))
        })?;
        let event = apply_stream_chunk(chunk, last_model, last_usage, aggregator);
        if let Some(d) = event.text_delta {
            out_events.push(ChatStreamEvent::Delta(d));
        }
        out_events.extend(event.tool_call_deltas);
        out_events.extend(event.aggregated_calls);
        if let Some(reason) = event.finish_reason {
            *finish_reason = Some(reason);
        }
    }
    Ok(())
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, OrkError> {
        let model = request.model.clone().or_else(|| self.default_model.clone()).ok_or_else(|| {
            OrkError::LlmProvider(format!(
                "no model resolved for provider `{}`: ChatRequest.model is None and the catalog entry has no default_model",
                self.provider_id
            ))
        })?;

        let messages: Vec<OpenAiMessage> = request.messages.iter().map(message_to_wire).collect();

        let body = OpenAiRequest {
            model: model.clone(),
            messages,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stream: None,
            stream_options: None,
            tools: tools_to_wire(&request.tools),
            tool_choice: tool_choice_to_wire(&request.tool_choice),
        };

        debug!(provider = %self.provider_id, model = %model, "sending chat request");

        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body);
        let resp = self
            .apply_headers(req)
            .send()
            .await
            .map_err(|e| OrkError::LlmProvider(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_else(|_| "no body".into());
            return Err(OrkError::LlmProvider(format!(
                "{} API error {status}: {body}",
                self.provider_id
            )));
        }

        let api_resp: OpenAiResponse = resp
            .json()
            .await
            .map_err(|e| OrkError::LlmProvider(format!("failed to parse response: {e}")))?;

        let choice = api_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| OrkError::LlmProvider("no choices in response".into()))?;

        let usage = api_resp
            .usage
            .map(usage_to_token)
            .unwrap_or_else(zero_usage);

        let tool_calls = parse_tool_calls(&choice.message.tool_calls);
        let finish_reason = map_finish_reason(choice.finish_reason);

        Ok(ChatResponse {
            content: choice.message.content,
            model: api_resp.model,
            usage,
            tool_calls,
            finish_reason,
        })
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        let fallback_model = request.model.clone().or_else(|| self.default_model.clone()).ok_or_else(|| {
            OrkError::LlmProvider(format!(
                "no model resolved for provider `{}`: ChatRequest.model is None and the catalog entry has no default_model",
                self.provider_id
            ))
        })?;

        let messages: Vec<OpenAiMessage> = request.messages.iter().map(message_to_wire).collect();

        let body = OpenAiRequest {
            model: fallback_model.clone(),
            messages,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stream: Some(true),
            stream_options: Some(OpenAiStreamOptions {
                include_usage: true,
            }),
            tools: tools_to_wire(&request.tools),
            tool_choice: tool_choice_to_wire(&request.tool_choice),
        };

        debug!(provider = %self.provider_id, model = %fallback_model, "sending streaming chat request");

        // Serialize the body once so the retry path (inside the stream)
        // doesn't have to re-serialize the same `OpenAiRequest`. Cloning
        // the bytes is much cheaper than re-walking serde for every
        // attempt.
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| OrkError::LlmProvider(format!("serialise chat request body: {e}")))?;
        let url = format!("{}/chat/completions", self.base_url);

        // Initial-send retry loop. Up to `STREAM_MAX_ATTEMPTS` total
        // HTTP requests are issued here, but only when the first one
        // fails with a *transient* class (TCP/TLS reset, 5xx, …). 4xx
        // responses surface synchronously on the first attempt because
        // replaying them just burns budget the next call might want
        // for an actually-flapping endpoint. Mirrors the safety check
        // we apply mid-stream below.
        let mut initial_attempts: usize = 0;
        let resp = loop {
            initial_attempts += 1;
            match send_chat_stream_attempt(
                &self.client,
                &url,
                &body_bytes,
                &self.headers,
                &self.provider_id,
            )
            .await
            {
                Ok(r) => break r,
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Transient(e)) => {
                    if initial_attempts >= STREAM_MAX_ATTEMPTS {
                        return Err(e);
                    }
                    warn!(
                        provider = %self.provider_id,
                        attempt = initial_attempts,
                        error = %e,
                        "transient initial-send failure; retrying once before giving up"
                    );
                    tokio::time::sleep(STREAM_RETRY_BACKOFF).await;
                }
            }
        };

        // Pieces the mid-stream retry loop needs to re-issue the
        // request from inside the `async_stream` block.
        let client = self.client.clone();
        let headers = self.headers.clone();
        let provider_id = self.provider_id.clone();
        // Share the attempt budget with the initial-send loop above so
        // the worst case is at most `STREAM_MAX_ATTEMPTS` HTTP requests
        // per `chat_stream` call no matter where the failures cluster.
        let attempts_used_at_stream_start = initial_attempts;

        let stream = async_stream::stream! {
            let mut current_resp = Some(resp);
            let mut attempts: usize = attempts_used_at_stream_start - 1;
            // True once any `Ok` event has been forwarded to the
            // consumer. Past that point a retry would corrupt the
            // observed stream (replayed deltas, duplicated tool calls)
            // so the next stream-read error must propagate as-is.
            let mut yielded_ok = false;

            'attempt: loop {
                attempts += 1;
                let resp = match current_resp.take() {
                    Some(r) => r,
                    None => {
                        // Mid-stream retry: re-issue the request. A 4xx
                        // here is unlikely (the first attempt already
                        // showed the model server is willing to talk
                        // to us) but if it happens, surface as-is —
                        // the consumer would not benefit from a third
                        // attempt anyway.
                        match send_chat_stream_attempt(
                            &client,
                            &url,
                            &body_bytes,
                            &headers,
                            &provider_id,
                        )
                        .await {
                            Ok(r) => r,
                            Err(retry_err) => {
                                yield Err(retry_err.into_inner());
                                return;
                            }
                        }
                    }
                };

                let mut byte_stream = resp.bytes_stream();
                let mut buf = String::new();
                let mut last_model: Option<String> = None;
                let mut last_usage: Option<OpenAiUsage> = None;
                let mut aggregator = ToolCallAggregator::default();
                let mut finish_reason_raw: Option<String> = None;

                while let Some(item) = byte_stream.next().await {
                    let chunk = match item {
                        Ok(b) => b,
                        Err(e) => {
                            // Retry the *whole* HTTP attempt only when:
                            //   1. nothing has reached the consumer yet
                            //      (a replayed request would otherwise
                            //      duplicate already-rendered content),
                            //   2. we still have attempt budget.
                            // Anything else propagates as-is so flapping
                            // mid-stream doesn't get masked behind two
                            // failed attempts.
                            if !yielded_ok && attempts < STREAM_MAX_ATTEMPTS {
                                warn!(
                                    provider = %provider_id,
                                    attempt = attempts,
                                    error = %e,
                                    "transient mid-stream failure before any SSE event; retrying once"
                                );
                                tokio::time::sleep(STREAM_RETRY_BACKOFF).await;
                                continue 'attempt;
                            }
                            yield Err(OrkError::LlmProvider(format!("stream read failed: {e}")));
                            return;
                        }
                    };
                    buf.push_str(&String::from_utf8_lossy(&chunk));

                    while let Some(pos) = buf.find("\n\n") {
                        let raw_event = buf[..pos].to_string();
                        buf = buf[pos + 2..].to_string();

                        let mut events = Vec::new();
                        if let Err(e) = process_sse_event_lines(
                            &raw_event,
                            &mut last_model,
                            &mut last_usage,
                            &mut aggregator,
                            &mut events,
                            &mut finish_reason_raw,
                        ) {
                            yield Err(e);
                            return;
                        }
                        for ev in events {
                            yielded_ok = true;
                            yield Ok(ev);
                        }
                    }
                }

                let tail = buf.trim();
                if !tail.is_empty() {
                    let mut events = Vec::new();
                    if let Err(e) = process_sse_event_lines(
                        tail,
                        &mut last_model,
                        &mut last_usage,
                        &mut aggregator,
                        &mut events,
                        &mut finish_reason_raw,
                    ) {
                        yield Err(e);
                        return;
                    }
                    // No need to update `yielded_ok` here — we have
                    // already exited the byte_stream loop, so the only
                    // consumer of the flag (the retry guard) cannot fire
                    // again on this attempt.
                    for ev in events {
                        yield Ok(ev);
                    }
                }

                let finish_reason = map_finish_reason(finish_reason_raw);
                // Final flush of any straggler tool calls. Defensive — at this
                // point either every slot has emitted or finish_reason is
                // ToolCalls and the model just sent very-late argument bytes.
                if matches!(finish_reason, FinishReason::ToolCalls) {
                    for ev in aggregator.drain_remaining() {
                        yield Ok(ChatStreamEvent::ToolCall(ev));
                    }
                }

                let usage = last_usage
                    .map(usage_to_token)
                    .unwrap_or_else(zero_usage);
                let model = last_model.unwrap_or(fallback_model);
                yield Ok(ChatStreamEvent::Done { usage, model, finish_reason });
                return;
            }
        };

        Ok(Box::pin(stream))
    }

    fn provider_name(&self) -> &str {
        &self.provider_id
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // Walk the catalog entry; the first match wins (operators can list
        // generic-then-specific). Returning the trait-default when there's
        // no declared entry keeps the agent loop from accidentally
        // disabling tool-calling for models we just haven't catalogued
        // yet.
        for entry in &self.capabilities {
            if entry.model == model {
                return ModelCapabilities {
                    supports_tools: entry.supports_tools,
                    supports_streaming: entry.supports_streaming,
                    supports_vision: entry.supports_vision,
                    max_context: entry.max_context.unwrap_or(0),
                };
            }
        }
        ModelCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aggregates_streaming_tool_call_across_chunks() {
        let mut agg = ToolCallAggregator::default();
        let r1 = agg.apply(OpenAiToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            function: Some(OpenAiToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some("{\"city\":".into()),
            }),
        });
        assert!(r1.is_none(), "incomplete JSON should not emit yet");
        let r2 = agg.apply(OpenAiToolCallDelta {
            index: 0,
            id: None,
            function: Some(OpenAiToolCallFunctionDelta {
                name: None,
                arguments: Some("\"sf\"}".into()),
            }),
        });
        let call = r2.expect("complete JSON should emit");
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments, json!({"city":"sf"}));
    }

    #[test]
    fn drains_unparsed_slots_when_finish_reason_is_tool_calls() {
        let mut agg = ToolCallAggregator::default();
        agg.apply(OpenAiToolCallDelta {
            index: 0,
            id: Some("call_a".into()),
            function: Some(OpenAiToolCallFunctionDelta {
                name: Some("noop".into()),
                arguments: None,
            }),
        });
        let calls = agg.drain_remaining();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "noop");
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn finish_reason_strings_map_correctly() {
        assert!(matches!(map_finish_reason(None), FinishReason::Stop));
        assert!(matches!(
            map_finish_reason(Some("stop".into())),
            FinishReason::Stop
        ));
        assert!(matches!(
            map_finish_reason(Some("tool_calls".into())),
            FinishReason::ToolCalls
        ));
        assert!(matches!(
            map_finish_reason(Some("length".into())),
            FinishReason::Length
        ));
        assert!(matches!(
            map_finish_reason(Some("content_filter".into())),
            FinishReason::ContentFilter
        ));
        match map_finish_reason(Some("ai_safety".into())) {
            FinishReason::Other(s) => assert_eq!(s, "ai_safety"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn tool_choice_serialises_to_openai_shape() {
        assert_eq!(
            tool_choice_to_wire(&Some(ToolChoice::Auto)),
            Some(Value::String("auto".into()))
        );
        assert_eq!(
            tool_choice_to_wire(&Some(ToolChoice::None)),
            Some(Value::String("none".into()))
        );
        assert_eq!(
            tool_choice_to_wire(&Some(ToolChoice::Required)),
            Some(Value::String("required".into()))
        );
        assert_eq!(
            tool_choice_to_wire(&Some(ToolChoice::Named { name: "foo".into() })),
            Some(json!({"type":"function","function":{"name":"foo"}}))
        );
        assert_eq!(tool_choice_to_wire(&None), None);
    }

    #[test]
    fn tool_messages_carry_tool_call_id_on_wire() {
        let m = ChatMessage::tool("call_42", "{\"ok\":true}");
        let wire = message_to_wire(&m);
        assert_eq!(wire.role, "tool");
        assert_eq!(wire.tool_call_id.as_deref(), Some("call_42"));
        assert_eq!(wire.content, "{\"ok\":true}");
    }

    #[test]
    fn assistant_tool_calls_serialise_arguments_as_string() {
        let m = ChatMessage::assistant(
            "",
            vec![ToolCall {
                id: "call_x".into(),
                name: "echo".into(),
                arguments: json!({"hello":"world"}),
            }],
        );
        let wire = message_to_wire(&m);
        assert_eq!(wire.tool_calls.len(), 1);
        let tc = &wire.tool_calls[0];
        assert_eq!(tc.id, "call_x");
        assert_eq!(tc.function.name, "echo");
        assert_eq!(tc.function.arguments, "{\"hello\":\"world\"}");
    }

    #[test]
    fn capabilities_lookup_uses_catalog_then_falls_through() {
        let p = OpenAiCompatibleProvider::new(
            "openai",
            "https://example.com/v1",
            None,
            HashMap::new(),
            vec![ModelCapabilitiesEntry {
                model: "gpt-4o-mini".into(),
                supports_tools: true,
                supports_streaming: true,
                supports_vision: false,
                max_context: Some(128_000),
            }],
        );
        let caps = p.capabilities("gpt-4o-mini");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context, 128_000);
        // Unknown models fall through to the trait default.
        let unknown = p.capabilities("not-listed");
        assert_eq!(unknown.max_context, 0);
    }
}
