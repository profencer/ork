# 0051 — Code-first Tool DSL on `rig::Tool` with typed Args/Output

- **Status:** Proposed
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0010, 0011, 0047, 0048, 0049, 0050, 0052
- **Supersedes:** —

## Context

Native tools today are described by hand-built JSON Schemas in
[`crates/ork-agents/src/tool_catalog.rs`](../../crates/ork-agents/src/tool_catalog.rs)
and dispatched through the
[`ToolExecutor`](../../crates/ork-core/src/workflow/engine.rs)
trait. `ToolDescriptor { name, description, parameters:
serde_json::Value }` carries the schema as raw JSON; the
implementation that consumes that descriptor is a separate function
the author has to keep in sync. ADR
[`0011`](0011-native-llm-tool-calling.md) formalised this for the
LLM tool-calling path; ADR
[`0047`](0047-rig-as-local-agent-engine.md) §`Phase C` previewed the
next step: re-implement native tools as `impl rig::tool::Tool` types
with `#[derive(JsonSchema, Deserialize)]` `Args` so the schema is
generated, not hand-written.

The pivot ([`0048`](0048-pivot-to-code-first-rig-platform.md))
makes that the *primary* tool-authoring surface. Mastra's
[`createTool`](https://mastra.ai/reference/tools/create-tool) ships
the same shape:

```typescript
export const tool = createTool({
  id: 'reverse',
  description: 'Reverse the input string',
  inputSchema: z.object({ input: z.string() }),
  outputSchema: z.object({ output: z.string() }),
  execute: async (inputData) => ({ output: inputData.input.split('').reverse().join('') }),
})
```

Rust's analogue with `schemars` + rig's typed `Tool` is identical in
spirit and stricter in the type system.

## Decision

ork **introduces a `Tool` builder DSL** that produces values
implementing `rig::tool::Tool` and ork's `ToolDef` port (registered
with [`OrkApp`](0049-orkapp-central-registry.md)). The `MCPClient`
plane from ADR [`0010`](0010-mcp-tool-plane.md) stays as the way
*external* tools enter the system; this ADR is about the
*authoring* shape for *native* tools. MCP tools surface through the
same `ToolDef` port so a user-facing call site does not see the
native-vs-MCP split.

```rust
use ork_tool::{tool, ToolContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, JsonSchema)]
struct ReverseIn { input: String }

#[derive(Serialize, JsonSchema)]
struct ReverseOut { output: String }

pub fn reverse_tool() -> impl ork_tool::IntoToolDef {
    tool("reverse")
        .description("Reverse the input string")
        .input::<ReverseIn>()
        .output::<ReverseOut>()
        .execute(|ctx, ReverseIn { input }| async move {
            Ok(ReverseOut { output: input.chars().rev().collect() })
        })
}
```

User registers via
[`OrkApp::builder().tool(reverse_tool())`](0049-orkapp-central-registry.md).

### Tool surface

```rust
// crates/ork-tool/src/lib.rs
pub fn tool(id: impl Into<String>) -> ToolBuilder<(), ()>;

pub struct ToolBuilder<I, O> { /* typestate */ }

impl<I, O> ToolBuilder<I, O> {
    pub fn input<X: JsonSchema + DeserializeOwned + Send + Sync + 'static>(self)
        -> ToolBuilder<X, O>;
    pub fn output<X: JsonSchema + Serialize + Send + Sync + 'static>(self)
        -> ToolBuilder<I, X>;

    pub fn description(self, s: impl Into<String>) -> Self;
    pub fn timeout(self, d: Duration) -> Self;
    pub fn retry(self, p: RetryPolicy) -> Self;

    /// Mark non-fatal: classification fed to ADR 0010 §`Failure
    /// model`. Default is non-fatal (LLM gets the error and may
    /// recover); `.fatal_on(|err| ...)` flips classification per
    /// error variant.
    pub fn fatal_on<F: Fn(&OrkError) -> bool + Send + Sync + 'static>(self, f: F) -> Self;

    /// The execute closure. The captured environment is part of the
    /// tool value; `ToolContext` carries per-call state.
    pub fn execute<F, Fut>(self, f: F) -> Tool<I, O>
    where
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, OrkError>> + Send + 'static;
}

pub struct ToolContext {
    pub agent_context: AgentContext,
    pub run: RunInfo,
    pub artifact_store: ArtifactHandle,   // ADR 0016
    pub memory: MemoryHandle,             // ADR 0053 (optional)
}

