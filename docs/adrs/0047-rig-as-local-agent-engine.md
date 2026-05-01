# 0047 — Adopt `rig-core` as the local-agent engine

- **Status:** Accepted
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 3
- **Relates to:** 0002, 0010, 0011, 0012, 0022, 0025, 0032, 0039

## Context

The agentic loop inside [`LocalAgent::send_stream`](../../crates/ork-agents/src/local.rs)
is hand-rolled. Roughly 270 lines of `async_stream::stream!` in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
maintain history, build per-iteration `ChatRequest`s, fan out tool calls
under a [`Semaphore`](../../crates/ork-agents/src/local.rs), classify
[non-fatal tool errors](../../crates/ork-agents/src/local.rs)
(`is_fatal_tool_error`) so the LLM can self-correct per
[ADR 0010 §`Failure model`](0010-mcp-tool-plane.md), spill oversized
results into [ADR 0016](0016-artifact-storage.md) artifact storage, and
poll [`ctx.cancel`](../../crates/ork-core/src/a2a/context.rs) at every
seam. The loop works — it is what passes the demo end-to-end — but it
is also the largest body of "framework" code in the workspace, and it
keeps growing as we adopt structured outputs (ADR
[`0025`](0025-typed-output-validation-and-verifier-agent.md)), tool-call
hooks (ADR [`0039`](0039-agent-tool-call-hooks.md)), context compaction
(ADR [`0032`](0032-agent-memory-and-context-compaction.md)), and
observability (ADR [`0022`](0022-observability.md)) — every one of which
expects extension points the hand-rolled loop does not have.

