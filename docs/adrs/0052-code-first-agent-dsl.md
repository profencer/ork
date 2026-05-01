# 0052 — Code-first Agent DSL on `rig::Agent` with structured outputs

- **Status:** Proposed
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0002, 0003, 0006, 0011, 0012, 0034, 0047, 0048, 0049, 0051, 0053, 0054
- **Supersedes:** 0025, 0033

## Context

The hand-rolled `LocalAgent` in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
is a 270-line `async_stream::stream!` that builds chat requests,
manages history, dispatches tools, and emits A2A events. ADR
[`0047`](0047-rig-as-local-agent-engine.md) Phase A swaps the inner
loop to `rig::agent::Agent`, but does not change the *authoring
shape* — agents are still constructed by ad-hoc factory code in
[`crates/ork-cli/`](../../crates/ork-cli/) and the demo scripts.

Mastra's
[`new Agent({ id, name, description, instructions, model, tools,
agents, workflows, memory, scorers, voice, defaultOptions,
inputProcessors, outputProcessors, requestContextSchema })`](https://mastra.ai/reference/agents/agent)
is the user-facing shape. Sub-agents and workflows are first-class
collaborators (the agent can invoke them as tools); structured
output is configured per-call. After ADR 0047 the rig engine
provides every primitive needed to ship the same shape in Rust;
this ADR is the user-facing API.

## Decision

ork **introduces `CodeAgent::builder(id)`**, a typestate Rust
builder that produces a value implementing ork's
[`Agent` port](../../crates/ork-core/src/ports/agent.rs) (preserving
the A2A-first invariant from
[`AGENTS.md`](../../AGENTS.md) §3) and runs on top of the rig engine
introduced by ADR 0047. The hand-rolled `LocalAgent` becomes one of
two things `OrkApp` may register: a low-level user-supplied `dyn
Agent` (still allowed) or a `CodeAgent` built via this DSL (the
common path).

```rust
use ork_agents::{CodeAgent, agent_as_tool};

pub fn weather_agent() -> CodeAgent {
    CodeAgent::builder("weather")
        .description("Reports weather conditions for a city.")
        .instructions("You are a meteorologist. Answer in <= 2 sentences.")
        .model("openai/gpt-4o-mini")
        .tool(weather_tool())                 // ADR 0051
        .tool_server("docs")                  // MCP server registered on OrkApp
        .agent_as_tool("forecaster")          // sub-agent invocable
        .max_steps(8)
        .temperature(0.2)
        .build()
        .expect("weather agent builder")
}
```

### Builder surface

```rust
// crates/ork-agents/src/code_agent.rs
pub struct CodeAgentBuilder { /* private */ }

impl CodeAgent {
    pub fn builder(id: impl Into<String>) -> CodeAgentBuilder;
}

impl CodeAgentBuilder {
    // identity / description
    pub fn description(self, s: impl Into<String>) -> Self;
    pub fn skills(self, s: Vec<AgentSkill>) -> Self;     // A2A AgentCard skills

    // prompting
    pub fn instructions(self, s: impl Into<InstructionSpec>) -> Self;
    pub fn dynamic_instructions<F>(self, f: F) -> Self
    where F: Fn(&AgentContext) -> Pin<Box<dyn Future<Output = String> + Send>>
              + Send + Sync + 'static;

    // model selection (fed to LlmRouter, ADR 0012)
    pub fn model(self, spec: impl Into<ModelSpec>) -> Self;
    pub fn dynamic_model<F>(self, f: F) -> Self
    where F: Fn(&AgentContext) -> Pin<Box<dyn Future<Output = ModelSpec> + Send>>
              + Send + Sync + 'static;
    pub fn temperature(self, t: f32) -> Self;
    pub fn max_tokens(self, n: u32) -> Self;
    pub fn additional_params(self, v: serde_json::Value) -> Self;

    // tools, sub-agents, workflows
    pub fn tool<T: IntoToolDef>(self, t: T) -> Self;       // ADR 0051
    pub fn tool_server(self, mcp_server_id: impl Into<String>) -> Self;
    pub fn agent_as_tool(self, agent_id: impl Into<String>) -> Self;
    pub fn workflow_as_tool(self, workflow_id: impl Into<String>) -> Self;
    pub fn dynamic_tools<F>(self, f: F) -> Self
    where F: Fn(&AgentContext) -> Vec<Box<dyn ToolDef>> + Send + Sync + 'static;

    // memory (ADR 0053)
    pub fn memory<M: IntoMemoryHandle>(self, m: M) -> Self;
    pub fn memory_options(self, o: MemoryOptions) -> Self;

    // structured outputs via rig::Extractor
    pub fn output_schema<O: JsonSchema + DeserializeOwned + Send + Sync + 'static>(self)
        -> CodeAgentBuilder /* a typed sub-builder, see below */;

    // limits
    pub fn max_steps(self, n: u32) -> Self;
    pub fn max_parallel_tool_calls(self, n: u32) -> Self;

    // request validation
    pub fn request_context_schema<C: JsonSchema + DeserializeOwned>(self) -> Self;

    // scorers (ADR 0054)
    pub fn scorer(self, s: ScorerBinding) -> Self;

    // hooks (ADR 0058 observability + ADR 0039-superseded shape)
    pub fn on_tool_call<H: ToolHook + 'static>(self, h: H) -> Self;
    pub fn on_completion<H: CompletionHook + 'static>(self, h: H) -> Self;

    pub fn build(self) -> Result<CodeAgent, OrkError>;
}

pub struct CodeAgent { /* implements `dyn Agent` from ADR 0002 */ }

impl Agent for CodeAgent {
    fn agent_card(&self) -> &AgentCard { /* derived from id, description, skills */ }
    async fn send_stream(&self, ctx: AgentContext, msg: A2aMessage)
        -> Result<AgentEventStream, OrkError> { /* ... */ }
    async fn cancel(&self, ctx: AgentContext, task_id: TaskId)
        -> Result<(), OrkError> { /* ... */ }
}
```

The builder produces a `CodeAgent` value. ork's `Agent` port from
ADR [`0002`](0002-agent-port.md) is implemented on it; A2A surface
(cards, tasks, streaming, push) is *automatic*. Remote callers
(ADR [`0007`](0007-remote-a2a-agent-client.md)) reach a `CodeAgent`
exactly the way they reach today's `LocalAgent`.

### Structured output via `rig::Extractor`

```rust
#[derive(Deserialize, JsonSchema)]
struct Forecast { high_f: f32, low_f: f32, summary: String }

let typed = CodeAgent::builder("forecaster")
    .instructions("Produce a structured forecast.")
    .model("openai/gpt-4o-mini")
    .output_schema::<Forecast>()
    .build()?;

// Per-call:
let f: Forecast = app.run_agent_typed::<Forecast>(
    "forecaster", ctx, "Forecast for SF tomorrow.").await?;
```

`output_schema::<O>()` switches the agent into extractor mode: rig's
[`Extractor<M, O>`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html)
is built per call, the LLM is steered to call a `submit` tool with
`O`-shaped arguments, and the agent's stream emits a single
`AgentEvent::Output(serde_json::Value)` for `O` (plus the regular
status events). The A2A surface still applies — the response is a
`Data` part with the typed payload.

This subsumes ADR
[`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
"verifier-agent" pattern by a strict construction: the *extractor*
guarantees the shape, no separate verifier needed.

### Sub-agents and workflows as tools

`agent_as_tool("forecaster")` and `workflow_as_tool("nightly")`
register *the registered-component-id* as a callable tool. At
build time the ADR 0049 registry is consulted to confirm the id
exists; at call time the agent path goes through ADR
[`0006`](0006-peer-delegation.md) for sub-agents (so federation /
delegation chain logging stays intact) and through ADR 0050's
`run_workflow` for workflows.

This is Mastra's
[`agents`](https://mastra.ai/reference/agents/agent) and
[`workflows`](https://mastra.ai/reference/agents/agent) parameters
collapsed into one consistent verb.

### Dynamic instructions, model, tools

```rust
let agent = CodeAgent::builder("triage")
    .dynamic_instructions(|ctx| Box::pin(async move {
        let role = ctx.tenant_role().await;
        format!("You are a {} support agent.", role)
    }))
    .dynamic_model(|ctx| Box::pin(async move {
        if ctx.tenant_tier() == "premium" {
            ModelSpec::from("openai/gpt-4o")
        } else {
            ModelSpec::from("openai/gpt-4o-mini")
        }
    }))
    .build()?;
```

These resolve at request entry, before rig is invoked. The
`AgentContext` is the existing per-request value
(tenant, cancel token, caller). `ModelSpec` flows into ADR 0012's
[`LlmRouter`](../../crates/ork-llm/src/router.rs) resolution chain
unchanged; per-tenant model switching via
[`0034`](0034-per-model-capability-profiles.md) still applies.

### `request_context_schema`

`request_context_schema::<C>()` declares the JSON Schema the request
must satisfy. The auto-generated server (ADR 0056) emits the schema
in OpenAPI; Studio (ADR 0055) surfaces it as the form for "send a
message"; the runtime validates request bodies against it.

This replaces the ad-hoc validation scattered through
[`crates/ork-api/`](../../crates/ork-api/).

### Hooks

```rust
pub trait ToolHook: Send + Sync {
    async fn before(&self, ctx: &AgentContext, descriptor: &ToolDescriptor,
                    args: &serde_json::Value) -> ToolHookAction;
    async fn after(&self, ctx: &AgentContext, descriptor: &ToolDescriptor,
                   result: &Result<serde_json::Value, OrkError>);
}

pub enum ToolHookAction { Proceed, Override(serde_json::Value), Cancel }
```

`ToolHook` and `CompletionHook` are thin shims over rig's
[`PromptHook`](https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/trait.PromptHook.html)
/ `ToolCallHookAction` (ADR 0047 §`Phase D`). Observability (ADR
0058) ships a default hook that emits OTel spans; user code can
attach additional hooks for redaction, policy checks, audit.

This subsumes ADR 0039 (superseded).

## Acceptance criteria

- [ ] `CodeAgent::builder(id) -> CodeAgentBuilder` exported from
      [`crates/ork-agents/src/code_agent.rs`](../../crates/ork-agents/src/)
      with the signatures shown in `Decision`.
- [ ] `CodeAgent` implements
      [`Agent`](../../crates/ork-core/src/ports/agent.rs) and
      satisfies the A2A surface tests in
      [`crates/ork-agents/tests/`](../../crates/ork-agents/tests/)
      (cards, tasks, streaming, cancel) without modification.
- [ ] Builder enforces required fields at build time: id is set,
      instructions is set (static or dynamic), model is set
      (static or dynamic). Each missing field returns
      `Err(OrkError::Configuration)` with a specific message.
- [ ] `output_schema::<O>()` produces a `CodeAgent` whose
      `send_stream` emits exactly one
      `AgentEvent::Output(serde_json::Value)` per request, where
      `O::deserialize(value)` succeeds. Verified by integration
      test
      `crates/ork-agents/tests/code_agent_extractor.rs` against a
      scripted LLM that returns the right `submit`-tool call.
- [ ] `agent_as_tool` and `workflow_as_tool`: build-time check
      that the referenced id exists in the same `OrkApp`. The
      registry consultation happens at `OrkApp::build()`, not at
      `CodeAgent::build()`, since the builder runs first; the
      `OrkApp` build step performs the cross-reference.
- [ ] `dynamic_instructions`, `dynamic_model`, `dynamic_tools`
      each have one happy-path integration test in
      `crates/ork-agents/tests/code_agent_dynamic.rs`.
- [ ] `request_context_schema::<C>()` produces JSON Schema
      readable by ADR 0056's OpenAPI emitter. Schema bytes
      asserted in a snapshot test.
- [ ] Hook surface implemented via rig's `PromptHook` /
      `ToolCallHookAction` with the ork-shape adapter; integration
      test
      `crates/ork-agents/tests/code_agent_hooks.rs` covers
      `Proceed` / `Override` / `Cancel` paths.
- [ ] At least three existing demo agents under
      [`demo/`](../../demo/) are reauthored with `CodeAgent`. The
      pre/post diff shows ~5–15× line reduction per agent.
- [ ] Hand-rolled `LocalAgent` stays as a low-level option for
      agents that need behaviour outside the builder shape (e.g.,
      tightly bespoke streaming). The builder is the *primary*
      authoring path; `LocalAgent` is documented as a legacy
      escape hatch in
      [`crates/ork-agents/src/lib.rs`](../../crates/ork-agents/src/lib.rs).
- [ ] [`README.md`](README.md) ADR index row added; ADRs 0025 and
      0033 status flipped to `Superseded by 0052`.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- The agent author writes one expression. The instruction string,
  model spec, tool list, sub-agent list, and structured-output
  schema are co-located. Refactoring is a Rust refactor; the
  compiler routes the changes.
- Structured outputs come for free via rig's `Extractor`. ADR 0025
  collapses; "verifier agent" patterns are unnecessary because the
  shape is enforced at the model boundary.
- Sub-agent and workflow composition is symmetric. Calling another
  agent and calling a workflow look the same to the LLM (`tool_*`
  schemas of the same shape). Federation through ADR 0006 stays
  the path of record.
- Hooks land via rig's existing surface, no parallel hook
  framework. Observability (ADR 0058) attaches one hook; user code
  attaches additional hooks for redaction or audit.
- Dynamic resolution (instructions, model, tools) covers the
  per-tenant / per-tier case that is currently spread across
  agent factories.

### Negative / costs

- Every `CodeAgent` carries the rig dependency transitively (via
  ADR 0047). For users who want a hand-rolled agent without rig,
  they keep using `LocalAgent` directly. The "two paths" cost is
  small but non-zero; the builder is the dominant case.
- The builder's `output_schema::<O>()` returns a typed sub-builder
  in this draft; the final shape may need to be a top-level
  `CodeAgent::extractor::<O>(id)` constructor. Decided during
  implementation; both are equivalent in semantics.
- `agent_as_tool` and `workflow_as_tool` cross-references mean
  `OrkApp::build()` is not embarrassingly parallel — a topological
  order over registered components is required to validate. The
  acceptance criterion specifies this is a build-time check.
- The hook surface is generic over `&AgentContext`. Hooks that
  need to mutate per-request state must do so through the context
  (cancel, tracing). Documented in the hook trait docs.

### Neutral / follow-ups

- A `#[derive(CodeAgent)]` macro could collapse the builder syntax
  for simple cases. Out of scope.
- `voice` (TTS/STT) — Mastra wires it on the agent. ork punts to
  a future ADR; the builder gains a `.voice(...)` method when
  there is a customer asking for voice.
- `inputProcessors` / `outputProcessors` — Mastra's pre/post-LLM
  message transforms. The hook surface covers most of this; a
  dedicated processor surface lands when a pattern emerges.
- Ports for `MemoryHandle`, `ScorerBinding`, `IntoMemoryHandle`
  arrive via ADRs 0053 and 0054; the builder API references them
  before they ship and gets minor signature touch-ups when
  those land.

## Alternatives considered

- **Skip the builder; export `rig::AgentBuilder` directly.**
  Rejected. rig's `AgentBuilder<M>` is generic over the
  completion model and does not satisfy ork's `Agent` port (no
  A2A card, no task lifecycle, no per-tenant context). Re-exporting
  it would push every consumer to bridge themselves.
- **Single `Agent::new(config)` constructor with a config struct.**
  Rejected. The config-struct shape forces every field to be
  present at definition time, fights with closures (dynamic
  resolvers), and produces worse compile-time errors. The builder
  shape composes incrementally.
- **Replace `LocalAgent` entirely with `CodeAgent`.** Rejected.
  Some operators want a hand-rolled agent for tightly bespoke
  behaviour. Keeping `LocalAgent` as an escape hatch costs little
  and preserves backward compat.
- **Keep ADR 0025's verifier-agent pattern alongside Extractor.**
  Rejected. Two ways to validate the same boundary is the source
  of bugs. Extractor is strictly stronger because the model is
  *steered* to the shape, not validated after the fact.

## Affected ork modules

- [`crates/ork-agents/src/code_agent.rs`](../../crates/ork-agents/src/)
  — new module: builder, types.
- [`crates/ork-agents/src/lib.rs`](../../crates/ork-agents/src/lib.rs)
  — re-export `CodeAgent`.
- [`crates/ork-agents/src/rig_engine.rs`](../../crates/ork-agents/src/)
  — engine driver consumed by `CodeAgent::send_stream`; ADR 0047
  shipped this module.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/) — any
  new ports the builder references (`MemoryHandle`,
  `ScorerBinding`) live here, defined by ADRs 0053 / 0054.