pub struct Tool<I, O> { /* private */ }

pub trait IntoToolDef { fn into_tool_def(self) -> Box<dyn ToolDef>; }
impl<I, O> IntoToolDef for Tool<I, O> { /* ... */ }
```

### Trait bridges

`Tool<I, O>` implements three interfaces:

1. **ork's `ToolDef` port** (defined in `ork-core`) — the surface
   `OrkApp` registers and that the workflow engine (ADR 0050)
   calls via `ctx.tools().call(id, value)`.
2. **`rig::tool::Tool`** — when the agent (ADR 0052) is built
   on top of rig, the same `Tool<I, O>` value is wrapped (or
   blanket-implemented through) a rig-shaped tool, so the LLM
   tool-call path goes directly through rig with no parallel
   schema definition.
3. **MCP exposure** — every native `Tool<I, O>` may be re-exported
   via the MCP server surface (ADR 0061-future / Mastra's
   [`MCPServer`](https://mastra.ai/docs/mcp/overview)) so external
   agents can call ork-native tools over the wire.

### Schema generation

```rust
impl<I: JsonSchema, O: JsonSchema> Tool<I, O> {
    pub fn parameters_schema(&self) -> serde_json::Value {
        // schemars::schema_for!(I) → serde_json::Value
    }
    pub fn output_schema(&self) -> serde_json::Value {
        // schemars::schema_for!(O) → serde_json::Value
    }
}
```

The generated parameter schema is what rig's `ToolDefinition`
carries to the LLM, what Studio renders as the tool's input form
(ADR 0055), and what the auto-generated REST surface validates
against (ADR 0056). One source.

### Failure model parity

ADR [`0010`](0010-mcp-tool-plane.md) §`Failure model` distinguishes
**non-fatal** (LLM sees a tool-result with an error, may
self-correct) from **fatal** (the run aborts). The new builder
preserves that:

- Default: every `OrkError` returned by `.execute(...)` is
  non-fatal. The LLM sees the error payload as a tool result.
- `.fatal_on(|err| matches!(err, OrkError::Authn(_)))` lets the
  author flip classifications per variant. Authn errors,
  permission errors, and tenant-bound configuration errors are the
  canonical fatal cases.
- Inside [`crates/ork-agents/src/rig_engine.rs`](../../crates/ork-agents/src/)
  (introduced by ADR 0047), the `OrkToolDyn` adapter consults this
  classification. The `FatalSlot` mechanism from ADR 0047
  §`Spike findings` *Surprise 1* drives loop abort.

### MCP tools through the same surface

ADR [`0010`](0010-mcp-tool-plane.md) plumbs MCP tools through
[`McpClient`](../../crates/ork-mcp/src/client.rs) and
`tool_descriptors_for_agent`. After this ADR, MCP tools are still
sourced from `McpClient` but presented to user code as
`Box<dyn ToolDef>` values — the same trait `Tool<I, O>` implements.
The user does not write a builder for MCP tools; they register an
MCP server with [`OrkApp::builder().mcp_server(...)`](0049-orkapp-central-registry.md)
and the system autodiscovers tools from it.

The schema for MCP tools comes from the wire (`tools/list`); the
builder's typed `I, O` shape is unique to native tools.

### `dynamic_tools`

For tools whose existence depends on per-request context (Mastra's
dynamic tools, rig's `dynamic_tools`), the builder accepts a
resolver:

```rust
let user_specific = tool("send_invoice")
    .input::<InvoiceIn>().output::<InvoiceOut>()
    .gate(|ctx| ctx.tenant_can("billing.write"))
    .execute(|ctx, x| async move { /* ... */ });