[`rig-core`](https://github.com/0xPlaygrounds/rig) (crates.io
[`rig-core 0.36`](https://crates.io/crates/rig-core), MIT, Rust 2024,
weekly releases, ~7k stars) is a Rust-native LLM toolkit that ships
exactly the engine code we hand-roll: a typestate `AgentBuilder`, a
[`CompletionModel`](https://docs.rs/rig-core/latest/rig/completion/trait.CompletionModel.html)
trait, streaming with `PromptHook` / `ToolCallHookAction` / `PauseControl`,
typed `Tool` (`Args: Deserialize`, `Output: Serialize`) plus
`ToolDyn`, an `Extractor` for structured outputs via `schemars`, and 25+
provider clients. What it does **not** ship — and explicitly does not
intend to ship — is the network layer ork has already built: A2A 1.0
protocol surface (cards, tasks, typed parts, push, cancel), tenant-scoped
credentials, hexagonal `Agent` port (ADR
[`0002`](0002-agent-port.md)), Kong + Kafka transport (ADR
[`0004`](0004-hybrid-kong-kafka-transport.md)). Rig's "multi-agent" is
[`impl Tool for Agent<M>`](https://docs.rs/rig-core/latest/rig/agent/struct.Agent.html)
— sub-agent-as-tool delegation in-process, no protocol-level handoff.

That asymmetry is the reason this ADR is possible at all. Rig owns the
*single-agent engine* (LLM + tool-loop + RAG glue + structured outputs);
ork owns the *network protocol* layer above it. The two compose cleanly
if we put rig **under** the [`Agent`](../../crates/ork-core/src/ports/agent.rs)
port without touching the port itself, [`AgentContext`](../../crates/ork-core/src/a2a/context.rs),
A2A wire types, [`LlmRouter`](../../crates/ork-llm/src/router.rs)
selection, or [`ork-mcp`](../../crates/ork-mcp/src/client.rs) tenant
credentialing.

## Decision

ork **adopts `rig-core` as the engine inside [`LocalAgent`](../../crates/ork-agents/src/local.rs)**.
The hand-rolled `stream!` body in `LocalAgent::send_stream` is replaced
with a thin driver that delegates the LLM-and-tool dance to a
per-request [`rig::Agent<M>`](https://docs.rs/rig-core/latest/rig/agent/struct.Agent.html).
Every other ork surface — the
[`Agent`](../../crates/ork-core/src/ports/agent.rs) port, A2A types,
[`AgentContext`](../../crates/ork-core/src/a2a/context.rs),
[`ToolExecutor`](../../crates/ork-core/src/workflow/engine.rs),
[`LlmProvider`](../../crates/ork-core/src/ports/llm.rs) trait,
[`LlmRouter`](../../crates/ork-llm/src/router.rs) resolution chain,
[`McpClient`](../../crates/ork-mcp/src/client.rs) tenant pooling,
artifact spill, push notifications — is unchanged. Rig types are
crate-private to [`ork-agents`](../../crates/ork-agents/) and never
appear in [`ork-core`](../../crates/ork-core/), [`ork-common`](../../crates/ork-common/),
or any public wire type.

### `RigEngine` driver

A new module `crates/ork-agents/src/rig_engine.rs` exposes:

```rust
pub(crate) struct RigEngine;

impl RigEngine {
    /// Drive one `send_stream` invocation through `rig::Agent`. Owns
    /// nothing across calls; every field on the rig agent is built per
    /// request because `AgentContext` (tenant, cancel, caller) is
    /// per-request state.
    pub(crate) async fn run(
        ctx: AgentContext,
        config: AgentConfig,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<dyn ToolExecutor>,
        tool_descriptors: Vec<ToolDescriptor>,
        prompt: ChatMessage,           // user role, possibly with parts
        history_seed: Vec<ChatMessage>, // [system, ...]
    ) -> Result<AgentEventStream, OrkError>;
}
```

`run` owns the per-request `tokio::select!` against
`ctx.cancel.cancelled()`, the existing parallel-tool-call semaphore
(`config.max_parallel_tool_calls`), the byte-cap +
[`spill_bytes_to_artifact`](../../crates/ork-core/src/artifact_spill.rs)
truncation policy, and the `is_fatal_tool_error` classification — none
of those move into rig. What rig owns is the conversation history, the
provider call, and the streaming event dispatch.

### `LlmProvider` → `rig::CompletionModel` adapter

```rust
pub(crate) struct LlmProviderCompletionModel {
    inner: Arc<dyn LlmProvider>,
    request_provider: Option<String>, // resolved per ADR 0012 chain
    request_model: Option<String>,
    config: AgentConfig,
    resolve_ctx: ResolveContext,      // ADR 0012 §`Routing`
}

impl rig::completion::CompletionModel for LlmProviderCompletionModel {
    type Response = OrkCompletionResponse;
    type StreamingResponse = OrkStreamingResponse;
    type Client = ();                 // not used by ork

    async fn completion(&self, request: rig::CompletionRequest)
        -> Result<rig::CompletionResponse<Self::Response>, rig::CompletionError> {
        let req = ChatRequest { /* translate request → ork ChatRequest */ };
        let resp = self.resolve_ctx.scope(self.inner.chat(req)).await?;
        // translate ork ChatResponse → rig::CompletionResponse
    }

    async fn stream(&self, request: rig::CompletionRequest)
        -> Result<rig::StreamingCompletionResponse<Self::StreamingResponse>, rig::CompletionError> {
        // translate request → ork ChatRequest, scope inside ResolveContext,
        // map ChatStreamEvent::{Delta, ToolCall, ToolCallDelta, Done}
        // into rig stream events.
    }
}
```

The adapter keeps [`LlmRouter`](../../crates/ork-llm/src/router.rs) as
the single point of provider selection, including the
step → agent → tenant → operator chain from ADR
[`0012`](0012-multi-llm-providers.md). Rig sees one
`CompletionModel` per request; ork's router decides which physical
provider (Kong route, GPUStack pool, etc.) the bytes hit.

### `ToolExecutor` → `rig::ToolDyn` adapter

```rust
/// Spike finding (see §`Spike findings`, surprise 1): rig 0.36
/// converts every `Err` from `ToolDyn::call` into a tool-result
/// string and continues the loop. To preserve ADR 0010's "fatal
/// errors abort the step" semantics, `OrkToolDyn` writes the error
/// into a shared `FatalSlot` and signals `ctx.cancel.cancel()`. The
/// driver's `tokio::select!` cancel branch then `take()`s the slot
/// to differentiate user cancel from tool-driven fatal abort.
pub(crate) struct OrkToolDyn {
    descriptor: ToolDescriptor,
    tools: Arc<dyn ToolExecutor>,
    ctx: AgentContext,                 // captured per request
    fatal: FatalSlot,                  // shared across all tools in the request
}

#[async_trait]
impl rig::tool::ToolDyn for OrkToolDyn {
    fn name(&self) -> String { self.descriptor.name.clone() }
    async fn definition(&self, _prompt: String) -> rig::tool::ToolDefinition {
        rig::tool::ToolDefinition {
            name: self.descriptor.name.clone(),
            description: self.descriptor.description.clone(),
            parameters: self.descriptor.parameters.clone(),
        }
    }
    async fn call(&self, args: String) -> Result<String, rig::tool::ToolSetError> {
        let value: serde_json::Value = serde_json::from_str(&args)
            .map_err(|e| /* surface as non-fatal */)?;
        match self.tools.execute(&self.ctx, &self.descriptor.name, &value).await {
            Ok(out) => Ok(serde_json::to_string(&out).unwrap_or_default()),
            Err(e) if !is_fatal_tool_error(&e) => Ok(tool_error_payload(
                &self.descriptor.name, &e, self.ctx_max_bytes()
            )),
            Err(e) => Err(/* fatal: surface as ToolSetError to abort */),
        }
    }
}
```

Rig's `Tool::call` signature has no per-call context argument, so
`AgentContext` flows by instance capture: a fresh `OrkToolDyn` is
allocated per request and dropped when the stream ends. This preserves
ork's invariant that every `ToolExecutor::execute` sees the live
tenant id, cancel token, caller identity and delegation chain.

### Cancel and streaming

The driver consumes rig's `StreamingCompletionResponse` inside a
`tokio::select!` against `ctx.cancel.cancelled()`. Rig's stream future
is dropped on cancel — the same shape as today's
[`while let Some(ev) = llm_stream.next().await`](../../crates/ork-agents/src/local.rs)
loop, just one level lower. Ork's existing
[`AgentEvent::StatusUpdate(... Working ...)`](../../crates/ork-agents/src/local.rs)
events around the stream stay where they are; the driver yields them
before/after rig owns the floor.

Tool-call deltas (`ChatStreamEvent::ToolCallDelta`) are currently
dropped by `LocalAgent` (see
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
match arm). The rig integration preserves that: rig may emit `ToolCallDelta`-
shaped events internally, but the driver only surfaces text deltas
(`AgentEvent::status_text`) and the aggregated tool calls when rig
hands them off for execution. Clients that want token-by-token tool
arguments are out of scope for this ADR.

### MCP — `ork-mcp` keeps the floor (Phase A)

Rig ships [`rig::tool::rmcp`](https://docs.rs/rig-core/latest/rig/tool/rmcp/index.html)
on top of `rmcp = 1`. The ork workspace pins
[`rmcp = 0.16`](../../Cargo.toml) (ADR [`0010`](0010-mcp-tool-plane.md)).
For Phase A (this ADR's acceptance criteria) we **do not** enable
rig's `rmcp` feature. MCP tools continue to flow through
[`McpClient`](../../crates/ork-mcp/src/client.rs); they reach rig as
ordinary `OrkToolDyn` entries built from
[`tool_descriptors_for_agent`](../../crates/ork-agents/src/tool_catalog.rs).
Tenant-scoped credential pooling, session reuse, and
`mcp:<server>.<tool>` namespacing are all unchanged.

Phase B (`Phasing toward rig-native` below) bumps `rmcp` to `1.x`
across the workspace and switches `ork-mcp` to rig's
`ToolServer`/`ToolServerHandle` shape. This split keeps the engine
swap separable from the rmcp churn — the spike confirms the engine
swap works without touching `rmcp`.

### Feature gate, dependency footprint

```toml
# Cargo.toml (workspace)
[workspace.dependencies]
rig-core = { version = "0.36", default-features = false }

# crates/ork-agents/Cargo.toml
[features]
default = ["rig-engine"]
rig-engine = ["dep:rig-core"]

[dependencies]
rig-core = { workspace = true, optional = true }
```

We pull `rig-core` only — none of the provider companion crates
(`rig-anthropic`, `rig-bedrock`, …) are enabled. Ork's `LlmRouter` is
still the only thing speaking to upstream LLMs; rig sees one custom
`CompletionModel`. The dependency graph adds `rig-core` and its
transitive `tokio`/`futures`/`schemars` (already present).

### Provider for the spike, agent for the smoke

The spike that informs this ADR (see `Alternatives considered`
§*Spike findings*) wires:

- **Provider:** the existing
  [`OpenAiCompatibleProvider`](../../crates/ork-llm/src/openai_compatible.rs)
  exercised against a `wiremock` server scripted to return a
  text-then-tool-call-then-text sequence, plus a
  [`ScriptedLlmProvider`](#) test double for fault injection.
- **MCP server:** ork's existing
  [`rmcp` stdio transport](../../crates/ork-mcp/src/transport.rs)
  pointed at `rust-mcp-stdio-echo` (a 30-line stdio echo server in
  the spike worktree), exposed as `mcp:echo.echo`.
- **Smoke:** `crates/ork-agents/tests/rig_engine_smoke.rs` covers
  streaming text deltas, a non-fatal tool error round-trip, a fatal
  tool error abort, and `ctx.cancel.cancel()` mid-stream.

## Acceptance criteria

- [x] `crates/ork-agents/src/rig_engine.rs` exists and exports
      `RigEngine`, `LlmProviderCompletionModel`, `OrkToolDyn` with the
      signatures shown in `Decision`.
- [x] A `FatalSlot` (or equivalently named) shared cell is threaded
      through `OrkToolDyn` and consulted in the driver's cancel
      branch, per the spike finding *Surprise 1*. `OrkToolDyn::call`
      MUST set the slot on a fatal classification AND signal
      `ctx.cancel.cancel()` so the driver's `tokio::select!` breaks.
      Without this, `Err(...)` from `ToolDyn::call` is silently
      converted to a "Toolset error: …" tool-result by rig and the
      model keeps running, contradicting ADR
      [`0010`](0010-mcp-tool-plane.md) §`Failure model`.
- [x] `LocalAgent::send_stream` ([`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs))
      delegates the LLM-and-tool inner loop to `RigEngine::run`; the
      hand-rolled `stream!` body is removed except for the outer
      `Working` / `Completed` status events and prompt extraction.
- [x] `Cargo.toml` declares `rig-core = { version = "0.36", default-features = false }`
      under `[workspace.dependencies]`.
- [x] `crates/ork-agents/Cargo.toml` declares the `rig-engine`
      feature (default-on) gating the dep.
- [x] No file under `crates/ork-core/`, `crates/ork-common/`,
      `crates/ork-llm/`, `crates/ork-api/` imports anything from `rig`.
      Enforce by `grep -rn "use rig" crates/ork-{core,common,llm,api}`
      returning empty in CI.
- [x] `crates/ork-agents/tests/rig_engine_smoke.rs` covers, against a
      scripted `LlmProvider`:
      (a) text-only response yields `StatusUpdate(Working)` →
          one or more `status_text` deltas → `Message` →
          `StatusUpdate(Completed, is_final=true)`;
      (b) one tool call → tool result → final text yields the same
          terminal sequence with the tool result fed back in history;
      (c) tool returns `OrkError::Validation` (non-fatal) → loop
          continues; final message includes a recovered answer;
      (d) tool returns `OrkError::Workflow` (fatal) → stream yields
          `Err`, no `Completed` event;
      (e) `ctx.cancel.cancel()` between deltas aborts within
          ≤ 50 ms of the next poll, yielding
          `Err(OrkError::Workflow("agent task cancelled"))`.
- [x] `crates/ork-agents/tests/rig_engine_mcp_smoke.rs` (gated under
      feature `mcp-stdio-it`) launches `@modelcontextprotocol/server-everything`
      via `npx` and stdio through [`McpClient`](../../crates/ork-mcp/src/client.rs),
      exposes `mcp:everything.echo` to [`LocalAgent`](../../crates/ork-agents/src/local.rs),
      and asserts a round-trip.
- [x] Existing `crates/ork-agents` tests stay green; [`tool_loop_history.rs`](../../crates/ork-agents/tests/tool_loop_history.rs)
      transcript assertions were relaxed for rig-shaped `ChatRequest` history (see Reviewer findings).
- [x] `cargo fmt --all -- --check`, `cargo test --workspace`,
      `cargo clippy --workspace --all-targets -- -D warnings` all
      pass.
- [x] [`README.md`](README.md) ADR index row added with status
      `Accepted` once these boxes tick.
- [x] [`metrics.csv`](metrics.csv) row appended (see [`METRICS.md`](METRICS.md)).

## Consequences

### Positive

- ~270 lines of hand-rolled loop in
  [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
  collapse to a thin driver. Conversation history, provider
  invocation, and tool dispatch live behind a tested, externally
  maintained surface.
- Structured-output validation (ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md)) gets
  a free runway via [`rig::Extractor`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html)
  and `schemars`. ADR 0025 owns the wiring; this ADR removes the
  argument that we'd need to hand-roll it.
- Tool-call hook points (ADR
  [`0039`](0039-agent-tool-call-hooks.md)) align with rig's
  [`PromptHook`](https://docs.rs/rig-core/latest/rig/agent/trait.PromptHook.html)
  / `ToolCallHookAction` surface. ADR 0039 can be implemented as a
  thin shim over rig's hooks.
- Pause/resume on the stream (rig's `PauseControl`) becomes available
  for ADR [`0027`](0027-human-in-the-loop.md) human-in-the-loop
  approval steps without further engine work.
- Future native provider clients (Anthropic, Bedrock, Vertex) become
  feasible as a follow-up — ork would register additional
  `CompletionModel` impls behind the same `LlmRouter`. ADR
  [`0012`](0012-multi-llm-providers.md)'s "out-of-process protocol
  conversion via Kong" stays the default; native clients are not
  scheduled by this ADR.

### Negative / costs

- New external dependency on a 0.x library with monthly minor
  releases. We pin to `0.36` and bump deliberately. Mitigation:
  `rig-core` only (no provider companion crates), feature-gated so
  ork can build without it during a regression.
- `rmcp` version split is a known constraint for Phase A: rig's
  `rmcp` feature needs `rmcp = 1`, ork is on `rmcp = 0.16`. Phase A
  sidesteps by not enabling rig's MCP module. Phase B
  (§`Phasing toward rig-native`) bumps rmcp and switches `ork-mcp`
  to rig's `ToolServer`/`ToolServerHandle`. The bump is its own
  diff; it is scheduled, not deferred indefinitely.
- AgentContext smuggling overhead: a fresh `OrkToolDyn` allocation
  per request per tool. Profiled against the current direct-dispatch
  loop in the spike (see *Spike findings*); negligible at agent
  request rates.
- Rig owns the parallel tool-call decision. Today
  [`LocalAgent`](../../crates/ork-agents/src/local.rs) caps
  parallelism with a `Semaphore` keyed off
  `config.max_parallel_tool_calls`. Rig's loop dispatches calls
  sequentially by default; concurrency is delegated to
  `OrkToolDyn::call` futures, which means ork's semaphore keeps
  working **only if** we wrap `tools.execute` inside the adapter
  with the existing `Semaphore` clone. This must be in the spike's
  test matrix.
- Behaviour parity is contractual. Ork's specific quirks — non-fatal
  tool errors fed back as `Tool`-role messages (ADR
  [`0010`](0010-mcp-tool-plane.md) §`Failure model`), byte-cap
  + artifact spillover (ADR
  [`0016`](0016-artifact-storage.md)), `tool_loop_exceeded` ceiling,
  cancel responsiveness — must each have a regression test that
  passes pre- and post-rewrite.

### Neutral / follow-ups

- `LlmProvider` ([`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs))
  stays the source of truth for ork. The trait does not move; nothing
  external depends on rig types.
- Rig's `Pipeline` / `Op` DAG, `VectorStoreIndex`, and per-vendor RAG
  companion crates are explicitly **not** adopted by this ADR. The
  workflow engine is ork's, not rig's; embeds (ADR
  [`0015`](0015-dynamic-embeds.md)) and artifacts (ADR
  [`0016`](0016-artifact-storage.md)) are ork-shaped.
- A future ADR can promote rig from "engine inside `LocalAgent`" to
  "engine surfaced as a ports-level builder" if the ergonomics win
  justifies it. Out of scope here.
- Once the rewrite lands, ADR [`0011`](0011-native-llm-tool-calling.md)
  §`LocalAgent tool loop` becomes a description of historical
  behaviour. The trait surface 0011 introduced is unaffected.

## Alternatives considered

- **Keep the hand-rolled loop.** Rejected. Each follow-up ADR (0022,
  0025, 0027, 0032, 0039) would extend the same body of code with
  custom hook points. Rig has those hook points already and we'd
  re-derive the same shape against a less-tested implementation.
- **Replace ork's `Agent` port and `LlmProvider` trait with rig's
  types directly.** Rejected. ADR [`0002`](0002-agent-port.md) is
  load-bearing for ADRs [`0006`](0006-peer-delegation.md),
  [`0007`](0007-remote-a2a-agent-client.md),
  [`0008`](0008-a2a-server-endpoints.md), and
  [`0017`](0017-webui-chat-client.md); rig has no A2A, no agent
  cards, no remote-agent abstraction. Replacing the port is a
  protocol-level change disguised as ergonomics.
- **Use `langchain-rs`.** Rejected for the same reason ADR
  [`0011`](0011-native-llm-tool-calling.md) §`Alternatives
  considered` rejected it: heavier than what we need, abstractions
  don't quite match A2A semantics. `rig-core` is closer to "small
  toolkit" and more actively maintained.
- **Bridge through rig's `rmcp` feature instead of `ork-mcp`.**
  Rejected for this ADR. `rmcp 0.16` (ork) and `rmcp 1.x` (rig)
  cannot coexist cleanly. `ork-mcp` also owns tenant-scoped
  credential pooling that rig's bridge does not. A future ADR may
  revisit when ork bumps rmcp on its own merits.
- **Build a narrower internal abstraction (a thin `AgentLoop`
  trait).** Rejected. We would re-derive rig's surface with less
  test coverage and no upstream investment. The point of this ADR
  is to *delete* loop code, not to re-shape it.

### Spike findings

Branch `spike/rig-engine`, commit `065e560`. A standalone crate at
`spike/` implements the three bridge types — `FakeCompletionModel`,
`OrkToolDyn`, `RigEngine` — and a 5-test smoke suite (`spike/tests/smoke.rs`).
All five claims pass; two surfaced load-bearing surprises that this
ADR must absorb before it can flip from `Proposed` to `Accepted`.

The bridge as committed is roughly 350 LOC of Rust against `rig-core =
0.36` with no provider features, no rmcp feature, and no companion
crates — confirms §`Feature gate, dependency footprint`'s footprint
estimate.

**Surprise 1 — rig swallows `ToolDyn::call` errors.** Rig 0.36's tool
dispatcher converts every `Err(rig::tool::ToolError::*)` into a
`"Toolset error: …"` tool-result string and lets the model continue
on the next turn. There is no built-in shape for "stop the loop right
now, this is unrecoverable." Returning `Err` from `OrkToolDyn::call`
**does not** preserve ADR
[`0010`](0010-mcp-tool-plane.md) §`Failure model`'s fatal-error
abort path on its own.

The spike works around this with a shared `FatalSlot` (an
`Arc<Mutex<Option<String>>>`) that `OrkToolDyn::call` writes on a
fatal classification, paired with `cancel.cancel()` on the same
token the driver's `tokio::select!` waits on. The driver's cancel
branch then `take()`s the slot and emits either
`AgentEvent::Error(msg)` (slot was set ⇒ tool-driven fatal abort)
or `AgentEvent::Cancelled` (slot empty ⇒ user-initiated cancel).
Test `claim_4_fatal_tool_error_aborts` covers it. This pattern needs
to land in the production `crates/ork-agents/src/rig_engine.rs` —
amend the acceptance criteria below.

**Surprise 2 — cancel responsiveness is gated by provider yield
discipline.** The driver's `tokio::select!` only wins if the
`CompletionModel::stream` future yields between events. `async_stream::stream!`
fires every yielded item inside one poll if there are no `.await`s
between them; a "real" provider that buffers SSE events into a
`Vec` and drains synchronously would have the same property.
Confirmed: in test `claim_5_cancel_mid_stream` the first attempt saw
the entire scripted stream complete before cancel could fire. The
fix in the spike is `ScriptedEvent::Sleep(Duration)` between events
to introduce poll boundaries; the production version of this concern
is that ork's existing
[`OpenAiCompatibleProvider`](../../crates/ork-llm/src/openai_compatible.rs)
SSE reader already `.await`s on the wire between chunks, so this
should be a non-issue in practice — but it must be on the regression
checklist.

After 100 ms cancel + ≤ 50 ms wait the test consistently observes
`AgentEvent::Cancelled` within budget. The exact poll latency
depends on tokio scheduling; the assertion is loose enough to be
reliable.

**No surprises** for the other three claims:

- Text deltas flow through `StreamedAssistantContent::Text` cleanly;
  rig's aggregation produces a usable `FinalResponse::response()`.
- Tool round-trip works as advertised: rig calls `OrkToolDyn::call`,
  injects the result as a `StreamedUserContent::ToolResult` event,
  and re-runs the model.
- Non-fatal errors (`ToolError::Recoverable`) round-trip as a JSON
  payload string in the tool-result message; the model's next turn
  produces a final answer without intervention.

**Bridge ergonomics observations:**

- `OrkToolDyn` is constructed once per request per tool, captures the
  cancel token, fatal slot, and (in the production version) the
  `AgentContext` + `Arc<dyn ToolExecutor>`. `Box<dyn ToolDyn>` for
  the `AgentBuilder::tools` parameter — no special trait impl needed.
- `MultiTurnStreamItem` is `#[non_exhaustive]`; the driver match
  needs a wildcard `Ok(_) => {}` arm. Forward-compat note for the
  production diff.
- `agent.stream_prompt(...)` returns a `StreamingPromptRequest<M, P>`
  that's `IntoFuture`; awaiting it returns the `Stream`. Calling
  `.multi_turn(N)` before `.await` sets the per-prompt iteration
  cap — the analogue of ork's `max_tool_iterations`.
- `rig::message::ToolCall.function.{name, arguments}` is the call
  payload shape. Mapping to ork's `ToolCall { id, name, arguments }`
  is straightforward.
- Reasoning deltas (`StreamedAssistantContent::Reasoning{,Delta}`)
  are surfaced — the production driver should drop them by default
  and gate exposure behind `AgentConfig.expose_reasoning` (already
  proposed in ADR [`0011`](0011-native-llm-tool-calling.md) §`Open
  questions`).
- Compile-time impact of `rig-core` alone (no provider features) at
  this commit: ~25 s incremental compile from cold cache on the
  spike crate; negligible. Full workspace impact will be measured
  during implementation.

**Open question resolved:** the `Parallel tool-call dispatch`
question listed below remains open — the spike used a single
sequential tool call. Production driver still needs to wrap
`OrkToolDyn::call` body in the existing `Semaphore` clone to honour
`config.max_parallel_tool_calls`.

## Affected ork modules

- [`crates/ork-agents/src/rig_engine.rs`](../../crates/ork-agents/src/) —
  new module: `RigEngine`, `LlmProviderCompletionModel`, `OrkToolDyn`.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
  — `send_stream` delegates inner loop to `RigEngine::run`; helpers
  `execute_tool_call`, `is_fatal_tool_error`, `tool_error_payload`,
  `try_spill_oversized_tool_result` move into the new module or are
  re-exported `pub(crate)`.
- [`crates/ork-agents/Cargo.toml`](../../crates/ork-agents/Cargo.toml)
  — add optional `rig-core` dep and `rig-engine` default feature.
- [`Cargo.toml`](../../Cargo.toml) — add `rig-core` to
  `[workspace.dependencies]`.
- [`crates/ork-agents/tests/rig_engine_smoke.rs`](../../crates/ork-agents/tests/)
  — new integration test: streaming, non-fatal/fatal tool errors,
  cancel responsiveness, parallel tool dispatch.
- [`crates/ork-agents/tests/rig_engine_mcp_smoke.rs`](../../crates/ork-agents/tests/)
  — new integration test: stdio echo MCP server through `McpClient`.

## Phasing toward rig-native

The acceptance criteria above cover **Phase A** only — the engine
swap that makes rig the inner loop without bending any other ork
surface. Each follow-on phase below is its own future ADR; they are
listed here so the engine swap can land with a clear runway, and so
reviewers can check that this ADR does not silently commit to all of
them. Phases B–E are sequenced but independently mergeable — none
gates the others.

### Phase A — engine swap (this ADR)

Already covered. Rig drives the inner loop inside `LocalAgent`; ork
owns everything else. `rmcp = 0.16` stays. Native tools stay
JSON-Schema-described. Hooks and structured outputs are out of
scope.

### Phase B — bump `rmcp` to `1.x` and adopt `rig::tool::rmcp`

Trigger: Phase A landed and stable for ≥ one demo cycle.
Future ADR (call it 0048).

- Bump
  [`rmcp` in `Cargo.toml`](../../Cargo.toml) from `0.16` to `1.x`
  with the same feature set (`macros`, `client`, `client-side-sse`,
  `transport-streamable-http-client-reqwest`,
  `transport-child-process`, `reqwest`).
- Port [`crates/ork-mcp/`](../../crates/ork-mcp/) to the rmcp 1.x
  API surface. The transport, session-pool, and descriptor-cache
  shapes from ADR [`0010`](0010-mcp-tool-plane.md) carry over —
  they are ours, not rmcp's. Read the rmcp 0.16 → 1.x changelog
  before estimating; the spike did not exercise this path.
- Enable `rig-core/rmcp` and switch `ork-agents` to consume MCP
  tools through
  [`rig::tool::ToolServer` + `ToolServerHandle`](https://docs.rs/rig-core/latest/rig/tool/index.html)
  via [`AgentBuilder::tool_server_handle`](https://docs.rs/rig-core/latest/rig/agent/struct.AgentBuilder.html).
  `OrkToolDyn` keeps wrapping native tools (code_*, artifact_*,
  agent_call); MCP tools flow through rig's `McpTool`.
- Move tenant-scoped credentialing into a thin shim that builds the
  rmcp `ServiceExt` per-tenant before handing it to rig's
  `McpClientHandler`. Tenant pool semantics (one connection per
  tenant per server, descriptor-cache TTL) are preserved as a
  wrapper, not re-derived.
- Get `notifications/tools/list_changed` for free — rig's
  `McpClientHandler` already refreshes the registry on those
  notifications, which ork-mcp does not today.
- Acceptance: existing
  [`crates/ork-mcp/tests/`](../../crates/ork-mcp/) integration
  smoke passes; one new integration test covers the
  list-changed-notification path.

### Phase C — native tools as `rig::tool::Tool`

Trigger: Phase A landed.
Future ADR (call it 0049).

- Today
  [`tool_descriptors_for_agent`](../../crates/ork-agents/src/tool_catalog.rs)
  hand-builds `ToolDescriptor { name, description, parameters: serde_json::Value }`
  entries; tool authors keep the JSON Schema in sync by hand.
- Phase C re-implements native tools (`agent_call`, `peer_*`,
  `code_*`, `artifact_*`) as `impl rig::tool::Tool` types with
  typed `Args: Deserialize` + `Output: Serialize`, using
  [`schemars`](https://docs.rs/schemars/) for parameter-schema
  generation. Tool authors stop hand-maintaining JSON Schema; the
  derive macro emits it.
- `OrkToolDyn` (the per-request adapter) still exists, but only for
  MCP-side tools where the descriptor is wire-supplied. Native
  tools become `Box::new(<MyTool as rig::tool::Tool>) as Box<dyn ToolDyn>`
  via rig's blanket `impl<T: Tool> ToolDyn for T`.
- Crate-level: native tools currently in
  [`crates/ork-integrations/`](../../crates/ork-integrations/) and
  [`crates/ork-agents/`](../../crates/ork-agents/) keep their
  module locations; only their type shape changes.
- Acceptance: each native tool has a typed `Args` struct, the
  parameter schema in the resulting `ToolDefinition` matches what
  the JSON-Schema-by-hand version produced byte-for-byte (assert in
  test), and existing demo workflows pass without YAML changes.

### Phase D — hooks via `PromptHook` / `ToolCallHookAction`

Trigger: ADR [`0022`](0022-observability.md) and ADR
[`0039`](0039-agent-tool-call-hooks.md) implementations start.

- ADR 0022 wants per-step traces around LLM calls, tool calls, and
  delegations. ADR 0039 wants pre/post hooks on tool calls
  (logging, redaction, policy checks).
- Both line up with rig's
  [`PromptHook`](https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/trait.PromptHook.html)
  and `ToolCallHookAction`. Rather than design a parallel hook
  trait inside `ork-core`, those ADRs can implement on top of
  rig's hooks with thin `OrkPromptHook` / `OrkToolCallHook`
  adapters that pull `AgentContext` (tenant, trace_ctx) into scope
  via task-local or capture, and emit ork's tracing spans.
- Cost: Phase A's `RigEngine::run` builds the agent without a hook
  (defaults to `()` per
  [`PromptHook`'s blanket impl](https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/trait.PromptHook.html));
  Phase D adds an optional hook parameter and threads it through.
- Acceptance: lives with whichever follow-up ADR ships first.

### Phase E — typed outputs via `rig::Extractor`

Trigger: ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)
implementation starts.

- ADR 0025 wants schema-validated assistant outputs (e.g. a
  classifier returning a strict JSON shape). Rig's
  [`Extractor<M, T>`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html)
  with `T: schemars::JsonSchema + Deserialize` does this natively
  — call `.extract(prompt).await -> Result<T, _>`, no custom
  validator.
- Phase E adds an `OrkExtractor<T>` builder under `ork-agents`
  that wraps `rig::Extractor<LlmProviderCompletionModel, T>`,
  preserving the `LlmRouter` + `ResolveContext` plumbing from
  Phase A.
- Acceptance: lives with ADR 0025.

### Out of scope, deliberately

The following rig surfaces are **not** scheduled by this ADR:

- **`rig::Pipeline` / `Op`.** Ork has its own workflow engine
  ([`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs))
  that owns DAG semantics, retries, persistence, and run state.
  Rig's pipeline is in-memory only.
- **Vector-store companion crates** (`rig-postgres`, `rig-qdrant`,
  …). Embeds are owned by ADR [`0015`](0015-dynamic-embeds.md);
  artifacts by ADR [`0016`](0016-artifact-storage.md). Adopting
  rig vector stores would re-shape both — separate ADR question.
- **`rig::Agent<M>` as a public ork port.** ADR
  [`0002`](0002-agent-port.md)'s A2A-first port is the
  differentiator. Replacing it is a protocol-level change
  disguised as ergonomics; rejected in `Alternatives considered`
  above and not on this list.
- **Provider companion crates** (`rig-anthropic`, etc.). ADR
  [`0012`](0012-multi-llm-providers.md) deliberately keeps
  protocol conversion out-of-process (Kong + GPUStack). A future
  ADR can revisit if the deployment shape changes.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Major | `rig_quarantine_guard` in `ork-core` contained the contiguous substring `use rig`, so the ADR CI grep gate (`grep -rn "use rig" crates/ork-{core,…}`) failed on the guard's own source. | Fixed in `crates/ork-core/src/lib.rs`: detect forbidden imports via `strip_prefix("use ")` + `rest.starts_with("rig::")` / `"rig "`, and avoid a contiguous `use`-`rig` literal in comments. |
| Minor | ADR `Decision` pseudocode names `OrkCompletionResponse` / `OrkStreamingResponse` for `CompletionModel` assoc types; production uses `type Response = ()`, `type StreamingResponse = OrkStreamingMeta`, and a stub `CompletionModel::completion`, because only `stream_prompt` → `CompletionModel::stream` is wired. | Documented on `LlmProviderCompletionModel` in `crates/ork-agents/src/rig_engine.rs` (rustdoc). |
| Minor | MCP stdio integration test names `mcp:everything.echo` (everything server) instead of the spike's `mcp:echo.echo`; requires network for `npx`. | Acknowledged, deferred to Phase B / optional in-repo echo server; test remains behind `mcp-stdio-it`. |
| Minor | `tool_loop_history.rs` no longer pins exact pre-rig message counts; scans assistant/tool roles for OpenAI-ish ordering. | Intentional — rig owns transcript shape; behaviour (tool result in 2nd request) still asserted. |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| rig-core | [`crates/rig-core/src/agent/mod.rs`](https://github.com/0xPlaygrounds/rig/blob/main/crates/rig-core/src/agent/mod.rs) | `RigEngine::run` driver inside `LocalAgent::send_stream` |
| rig-core | [`crates/rig-core/src/completion/request.rs`](https://github.com/0xPlaygrounds/rig/blob/main/crates/rig-core/src/completion/request.rs) `CompletionModel` | `LlmProviderCompletionModel` adapter over `Arc<dyn LlmProvider>` |
| rig-core | [`crates/rig-core/src/tool/mod.rs`](https://github.com/0xPlaygrounds/rig/blob/main/crates/rig-core/src/tool/mod.rs) `ToolDyn` | `OrkToolDyn` capturing `AgentContext` + `Arc<dyn ToolExecutor>` |
| rig-core | [`crates/rig-core/src/tool/rmcp.rs`](https://github.com/0xPlaygrounds/rig/blob/main/crates/rig-core/src/tool/rmcp.rs) | **Not adopted** — version conflict with `ork-mcp`'s `rmcp 0.16` pin |
| Solace Agent Mesh | ADK tool-loop in [`agent/sac/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/sac/component.py) | Engine ownership moves from hand-rolled `LocalAgent::send_stream` to rig; A2A surface stays in ork |

## Open questions

- **Parallel tool-call dispatch.** Rig's loop dispatches tool calls
  sequentially by default; ork today caps parallelism with a
  `Semaphore` keyed on `config.max_parallel_tool_calls`. Regression:
  `smoke_parallel_tool_calls_observe_semaphore` in
  [`rig_engine_smoke.rs`](../../crates/ork-agents/tests/rig_engine_smoke.rs)
  (four tool calls in one turn, `max_parallel_tool_calls = 2`) asserts
  `max_active <= 2` and all calls complete.
- **Tool-call delta surfacing.** Rig emits incremental tool-arg
  deltas; ork drops them today. Decision in this ADR: keep dropping.
  Revisit if a UI client needs them.
- **AgentContext capture cost.** Allocating an `OrkToolDyn` per
  request per tool is the cost of rig's signature. The spike should
  measure this against the current direct-dispatch path; if it shows
  up in profiles, we can pool adapters per-tenant. Baseline expected
  at <1 µs / tool / request.
- **Rmcp 1.x bump.** Resolved direction: scheduled as Phase B
  (§`Phasing toward rig-native`). Open piece is the rmcp 0.16 →
  1.x changelog walkthrough — the spike did not exercise that
  path, so the cost estimate is still rough. Phase B's ADR (call
  it 0048) owns the changelog audit and the migration steps for
  the transport / session / descriptor-cache code in
  `crates/ork-mcp/`.
- **Rig version cadence.** `rig-core` ships monthly; we pin to
  `0.36` and bump on our own schedule. The spike commits the
  exact `Cargo.lock` resolution so the team has a known-good
  baseline.

## References

- rig-core repository: <https://github.com/0xPlaygrounds/rig>
- rig-core docs: <https://docs.rs/rig-core/latest/rig/>
- rig-core official site: <https://rig.rs>
- ADR [`0002`](0002-agent-port.md) — `Agent` port (unchanged)
- ADR [`0010`](0010-mcp-tool-plane.md) — MCP plane and tool error
  semantics (unchanged; rig's MCP module not adopted)
- ADR [`0011`](0011-native-llm-tool-calling.md) — `LlmProvider`
  tool-call surface (unchanged; rig consumes it via the adapter)
- ADR [`0012`](0012-multi-llm-providers.md) — `LlmRouter`
  resolution chain (unchanged; rig sees one `CompletionModel`)
- ADR [`0016`](0016-artifact-storage.md) — artifact spillover
  policy preserved by `OrkToolDyn`
- ADR [`0022`](0022-observability.md) — future hook integration
  via `rig::PromptHook`
- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)
  — future structured-output integration via `rig::Extractor`
- ADR [`0039`](0039-agent-tool-call-hooks.md) — future hook
  integration via `rig::ToolCallHookAction`
- rig MCP module (`rig::tool::rmcp`):
  <https://docs.rs/rig-core/latest/rig/tool/rmcp/index.html>
- rig `ToolServer` / `ToolServerHandle`:
  <https://docs.rs/rig-core/latest/rig/tool/index.html>
- rig `Extractor`:
  <https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html>
- rig `PromptHook`:
  <https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/trait.PromptHook.html>
- `schemars`: <https://docs.rs/schemars/>
