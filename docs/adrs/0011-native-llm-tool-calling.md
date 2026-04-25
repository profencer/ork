# 0011 — Native LLM tool-calling

- **Status:** Accepted
- **Date:** 2026-04-24
- **Phase:** 3
- **Relates to:** 0002, 0006, 0010, 0012, 0015, 0016

## Context

ork's current "tool calling" is degenerate: the engine runs each step's `tools:` list **before** the LLM is invoked, then **appends the JSON output to the user prompt** ([`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)). The model never decides which tool to call or with what arguments — it's pre-computed by the workflow author.

This pattern is incompatible with the parity goal in two important ways:

- **No agent-driven delegation.** The `agent_call` tool from ADR [`0006`](0006-peer-delegation.md) needs the LLM to pick a target peer at runtime — impossible if tools are pre-executed.
- **No MCP tool surface.** ADR [`0010`](0010-mcp-tool-plane.md) brings hundreds of MCP tools into ork; surfacing them all as pre-execute steps is absurd. The LLM must choose dynamically.

Modern LLM APIs (OpenAI, Anthropic, Bedrock, OpenAI-compatible like Minimax) all expose **native tool calling**: the request includes a tool catalog, the response indicates `tool_calls`, the client executes them, and the assistant message is followed by `role: tool` messages with the results.

[`LlmProvider`](../../crates/ork-core/src/ports/llm.rs) currently has no notion of tool calls at all — `ChatMessage.role` is only `System | User | Assistant`, and `ChatResponse.content` is a single string.

## Decision

ork **adopts native LLM tool calling** by extending the [`LlmProvider`](../../crates/ork-core/src/ports/llm.rs) port and replacing the engine's pre-execute-and-append pattern with an agent-driven loop inside `LocalAgent` (ADR [`0002`](0002-agent-port.md)).

### `LlmProvider` extensions

```rust
// crates/ork-core/src/ports/llm.rs

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,           // NEW: assistant-emitted calls
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,        // NEW: required when role == Tool
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,                    // NEW: A2A multimodal parts (ADR 0003)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,                                    // NEW
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,        // already-parsed JSON, not a string
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub model: Option<String>,
    pub tools: Vec<ToolDescriptor>,          // NEW
    pub tool_choice: Option<ToolChoice>,     // NEW: auto | none | required | { name: ... }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,       // JSON Schema for the tool's input
}

#[derive(Clone, Debug)]
pub enum ChatStreamEvent {
    Delta(String),                           // text delta
    ToolCall(ToolCall),                      // NEW: complete tool call (post-aggregation)
    ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, arguments_delta: String }, // NEW
    Done { usage: TokenUsage, model: String, finish_reason: FinishReason },
}

