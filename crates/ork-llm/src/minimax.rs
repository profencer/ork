//! OpenAI-compatible chat completions provider — used as the reference impl
//! for ADR 0011 native tool-calling on the OpenAI wire shape.
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
//!
//! Other OpenAI-compatible providers (per ADR 0012) can reuse the
//! aggregator + serde shapes here once they're factored into a shared module
//! — out of scope for this PR series.

use async_trait::async_trait;
use futures::StreamExt;
use ork_common::error::OrkError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

use ork_core::ports::llm::{
    ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream,
    LlmProvider, MessageRole, TokenUsage, ToolCall, ToolChoice, ToolDescriptor,
};

pub struct MinimaxProvider {
    client: Client,
    base_url: String,
    api_key: String,
    default_model: String,
}

impl MinimaxProvider {
    pub fn new(api_key: String, base_url: Option<String>, model: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.unwrap_or_else(|| "https://api.minimax.io/v1".into()),
            api_key,
            default_model: model.unwrap_or_else(|| "MiniMax-M2.5".into()),
        }
    }
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
impl LlmProvider for MinimaxProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, OrkError> {
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

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

        debug!(model = %model, "sending chat request to Minimax");

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| OrkError::LlmProvider(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_else(|_| "no body".into());
            return Err(OrkError::LlmProvider(format!(
                "Minimax API error {status}: {body}"
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
        let fallback_model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

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

        debug!(model = %fallback_model, "sending streaming chat request to Minimax");

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| OrkError::LlmProvider(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_else(|_| "no body".into());
            return Err(OrkError::LlmProvider(format!(
                "Minimax API error {status}: {body}"
            )));
        }

        let mut byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut buf = String::new();
            let mut last_model: Option<String> = None;
            let mut last_usage: Option<OpenAiUsage> = None;
            let mut aggregator = ToolCallAggregator::default();
            let mut finish_reason_raw: Option<String> = None;

            while let Some(item) = byte_stream.next().await {
                let chunk = match item {
                    Ok(b) => b,
                    Err(e) => {
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
        };

        Ok(Box::pin(stream))
    }

    fn provider_name(&self) -> &str {
        "minimax"
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
}