```

`.gate(predicate)` causes the tool to be **omitted** from the
descriptor list passed to the LLM when the predicate returns false.
This replaces the ad-hoc per-tenant filtering scattered across
[`crates/ork-agents/`](../../crates/ork-agents/).

## Acceptance criteria

- [ ] New crate `crates/ork-tool/` with `Cargo.toml` declaring
      `ork-core`, `ork-common`, `serde`, `schemars`, `tokio`,
      `futures`, `rig-core` (workspace dep). No `axum`/`sqlx`/
      `reqwest`/`rmcp`/`rskafka`.
- [ ] `tool(id) -> ToolBuilder<(), ()>` exported from
      `crates/ork-tool/src/lib.rs` with the signature shown in
      `Decision`.
- [ ] `ToolBuilder` enforces typestate: `.execute(...)` is gated
      on `I` and `O` having been set via `.input::<X>()` /
      `.output::<X>()`. Demonstrated by a `compile_fail` doc test.
- [ ] `Tool<I, O>` implements: (a) ork's `ToolDef` port from
      `ork-core`; (b) `rig::tool::Tool` (or convertible to it via
      a private adapter); (c) `IntoToolDef`.
- [ ] `Tool<I, O>::parameters_schema()` returns the
      `schemars`-generated schema for `I`, byte-for-byte equal to
      `serde_json::to_value(schemars::schema_for!(I))`. Asserted
      in `crates/ork-tool/tests/schema_roundtrip.rs`.
- [ ] `OrkAppBuilder::tool(t)` accepts any `IntoToolDef`. Test
      `crates/ork-tool/tests/registry.rs` builds an `OrkApp` with
      one native tool and one MCP-server-spec, calls
      `app.tool("reverse")` and asserts the descriptor matches
      the builder's input.
- [ ] Failure-model parity test under
      `crates/ork-tool/tests/failure_model.rs`: a tool returning
      `OrkError::Validation` round-trips to the LLM as a non-fatal
      tool result; one returning `OrkError::Authn` aborts the run
      via the `FatalSlot` path from ADR 0047.
- [ ] `.gate(predicate)` test under
      `crates/ork-tool/tests/dynamic_visibility.rs`: an agent run
      with `tenant_can("billing.write") == false` does not see
      the gated tool in the descriptor list passed to rig.
- [ ] [`crates/ork-agents/src/tool_catalog.rs`](../../crates/ork-agents/src/tool_catalog.rs)
      consumes `Box<dyn ToolDef>` from `OrkApp` rather than
      hand-built `ToolDescriptor` values; the existing native-tool
      authors (`agent_call`, `peer_*`, code-tools per ADR 0006 /
      0011) migrate to the new builder. **Bytes-for-bytes**
      equivalence of the resulting descriptors is asserted in
      `crates/ork-agents/tests/tool_catalog_parity.rs`.
- [ ] CI grep: no file under `crates/ork-tool/` imports `axum`,
      `sqlx`, `reqwest`, `rmcp`, or `rskafka`.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- One source of truth per tool: typed `Args`, typed `Output`, the
  `execute` closure, and a description string. The LLM-visible
  schema, the Studio form, the REST validator, and the MCP-export
  schema all derive from the same `JsonSchema`.
- Closures-with-captured-state are first-class. A tool that needs
  a database pool or a remote client just captures it; no
  registry-of-tool-state plumbing.
- `.gate(predicate)` formalises per-tenant tool visibility, a
  pattern scattered ad-hoc today.
- Bridge to rig is clean (rig already accepts typed
  `impl Tool<Args = T>`); ADR 0047 Phase C lands inside this ADR.

### Negative / costs

- Two trait surfaces per tool (`ToolDef` for ork's workflow
  engine, `rig::tool::Tool` for the rig-driven agent loop). The
  bridge keeps them in sync via a single concrete `Tool<I, O>`
  type, but the indirection is non-zero.
- `schemars` derive cost on the input/output types is paid at
  compile time. Negligible per type; non-trivial at scale (many
  tools = many derives). Documented as `Open question` if a user
  hits it.
- Migration cost: every existing native tool must be re-authored.
  The compatibility test (`tool_catalog_parity.rs`) keeps the
  schema bytes identical, but the *implementation* moves. We
  schedule the migration crate-by-crate and mark which tools are
  done in the implementation diff.
- `rig::tool::Tool`'s `Args = String` (rig deserialises inside
  `call`) vs ork's typed `Args` requires an adapter layer. The
  adapter is in `crates/ork-tool/src/rig_adapter.rs`; transparent
  to users.

### Neutral / follow-ups

- ADR 0061 (future) — a `MCPServer` adapter that auto-exposes
  every registered native `Tool<I, O>` over the MCP wire so other
  agents can call ork tools as MCP. Mastra ships this; we want it
  but it is its own ADR.
- A `#[derive(OrkTool)]` proc macro could collapse the builder
  syntax further (annotated `async fn` becomes a `Tool<I, O>` via
  argument and return-type inference). Out of scope.