- [`demo/`](../../demo/) — reauthor 3+ existing agents.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Agent reference](https://mastra.ai/reference/agents/agent) | `CodeAgent::builder(id)` |
| Mastra | [Dynamic agents](https://mastra.ai/docs/agents/dynamic-agents) | `dynamic_instructions`, `dynamic_model`, `dynamic_tools` |
| rig | [`AgentBuilder`](https://docs.rs/rig-core/latest/rig/agent/struct.AgentBuilder.html) | inner engine |
| rig | [`Extractor`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html) | `output_schema::<O>()` |
| rig | [`PromptHook`](https://docs.rs/rig-core/latest/rig/agent/prompt_request/hooks/trait.PromptHook.html) | hook adapter |
| Solace Agent Mesh | `SamAgentComponent` constructor | replaced by `CodeAgent::builder` |

## Open questions

- **Sub-agent/workflow forward references.** A user may construct
  an `agent_as_tool("forecaster")` before `forecaster` is built.
  We resolve at `OrkApp::build()` time; the order in the builder
  chain is irrelevant. Confirmed in the acceptance criteria.
- **`output_schema` vs separate type.** Decided during
  implementation: either `CodeAgentBuilder::output_schema::<O>()`
  returns a typed sub-builder, or a separate
  `CodeAgent::extractor::<O>(id)` constructor exists for the
  extractor shape. Both are valid; pick the cleaner ergonomic.
- **A2A skill derivation.** Today an `AgentCard` lists `skills`.
  The builder's `description` and tool list could auto-derive a
  default skill set (Mastra does not have an analogue here). Default:
  one skill per agent, name = id, description = description; user
  can override with `.skills(...)`.
- **Mastra parity for `inputProcessors` / `outputProcessors`.** We
  ship hooks; processors as a separate concept arrive only if a
  pattern emerges. Documented as deliberate omission.
- **Voice.** Out of v1 scope. Future ADR if a customer needs it.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — registry.
- ADR [`0047`](0047-rig-as-local-agent-engine.md) — engine.
- ADR [`0002`](0002-agent-port.md) — `Agent` port (unchanged).
- ADR [`0006`](0006-peer-delegation.md) — sub-agent calls.
- ADR [`0012`](0012-multi-llm-providers.md) — model resolution.
- ADR [`0034`](0034-per-model-capability-profiles.md) — per-model
  profiles consumed by `model` / `dynamic_model`.
- Mastra agent reference:
  <https://mastra.ai/reference/agents/agent>
- rig agent: <https://docs.rs/rig-core/latest/rig/agent/index.html>
- rig extractor:
  <https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html>