pub enum FinishReason { Stop, ToolCalls, Length, ContentFilter, Other(String) }
```

`provider_name` and `chat`/`chat_stream` signatures stay the same; existing call sites recompile because `tools` and `tool_choice` are added as optional defaults.

The streaming variant emits both `ToolCallDelta` (for clients that want incremental display) and `ToolCall` (aggregated, emitted before `Done`). `MinimaxProvider` ([`crates/ork-llm/src/minimax.rs`](../../crates/ork-llm/src/minimax.rs)) is the OpenAI-compatible reference impl; it parses the `delta.tool_calls` array per OpenAI streaming convention.

### `LocalAgent` tool loop

The pre-execute-and-append loop in [`engine.rs`](../../crates/ork-core/src/workflow/engine.rs) is removed. `LocalAgent::send_stream` (ADR [`0002`](0002-agent-port.md)) implements the standard agentic loop:

```rust
async fn send_stream(&self, ctx: AgentContext, msg: AgentMessage)
    -> Result<BoxStream<'static, Result<AgentEvent, OrkError>>, OrkError>
{
    let tools = self.tool_descriptors_for_agent(&ctx).await?;  // builtin + per-agent allow-list + MCP
    let (tx, rx) = mpsc::channel(64);

    tokio::spawn(async move {
        let mut history = self.build_initial_history(&ctx, &msg);
        loop {
            if ctx.cancel.is_cancelled() { break; }
            let req = ChatRequest {
                messages: history.clone(),
                tools: tools.clone(),
                tool_choice: Some(ToolChoice::Auto),
                temperature: Some(self.config.temperature),
                max_tokens: Some(self.config.max_tokens),
                model: self.config.model.clone(),
            };

            let mut stream = self.llm.chat_stream(req).await?;
            let mut text_buf = String::new();
            let mut tool_calls = Vec::new();
            while let Some(ev) = stream.next().await {
                match ev? {
                    ChatStreamEvent::Delta(d) => {
                        text_buf.push_str(&d);
                        tx.send(Ok(AgentEvent::status_text(&d))).await.ok();
                    }
                    ChatStreamEvent::ToolCall(tc) => tool_calls.push(tc),
                    ChatStreamEvent::ToolCallDelta { .. } => { /* optional surface */ }
                    ChatStreamEvent::Done { usage, model, finish_reason } => {
                        history.push(ChatMessage::assistant(text_buf.clone(), tool_calls.clone()));
                        match finish_reason {
                            FinishReason::ToolCalls => {
                                for call in &tool_calls {
                                    let result = self.tools
                                        .execute(ctx.tenant_id, &call.name, &call.arguments).await;
                                    tx.send(Ok(AgentEvent::tool_result(call, &result))).await.ok();
                                    history.push(ChatMessage::tool(call.id.clone(), &result));
                                }
                            }
                            _ => {
                                tx.send(Ok(AgentEvent::final_message(text_buf, usage, model))).await.ok();
                                return Ok::<_, OrkError>(());
                            }
                        }
                    }
                }
            }
        }
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
}
```

A hard cap `max_tool_iterations` (default 16) prevents runaway loops. On exceeding, the loop returns a final `AgentEvent::Failed` with reason `tool_loop_exceeded`.

### Tool descriptor source

`tool_descriptors_for_agent` composes from:

1. **Builtins**: `agent_call` (ADR [`0006`](0006-peer-delegation.md)) plus per-discovered-peer expansion (`peer_<agent>_<skill>`); `artifact_*` tools (ADR [`0016`](0016-artifact-storage.md)); `code_*` tools when a workspace is configured ([`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)).
2. **Integration tools** matching the agent's per-step `tools:` list ([`WorkflowStep.tools`](../../crates/ork-core/src/models/workflow.rs)).
3. **MCP tools** from `McpClient::list_tools_for_tenant(tenant_id)` (ADR [`0010`](0010-mcp-tool-plane.md)) filtered by the agent's allow-list (glob like `mcp:atlassian.*`).

The resulting `ToolDescriptor.parameters` is the JSON Schema that the upstream provider/MCP server publishes — we don't transform it, just pass through.

### `WorkflowStep.tools` semantics change

Today `tools:` lists the tools that get pre-executed. After this ADR, `tools:` is an **allow-list**: the LLM may only call tools whose name (or glob) matches an entry. An empty list means "builtins only". Existing workflow YAMLs in [`workflow-templates/`](../../workflow-templates/) keep working with one caveat: tools that used to be force-executed now require the LLM to invoke them. We add a one-time migration helper `ork workflow migrate-tools` (CLI) that auto-prepends a "Use the following tools as needed: …" hint to existing prompt templates.

### Per-agent model selection

`AgentConfig.model: Option<String>` (added by ADR [`0012`](0012-multi-llm-providers.md)) flows through to `ChatRequest.model`, letting different agents in the same workflow use different models.

## Consequences

### Positive

- LLM-driven delegation (ADR [`0006`](0006-peer-delegation.md)) is now possible at all.
- MCP tools are first-class — the LLM picks them with no engine intervention.
- Streaming the assistant's text + tool-call deltas to clients matches A2A's `message/stream` semantics.
- We can finally support multimodal tool inputs/outputs (image generation, file analysis) once `Part` is wired through history (already in the message struct).