- The `gate` predicate is synchronous to keep tool-list
  computation cheap; an `async_gate` variant for "is this tool
  available right now in this tenant" remote checks is a future
  ADR if needed.

## Alternatives considered

- **Keep hand-written JSON Schemas.** Rejected. ADR 0011's
  `ToolDescriptor` is the source of the duplication this ADR
  removes. Mastra's `createTool` and rig's typed `Tool` both
  validate that the typed-input shape is the right level.
- **Use `oneof`/`schemars`-only without rig integration.**
  Rejected. ADR 0047 already bought rig as the engine; tools are
  the natural place rig and ork meet at the type level. Keeping
  the rig bridge separate from ork's typed-input surface
  duplicates work.
- **Procedural macro with attribute syntax (`#[tool]` on an `async
  fn`).** Rejected for v1. The closure-with-typestate version is
  enough; macros are a syntax sugar follow-up. Macros also fight
  with IDE completion in many cases; the builder shape stays
  legible.
- **A per-vendor tool surface (one builder per LLM provider's
  tool calling spec).** Rejected. ADR
  [`0012`](0012-multi-llm-providers.md) standardises on
  OpenAI-compatible tool calling; rig and ork both produce a
  single normalised shape.

## Affected ork modules

- New: [`crates/ork-tool/`](../../crates/) — the builder crate.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/) —
  `ToolDef` port (already in 0049's port set).
- [`crates/ork-agents/src/tool_catalog.rs`](../../crates/ork-agents/src/tool_catalog.rs)
  — reshape to consume `Box<dyn ToolDef>` from `OrkApp`.
- [`crates/ork-agents/src/rig_engine.rs`](../../crates/ork-agents/src/)
  — `OrkToolDyn` (from ADR 0047) updated to wrap the new
  `Tool<I, O>` directly via the rig adapter.
- [`crates/ork-mcp/`](../../crates/ork-mcp/) — surfaces MCP tools
  as `Box<dyn ToolDef>` (no schema-gen change; the schema arrives
  on the wire).
- Native tools (`agent_call`, `peer_*`, code-tools) — re-authored
  with the new builder. Specific files listed in the
  implementation TODO list.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [`createTool`](https://mastra.ai/reference/tools/create-tool) | `tool().input::<I>().output::<O>().execute(...)` |
| Mastra | [Dynamic tools / `mastra` parameter](https://mastra.ai/docs/agents/dynamic-agents) | `.gate(predicate)` and `ToolContext` |
| rig | [`rig::tool::Tool`](https://docs.rs/rig-core/latest/rig/tool/index.html) | `Tool<I, O>` impl through rig adapter |
| rig | [`AgentBuilder::tool`](https://docs.rs/rig-core/latest/rig/agent/struct.AgentBuilder.html) | the agent-side consumption surface (ADR 0052) |
| Solace Agent Mesh | YAML tool registration in agent config | replaced by Rust registration |

## Open questions

- **`schemars` schema stability.** schemars produces stable but
  *not byte-identical* output across minor releases (titles,
  descriptions, ordering). The parity test pins schemars to the
  workspace version; cross-version drift is documented.
- **`rig::tool::Tool` arg shape.** rig 0.36's `ToolDyn::call`
  takes `String`; we deserialise inside the adapter. If a future
  rig release changes that surface, the adapter shifts but the
  user-visible builder does not.
- **MCP-as-server export.** ADR 0048 lists this as a future ADR.
  The cleanest shape is "0061: every registered native `Tool<I,
  O>` is exposed over MCP if the user opts in" — concrete
  schema mapping deferred.
- **Async predicates.** `.gate(predicate)` is sync. If a tenant
  capability check requires a DB hit, the tool descriptor
  computation has to skip awaiting in the synchronous step. We
  cache capabilities at request entry; documented limitation.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — registry.
- ADR [`0010`](0010-mcp-tool-plane.md) — MCP plane (unchanged).
- ADR [`0011`](0011-native-llm-tool-calling.md) — superseded by
  this ADR's authoring shape; the trait surface 0011 introduced
  is unaffected (LLM-visible JSON shape).
- ADR [`0047`](0047-rig-as-local-agent-engine.md) — engine and the
  `OrkToolDyn` mechanism this ADR feeds.
- Mastra `createTool`: <https://mastra.ai/reference/tools/create-tool>
- rig `Tool`: <https://docs.rs/rig-core/latest/rig/tool/index.html>
- schemars: <https://docs.rs/schemars/>
