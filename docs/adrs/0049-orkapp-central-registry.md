# 0049 — `OrkApp` central registry: code-first project entry point

- **Status:** Proposed
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0002, 0003, 0008, 0017, 0048, 0050, 0051, 0052, 0053, 0054, 0055, 0056, 0057
- **Supersedes:** —

## Context

Today an ork deployment is composed by editing TOML/YAML in
[`workflow-templates/`](../../workflow-templates/) and wiring agents
through procedural setup in [`crates/ork-cli/`](../../crates/ork-cli/)
and the demo scripts under [`demo/`](../../demo/). The `dyn Agent`
implementations themselves live in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
behind ad-hoc factory functions; tools are registered via the
[`ToolExecutor`](../../crates/ork-core/src/workflow/engine.rs)
catalog through whatever code path each gateway happens to take. There
is no single Rust value that *holds the application's shape*: agents,
workflows, tools, MCP server descriptors, memory backend, vector
store, observability config. New contributors have to read three
crates and the demo to figure out where to add a thing.

Mastra's
[`new Mastra({ agents, workflows, tools, mcpServers, storage,
vectors, observability, server, scorers })`](https://mastra.ai/reference/core/mastra-class)
is a single object that holds all of that. Every other surface in
Mastra reads off it: `mastra dev` reflects it into HTTP routes,
Studio reflects it into UI panes, scorers attach to its agents,
storage migrations key off it. ork has the same components but
spread across crates, so we cannot point a Studio-equivalent or a
"tell me what this app does" introspector at one value.

The pivot ADR ([`0048`](0048-pivot-to-code-first-rig-platform.md))
adopts a Mastra-shaped code-first surface. The first brick is the
central registry; without it none of 0050–0057 has a thing to attach
to.

## Decision

ork **introduces `OrkApp`**, the single Rust value that registers
every component of an ork deployment and exposes them by id. It is
the user's project entry point and the authoritative source every
other 0049–0057 surface reads from. The companion crate is new:
[`crates/ork-app/`](../../crates/) (`ork-app`), under hexagonal rules
from [`AGENTS.md`](../../AGENTS.md) §3 (no `axum`, `sqlx`, `reqwest`,
`rmcp`, `rskafka` imports — those flow through ork-existing crates'
ports).

```rust
// crates/ork-app/src/lib.rs
use std::sync::Arc;

use ork_core::ports::agent::Agent;
use ork_core::a2a::AgentCard;
use ork_common::{TenantId, OrkError};

pub struct OrkApp {
    inner: Arc<OrkAppInner>,
}

pub struct OrkAppBuilder {
    agents: Vec<Arc<dyn Agent>>,
    workflows: Vec<Arc<dyn WorkflowDef>>,         // ADR 0050
    tools: Vec<Arc<dyn ToolDef>>,                 // ADR 0051
    mcp_servers: Vec<McpServerSpec>,              // ADR 0010 + 0051
    memory: Option<Arc<dyn MemoryStore>>,         // ADR 0053
    storage: Option<Arc<dyn KvStorage>>,
    vectors: Option<Arc<dyn VectorStore>>,        // ADR 0053 + 0054
    scorers: Vec<ScorerBinding>,                  // ADR 0054
    observability: Option<ObservabilityConfig>,   // ADR 0058 (future)
    server: ServerConfig,                         // ADR 0056
    request_context_schema: Option<JsonSchema>,   // ADR 0052
    id_generator: Option<Arc<dyn IdGenerator>>,
    environment: Environment,
}

impl OrkApp {
    pub fn builder() -> OrkAppBuilder { OrkAppBuilder::default() }

    pub fn agent(&self, id: &str) -> Option<Arc<dyn Agent>>;
    pub fn workflow(&self, id: &str) -> Option<Arc<dyn WorkflowDef>>;
    pub fn tool(&self, id: &str) -> Option<Arc<dyn ToolDef>>;
    pub fn agents(&self) -> impl Iterator<Item = (&str, &Arc<dyn Agent>)>;
    pub fn workflows(&self) -> impl Iterator<Item = (&str, &Arc<dyn WorkflowDef>)>;
    pub fn tools(&self) -> impl Iterator<Item = (&str, &Arc<dyn ToolDef>)>;
    pub fn memory(&self) -> Option<&Arc<dyn MemoryStore>>;
    pub fn storage(&self) -> Option<&Arc<dyn KvStorage>>;
    pub fn vectors(&self) -> Option<&Arc<dyn VectorStore>>;
    pub fn agent_cards(&self) -> impl Iterator<Item = &AgentCard>;

    /// Cheap structural snapshot for introspection (Studio, OpenAPI,
    /// `ork inspect`). Does not include runtime state.
    pub fn manifest(&self) -> AppManifest;
}

impl OrkAppBuilder {
    pub fn agent<A: Agent + 'static>(self, a: A) -> Self;
    pub fn workflow<W: WorkflowDef + 'static>(self, w: W) -> Self;
    pub fn tool<T: ToolDef + 'static>(self, t: T) -> Self;
    pub fn mcp_server(self, id: impl Into<String>, spec: McpServerSpec) -> Self;
    pub fn memory<M: MemoryStore + 'static>(self, m: M) -> Self;
    pub fn storage<S: KvStorage + 'static>(self, s: S) -> Self;
    pub fn vectors<V: VectorStore + 'static>(self, v: V) -> Self;
    pub fn scorer(self, target: ScorerTarget, scorer: ScorerSpec) -> Self;
    pub fn observability(self, cfg: ObservabilityConfig) -> Self;
    pub fn server(self, cfg: ServerConfig) -> Self;
    pub fn request_context_schema(self, s: JsonSchema) -> Self;
    pub fn id_generator<G: IdGenerator + 'static>(self, g: G) -> Self;
    pub fn environment(self, e: Environment) -> Self;

    pub fn build(self) -> Result<OrkApp, OrkError>;
}
```

### Wiring rule (hexagonal boundary)

`OrkApp` is built in `ork-app`, a crate that depends on
[`ork-core`](../../crates/ork-core/) ports only. Concrete adapters
(rig agents from 0052, libsql memory from 0053, axum router from
0056) come from sibling crates and are passed *into* the builder by
the user, who is the place where the hexagonal boundary is allowed
to be crossed (per [`AGENTS.md`](../../AGENTS.md) §3 — `ork-core` and
`ork-agents` cannot import infra crates; the user's `main.rs` can).

```rust
// User's src/main.rs
use ork_app::OrkApp;
use ork_agents::CodeAgent;            // ADR 0052
use ork_persistence::LibsqlMemory;    // ADR 0053

let app = OrkApp::builder()
    .agent(CodeAgent::builder("weather")
        .instructions("You report the weather.")
        .model("openai/gpt-4o-mini")
        .tool(weather_tool())
        .build()?)
    .memory(LibsqlMemory::open("file:./ork.db").await?)
    .build()?;
```

### Naming and id rules

- Every registered component has an `id: String` that uniquely
  identifies it within its category. `OrkAppBuilder::build()` returns
  `Err(OrkError::Configuration { .. })` on duplicate id within a
  category; ids may collide *across* categories (e.g., a tool and an
  agent both named `"weather"` is allowed because their lookup
  surfaces are disjoint).
- Ids must match `^[a-z0-9][a-z0-9-]{0,62}$`. This is the same charset
  Kong route names accept, the same shape `ToolDescriptor.name`
  already enforces in
  [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs),
  and the same shape Studio (0055) and the auto-generated REST
  surface (0056) need for path safety.
- An agent's A2A `AgentCard.name` derives from `agent.id()`; A2A
  remote callers continue to address by name as in ADR
  [`0005`](0005-agent-card-and-devportal-discovery.md).

### `AppManifest` — the introspection lens

```rust
#[derive(Clone, Serialize)]
pub struct AppManifest {
    pub environment: Environment,
    pub agents: Vec<AgentSummary>,
    pub workflows: Vec<WorkflowSummary>,
    pub tools: Vec<ToolSummary>,
    pub mcp_servers: Vec<McpServerSummary>,
    pub memory: Option<MemorySummary>,
    pub vectors: Option<VectorStoreSummary>,
    pub scorers: Vec<ScorerSummary>,
    pub server: ServerSummary,
    pub built_at: DateTime<Utc>,
    pub ork_version: String,
}
```

The manifest is the answer to "what does this app expose?" — the
single value Studio reads (0055) and the one ADR 0056 walks to emit
OpenAPI, the one `ork inspect` (ADR 0057) prints. It is **not** a
runtime snapshot; it does not include task state, memory contents,
or workflow run state.

### Composition with ADR 0002 `Agent` port

`OrkAppBuilder::agent` accepts anything that implements ork's
existing `Agent` port (`crates/ork-core/src/ports/agent.rs`). That
includes today's `LocalAgent`, the upcoming `CodeAgent` from ADR
0052, and any user `impl Agent for MyAgent`. The pivot does not
introduce a parallel agent abstraction.

This means an ork project can mix code-first agents (declared via
0052's builder) and hand-written `dyn Agent` implementations in the
same `OrkApp::builder()` chain. The shape difference is an
ergonomic one for the *common* case; the port is the *contract*.

### Server lifecycle

```rust
impl OrkApp {
    /// Start the auto-generated REST/SSE server (ADR 0056) and
    /// optionally Studio (ADR 0055). Resolves once the server is
    /// listening; the returned handle can be used to shut down.
    pub async fn serve(&self) -> Result<ServeHandle, OrkError>;

    /// Run a single agent generation/stream programmatically without
    /// going through the HTTP surface. The same path the REST
    /// handlers use; useful for tests and embedded usage.
    pub async fn run_agent(
        &self,
        agent_id: &str,
        ctx: AgentContext,
        prompt: ChatMessage,
    ) -> Result<AgentEventStream, OrkError>;

    /// Run a workflow programmatically. Same shape; the ADR 0050
    /// engine reads the registered tools/agents off `&self`.
    pub async fn run_workflow(
        &self,
        workflow_id: &str,
        ctx: AgentContext,
        input: serde_json::Value,
    ) -> Result<WorkflowRunHandle, OrkError>;
}
```

`serve()` is the single entry point that ADRs 0056 (server) and 0057
(`ork dev`) target. It is also the binary the production deployment
uses (`ork start` after `ork build`).

## Acceptance criteria

- [ ] New crate `crates/ork-app/` exists with `Cargo.toml` declaring
      dependencies on `ork-core`, `ork-common` only (no `axum`,
      `sqlx`, `reqwest`, `rmcp`, `rskafka` direct deps). Verified by
      `cargo metadata` inspection in CI.
- [ ] `OrkApp`, `OrkAppBuilder`, `AppManifest` defined at
      `crates/ork-app/src/lib.rs` with the signatures shown in
      `Decision`.
- [ ] `OrkAppBuilder::build()` returns
      `Err(OrkError::Configuration { .. })` on (a) duplicate id
      within a category, (b) id failing the
      `^[a-z0-9][a-z0-9-]{0,62}$` regex, (c) a workflow that
      references an agent or tool id not registered in the same
      builder.
- [ ] `OrkApp::manifest()` round-trips through
      `serde_json::to_value` / `from_value` losslessly. Verified by a
      property test in `crates/ork-app/tests/manifest_roundtrip.rs`.
- [ ] `OrkApp::serve()` is implemented (in this ADR's diff) as a
      stub that launches an `axum` server provided by ADR 0056. The
      stub must accept a `ServerConfig` (host, port, tls, auth) and
      return a `ServeHandle` with a `shutdown()` method that
      gracefully stops the server within `Duration::from_secs(5)`.
- [ ] `OrkApp::run_agent` and `OrkApp::run_workflow` are implemented
      against the existing `Agent` port and ADR 0050's engine
      respectively; both honour `AgentContext::cancel`.
- [ ] Integration test
      `crates/ork-app/tests/builder_smoke.rs` covers: (a) building
      an app with two agents, one tool, one workflow, calling
      `manifest()` and asserting all three appear; (b) duplicate id
      rejected; (c) malformed id rejected; (d) workflow referencing
      an unregistered tool rejected.
- [ ] Integration test `crates/ork-app/tests/serve_smoke.rs` boots
      `OrkApp::serve()` against an ephemeral port, hits
      `/healthz`, asserts 200 OK, then `shutdown()` and asserts the
      socket closes within 5 s.
- [ ] No file under `crates/ork-app/` imports `axum`, `sqlx`,
      `reqwest`, `rmcp`, or `rskafka` (CI grep).
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- One Rust value answers "what is this ork project?" — the same
  question Studio (0055), the auto-generated server (0056),
  `ork inspect` (0057), and the OpenAPI emitter need an answer for.
- The hexagonal boundary clarifies. Today the question "where do I
  add a new thing?" is "find a similar thing and copy the pattern";
  after this ADR it is "implement the trait, pass it to
  `OrkApp::builder()`."
- Tests can build a complete app in one expression. Today the
  workflow integration tests stand up partial fixtures piece-by-piece;
  with `OrkApp` they construct one and then exercise the surfaces
  off it.
- The shape composes with ADR
  [`0007`](0007-remote-a2a-agent-client.md): a remote A2A agent is
  just another `dyn Agent` registered with the same builder
  method. No new code path.

### Negative / costs

- New crate adds a workspace member and a small chunk of build time.
  Mitigation: `ork-app` is leaf-shaped; it depends on `ork-core` +
  `ork-common`, and other crates do not depend on it (the user's
  binary does).
- One more Arc dance. Every registered component is `Arc`-wrapped;
  the builder takes ownership and stores `Arc<dyn ...>`. This is
  consistent with ork's existing
  [`Arc<dyn Agent>`](../../crates/ork-core/src/ports/agent.rs)
  patterns but adds an indirection vs holding values directly.
- The builder's id-collision check at `build()` time fails late.
  Users typing in an editor without `cargo check` won't see the
  error until they run. Mitigation: a `#[derive(OrkAgent)]` /
  `#[derive(OrkTool)]` macro family in a future ADR can move some
  checks to compile time. Out of scope here.
- The `ServeHandle` shape commits to graceful shutdown. If the
  underlying axum 0.8 surface ever changes the shape we propagate,
  this becomes a breaking change in `ork-app`. Mitigation: the
  signature returns `OrkError`, so we have one pre-existing error
  type to thread new variants through.

### Neutral / follow-ups

- `OrkApp` becomes the natural place to inject the `LlmRouter`
  ([`0012`](0012-multi-llm-providers.md)) once 0052's `CodeAgent`
  ships; today the router is held in
  [`crates/ork-llm/`](../../crates/ork-llm/) and threaded through
  agent constructors.
- `OrkApp::manifest()` is the right surface for ADR
  [`0005`](0005-agent-card-and-devportal-discovery.md)'s DevPortal
  publish step to read off — registration becomes "publish my
  manifest", deregistration "publish empty manifest." Future ADR.
- Hot reload (ADR 0057's `ork dev`) wraps the builder in a
  rebuild-on-file-change loop; the hot-reload contract is "swap
  the inner `Arc<OrkAppInner>` atomically", not "patch the running
  agent in place." Defines the contract early.
- A future ADR may make `OrkApp` a generic over a transport (in-
  process vs Kong-fronted) so the test surface and the production
  surface share one builder. Not needed for 0049's acceptance.

## Alternatives considered

- **Skip the registry; let users wire crates directly.** Rejected.
  This is the status quo and it produced the cognitive cost
  documented in `Context`. Mastra's success specifically rests on
  the central-registration shape.
- **Use a configuration file (`ork.toml`) instead of Rust
  registration.** Rejected. We tried YAML
  ([`workflow-templates/`](../../workflow-templates/)) and it
  produced a separate type system the developer has to learn,
  decoupled from the type-checker. Mastra is TypeScript code-first;
  Rust code-first is a strict ergonomic upgrade because the
  compiler catches refactors.
- **Make `OrkApp` a trait, not a struct.** Rejected. The trait
  shape buys flexibility nobody is asking for and forces
  every consumer (Studio, server, CLI) to be generic over `App`.
  The struct shape is what Mastra ships and the cleanest match
  for the introspection use case.
- **Put the registry inside `ork-core`.** Rejected on the hexagonal
  boundary. `ork-core` defines ports; the user's `main.rs` is
  where adapter values are constructed; `ork-app` is the seam
  between them. Putting the registry in `ork-core` would invite
  every adapter type into `ork-core`'s dependency graph.
- **One global `OrkApp` (singleton).** Rejected. Tests need
  multiple apps; embedding ork in a larger Rust application means
  the host owns the lifecycle, not us. The builder returns a
  value; users hold it where they like.

## Affected ork modules

- New: [`crates/ork-app/`](../../crates/) — this ADR creates it.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/) — if
  any new port traits are needed (e.g., `MemoryStore`,
  `VectorStore`, `KvStorage`, `WorkflowDef`, `ToolDef`,
  `IdGenerator`), they land here. ADRs 0050–0054 each ship the
  port relevant to their surface; 0049 only depends on the trait
  *bounds* existing.
- [`crates/ork-cli/`](../../crates/ork-cli/) — `ork dev` /
  `ork start` consume `OrkApp::serve()` (ADR 0057).
- [`crates/ork-api/`](../../crates/ork-api/) — auto-generated REST
  surface reads off `&OrkApp` (ADR 0056).
- User's `main.rs` (in template projects) — single registration
  block.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [`new Mastra({...})` reference](https://mastra.ai/reference/core/mastra-class) | `OrkApp::builder().build()` |
| Mastra | [project layout `src/mastra/index.ts`](https://mastra.ai/docs/getting-started/project-structure) | user's `src/main.rs` building `OrkApp` |
| Solace Agent Mesh | per-agent `SamAgentComponent` registration in YAML configs | replaced by central Rust registration |
| LangGraph | `StateGraph` per-graph + ad-hoc registration | superseded — ork registers across the app, not per-graph |

## Open questions

- **Crate name.** `ork-app` vs `ork-runtime` vs `ork`. `ork` (the
  bare name) is taken by the workspace meta-crate (or could be
  re-shaped to be it). Default in this ADR: `ork-app`. Will
  finalise during implementation.
- **Type-erased vs typed registry.** Today the surfaces are typed
  (`Arc<dyn Agent>`, `Arc<dyn ToolDef>`). A future ADR may add
  typed accessors keyed by const-generic name (à la
  `axum::Extension<T>`) — useful for tests where you want
  `app.agent::<WeatherAgent>()` and not the trait-object form. Out
  of scope for 0049.
- **Per-tenant `OrkApp`.** ADR
  [`0020`](0020-tenant-security-and-trust.md) implies an
  application-per-tenant or a tenant-aware app. The current shape
  is single-tenant; multi-tenant scoping should be handled inside
  registered components (each agent has tenant-aware behaviour
  via `AgentContext`), not by spawning N apps. Confirm during 0020
  implementation.
- **Hot-reload boundary.** When `ork dev` (0057) detects a file
  change, does it rebuild only the changed component or the whole
  `OrkApp`? Default in this ADR: rebuild the whole app and swap;
  components must therefore not own external sockets directly
  (use ports). 0057 owns the operational details.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0002`](0002-agent-port.md) — `Agent` port (unchanged).
- Mastra `Mastra` class:
  <https://mastra.ai/reference/core/mastra-class>
- Mastra project layout:
  <https://mastra.ai/docs/getting-started/project-structure>
- Mastra server overview:
  <https://mastra.ai/docs/server/mastra-server>