### Negative / costs

- `LlmProvider` impls grow; each provider needs to know its tool-call wire format. This ADR's reference impl is OpenAI-compatible (Minimax inherits it); ADR [`0012`](0012-multi-llm-providers.md) covers Anthropic and others.
- Existing workflows behave subtly differently: tool side-effects only fire if the LLM asks. The migration helper plus a release-note callout mitigate this.
- Tool-loop cost ceiling is now per-tenant — heavy users can run up bills. ADR [`0021`](0021-rbac-scopes.md) introduces budget scopes; ADR [`0022`](0022-observability.md) tracks the metric.

### Neutral / follow-ups

- ADR [`0015`](0015-dynamic-embeds.md)'s embeds may be resolved either before LLM call (early phase) or as a post-processing pass on tool results (late phase).
- Token-streaming SSE uses the same `AgentEvent` channel that A2A `message/stream` consumes.
- A future ADR may add structured-output mode (JSON Schema on the assistant's final answer) for non-tool flows.

## Alternatives considered

- **Stay with pre-execute and append.** Rejected: blocks delegation and MCP integration entirely.
- **Use a third-party tool-loop crate (e.g. `langchain-rs`).** Rejected: heavier than what we need, ties us to a vendor abstraction that doesn't quite match A2A semantics.
- **Force every tool to be MCP.** Rejected: builtin tools (`agent_call`, `artifact_*`) are part of ork's identity and shouldn't be MCP-shaped.
- **Implement tool-calling only for the OpenAI-compatible provider, not abstract it.** Rejected: ADR [`0012`](0012-multi-llm-providers.md) demands provider plurality.

## Affected ork modules

- [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs) — extend types as above.
- [`crates/ork-llm/src/minimax.rs`](../../crates/ork-llm/src/minimax.rs) — implement tool-call serialization (OpenAI-compatible).
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) — remove pre-execute-and-append loop; engine just calls `Agent::send_stream` (ADR [`0002`](0002-agent-port.md)).
- New: `crates/ork-agents/src/local_agent.rs` — the `LocalAgent` impl with the loop above.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) — add `ork workflow migrate-tools`.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| ADK tool execution loop | Google ADK runtime, used by [`agent/sac/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/sac/component.py) | `LocalAgent::send_stream` loop |
| Tool descriptor list (per-agent) | YAML `tools:` blocks in [`templates/agent_template.yaml`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/templates/agent_template.yaml) | `tool_descriptors_for_agent` composition |
| MCP tool injection | ADK `MCPToolset` | `McpClient::list_tools_for_tenant` (ADR [`0010`](0010-mcp-tool-plane.md)) |
| Stream of tool-call events | SAM SSE event types | `AgentEvent::tool_result` plus `ChatStreamEvent::ToolCall` |

## Open questions

- Parallel tool-call dispatch within a single LLM turn — providers like OpenAI return multiple `tool_calls` in one response. Decision: execute them concurrently with `try_join_all`, bounded by `max_parallel_tool_calls` (default 4).
- Should we expose the assistant's intermediate "thinking" (e.g. Anthropic's reasoning tokens) on the SSE? Decision: yes, gated behind agent config `expose_reasoning`, off by default.
- Tool-result truncation policy when a result is huge? Initially: truncate to `max_tool_result_bytes` (default 64KB) and store the full result as an artifact (ADR [`0016`](0016-artifact-storage.md)).

### Resolved (implementation)

- `max_tool_iterations = 16`
- `max_parallel_tool_calls = 4`
- `max_tool_result_bytes = 65536`
- `expose_reasoning = false`

## References

- OpenAI tool calling: <https://platform.openai.com/docs/guides/function-calling>
- Anthropic tool use: <https://docs.anthropic.com/claude/docs/tool-use>
- A2A message/streaming: <https://github.com/google/a2a>
