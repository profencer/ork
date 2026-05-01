# 0053 — Memory: working memory + semantic recall, threads and resources

- **Status:** Proposed
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0011, 0015, 0016, 0048, 0049, 0050, 0052, 0054
- **Supersedes:** 0032, 0043

## Context

Today an ork agent's "memory" is whatever the caller passes in
through `AgentContext` and the message history that
[`LocalAgent`](../../crates/ork-agents/src/local.rs) maintains for
the duration of one tool-calling loop. Across loops there is no
durable user memory. ADR
[`0032`](0032-agent-memory-and-context-compaction.md) (Proposed,
superseded) sketched a token-window-based compaction memory; ADR
[`0043`](0043-team-shared-memory.md) (Proposed, superseded) sketched
a multi-agent shared decision log. Neither shipped.

Mastra's
[Memory](https://mastra.ai/docs/memory/overview) is the canonical
shape. It has two surfaces:

- **Working memory** — durable, structured per-resource (per-user)
  state, written to on agent turns and read back across threads.
  ("name", "preferences", "goals" — not message history.)
- **Semantic recall** — vector-indexed past message bodies that the
  agent can retrieve by similarity for context priming.

Threads (`threadId`) isolate conversations; resources (`resourceId`)
own threads. Both default to `scope: 'resource'`, meaning working
memory is shared across threads owned by the same resource.

rig provides the underlying primitives: an
[`EmbeddingModel`](https://docs.rs/rig-core/latest/rig/embeddings/index.html)
trait, `EmbeddingsBuilder`, and companion crates (`rig-postgres`,
`rig-qdrant`, etc.) for vector stores. ork's
[`ork-persistence`](../../crates/ork-persistence/) crate already
runs Postgres; pgvector or libsql (Mastra's default) are both
plausible v1 backends.

## Decision

ork **introduces a `Memory` port and a default `Memory::libsql`
implementation**, registered with `OrkApp::builder().memory(...)`
and consumed by every `CodeAgent` (ADR 0052) by default unless the
agent opts out. The shape mirrors Mastra's working-memory plus
semantic-recall split.

```rust
use ork_memory::{Memory, MemoryOptions, WorkingMemoryShape};

let memory = Memory::libsql("file:./ork.db")
    .options(MemoryOptions {
        last_messages: 20,
        semantic_recall: SemanticRecallConfig {
            enabled: true,
            top_k: 6,
            scope: Scope::Resource,
            embedder: EmbedderSpec::from("openai/text-embedding-3-small"),
        },
        working_memory: Some(WorkingMemoryShape::User),
    })
    .open()
    .await?;

let app = OrkApp::builder()
    .memory(memory)
    .agent(weather_agent())
    .build()?;
```

### Trait surfaces

```rust
// crates/ork-memory/src/lib.rs (port lives in ork-core)
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn append_message(&self, ctx: &MemoryContext, msg: ChatMessage)
        -> Result<MessageId, OrkError>;
    async fn last_messages(&self, ctx: &MemoryContext, limit: usize)
        -> Result<Vec<ChatMessage>, OrkError>;

    async fn working_memory(&self, ctx: &MemoryContext)
        -> Result<Option<serde_json::Value>, OrkError>;
    async fn set_working_memory(&self, ctx: &MemoryContext, v: serde_json::Value)
        -> Result<(), OrkError>;

    async fn semantic_recall(&self, ctx: &MemoryContext, query: &str, top_k: usize)
        -> Result<Vec<RecallHit>, OrkError>;

    async fn list_threads(&self, resource_id: &ResourceId)
        -> Result<Vec<ThreadSummary>, OrkError>;
    async fn delete_thread(&self, ctx: &MemoryContext)
        -> Result<(), OrkError>;
}

pub struct MemoryContext {
    pub tenant_id: TenantId,         // ADR 0020
    pub resource_id: ResourceId,     // user / org / device
    pub thread_id: ThreadId,         // conversation
    pub agent_id: AgentId,
}
```

### Reference implementation: `Memory::libsql`

`Memory::libsql(url)` opens a libsql / sqlite database with three
tables (full migration in
[`migrations/`](../../migrations/)):

- `mem_messages(tenant_id, resource_id, thread_id, agent_id,
  message_id, role, parts JSONB, created_at)` — chat history.
- `mem_working(tenant_id, resource_id, agent_id, value JSONB,
  updated_at)` — structured per-resource state. The `agent_id` is
  optional (NULL = shared across agents).
- `mem_embeddings(tenant_id, resource_id, thread_id, message_id,
  embedding VECTOR, content TEXT, created_at)` — semantic-recall
  index.

For Postgres deployments, a parallel
`Memory::postgres(pg_pool)` opens the same schema in the existing
[`crates/ork-persistence/`](../../crates/ork-persistence/) Postgres
backend with `pgvector` for embeddings.

### Working memory shapes

```rust
pub enum WorkingMemoryShape {
    /// Free-form JSON. The agent reads/writes arbitrary keys.
    Free,
    /// Schema-constrained. The agent's working-memory writes are
    /// validated against the schema; reads are typed.
    Schema(JsonSchema),
    /// Pre-baked common shape: name, preferences, goals.
    User,
}
```

The `Schema(...)` variant is what ADR 0032 was reaching for; the
`User` shape is the default Mastra ships.

### Semantic-recall scope

Mastra's `scope: 'resource'` (default) shares semantic recall
across threads owned by the same resource; `scope: 'thread'`
isolates. We adopt the same vocabulary:

```rust
pub enum Scope { Resource, Thread, Tenant }
```

`Scope::Tenant` is ork's addition for the multi-team-shared-memory
case ADR 0043 was after — the recall set is the union of all
resources within a tenant. Used cautiously; defaults to `Resource`.

### Embedder selection

`EmbedderSpec::from("openai/text-embedding-3-small")` flows into
ADR [`0012`](0012-multi-llm-providers.md)'s router — the same
single-point selection ork already does for chat. Embedding models
are first-class providers; rig's
[`EmbeddingsBuilder`](https://docs.rs/rig-core/latest/rig/embeddings/index.html)
is what does the work under the hood, but ork owns the routing.

### Integration with `CodeAgent`

By default, every `CodeAgent` registered with `OrkApp` gets the
registered `MemoryStore` injected. The agent's prompt assembly
prepends:

1. The agent's static `instructions`.
2. Working-memory snapshot for `(tenant, resource, agent)` (if
   present).
3. Top-K semantic-recall hits for the current user message.
4. Last-N messages from the current thread.
5. The current user message.

Each piece is opt-out per agent via
`.memory_options(MemoryOptions { include_working: false, ... })`.

The agent's tool list automatically includes a built-in
`memory.update_working` tool (a `Tool<I, ()>` from ADR 0051) so the
LLM can write to working memory. Mastra surfaces this as a
[built-in tool](https://mastra.ai/docs/memory/overview); we mirror.

## Acceptance criteria

- [ ] New crate `crates/ork-memory/` with `Cargo.toml` declaring
      `ork-core`, `ork-common`, `serde`, `schemars`, `tokio`,
      `futures`, `libsql`. No `axum`/`reqwest`/`rmcp`/`rskafka`.
- [ ] `MemoryStore` port defined at
      `crates/ork-core/src/ports/memory.rs` with the surface
      shown in `Decision`.
- [ ] `Memory::libsql(url) -> MemoryBuilder` exported from
      `crates/ork-memory/src/lib.rs` with the chain shown in
      `Decision`.
- [ ] `Memory::postgres(pg_pool) -> MemoryBuilder` exported from
      [`crates/ork-persistence/`](../../crates/ork-persistence/).
- [ ] Migration `migrations/NNNN_memory_tables.sql` adds
      `mem_messages`, `mem_working`, `mem_embeddings` with the
      indices required for the queries above. pgvector extension
      enabled if Postgres backend.
- [ ] Integration test `crates/ork-memory/tests/working_memory.rs`
      covers (a) write/read round-trip, (b) per-tenant isolation,
      (c) `Schema` validation rejects out-of-shape writes.
- [ ] Integration test
      `crates/ork-memory/tests/semantic_recall.rs` covers (a)
      embed-store-retrieve round-trip with a deterministic mock
      embedder; (b) `Scope::Resource` shares hits across threads;
      (c) `Scope::Thread` isolates.
- [ ] `CodeAgent` (ADR 0052) consumes the registered `MemoryStore`
      automatically; integration test
      `crates/ork-agents/tests/code_agent_memory.rs` covers the
      "agent remembers user's name across threads" pattern.
- [ ] Built-in `memory.update_working` tool is automatically
      attached to agents that have memory enabled; verified by
      reading the descriptor list in the test above.
- [ ] `OrkApp::builder().memory(...)` registers the store; agents
      without explicit `.memory(...)` opt-out inherit it.
- [ ] CI grep: no file under `crates/ork-memory/` imports `axum`,
      `reqwest`, `rmcp`, or `rskafka`.
- [ ] [`README.md`](README.md) ADR index row added; ADRs 0032 and
      0043 status flipped to `Superseded by 0053`.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- Working memory and semantic recall both ship in v1. Today an ork
  agent has neither; the gap with Cursor / Claude Code / Mastra
  on the "remembers what the user said yesterday" axis closes.
- Per-tenant isolation is enforced at the schema level (every
  table keyed by `tenant_id`); ADR
  [`0020`](0020-tenant-security-and-trust.md)'s invariants extend
  cleanly to memory.
- `MemoryStore` as a port means a customer with their own vector
  DB (Pinecone, Qdrant) can plug it in. The libsql default is
  zero-config; the Postgres variant matches existing ork
  deployments.
- Built-in `memory.update_working` tool means the LLM writes its
  own structured state without per-agent boilerplate.

### Negative / costs

- Embeddings cost real money on every user turn (one embedding
  call per query). Documented; `top_k` and a cosine-similarity
  threshold both default conservative.
- The dual backend (libsql for dev, Postgres for production) means
  two code paths to maintain. Mitigation: the trait surface is
  the same; backend differences are inside the impls.
- Memory contents are user data. ADR 0020 (security) defines
  retention and encryption; this ADR's tables join that policy at
  schema time but full encryption is its own ADR.
- `Scope::Tenant` (the ADR 0043 use case) is the most leaky shape
  — multi-resource semantic recall can leak between users on the
  same tenant. We implement it but document the warning loudly.

### Neutral / follow-ups

- Embedding-cost optimisation (caching, chunked-write batching)
  arrives in a follow-up ADR if customer load demands it.
- A `MemoryViewer` Studio panel (ADR 0055) shows working memory
  and semantic-recall hits. The UI is in 0055; the port is here.
- A `MemoryEvent` stream (Kafka, per ADR 0004) for "memory was
  written" is a future ADR — useful for dashboards and audit.
- Mastra's `memory.delete_thread` is the right surface for GDPR
  deletion; we ship it; a fuller GDPR compliance ADR is
  downstream.

## Alternatives considered

- **Use only chat history; no working or semantic memory.**
  Rejected. ADR 0032 and 0043 already established the gap; pivot
  ADR 0048 makes Mastra parity load-bearing.
- **Adopt a single concept (working memory) and skip semantic
  recall in v1.** Rejected. Semantic recall is the cheaper of
  the two to ship (rig provides the embeddings primitive) and
  the one users notice. Working memory is the operationally
  harder one; both ship together.
- **Use rig's `VectorStoreIndex` + companion crate as the public
  surface.** Rejected. The companion crates (`rig-qdrant`,
  `rig-postgres`) lock the ork user into rig's specific vector
  store, fight the ork-persistence pool, and bypass tenant
  scoping. We use rig for the embedding model only; the storage
  is ork-shaped.
- **One unified `Memory` per agent.** Rejected. Mastra's
  per-resource shape is what makes "the agent remembers across
  conversations" work; binding memory to a single agent
  duplicates state.
- **Skip libsql; require Postgres.** Rejected on developer
  experience. `mastra dev` is one command and runs against a
  local sqlite-shaped store; ork should match.

## Affected ork modules

- New: [`crates/ork-memory/`](../../crates/) — libsql backend +
  port re-export.
- [`crates/ork-core/src/ports/memory.rs`](../../crates/ork-core/src/)
  — `MemoryStore` trait.
- [`crates/ork-persistence/`](../../crates/ork-persistence/) —
  Postgres backend + pgvector migration.
- [`crates/ork-agents/src/code_agent.rs`](../../crates/ork-agents/src/)
  — memory injection + built-in `memory.update_working` tool.
- [`crates/ork-llm/`](../../crates/ork-llm/) — embedder routing
  via `LlmRouter` (one-line addition to the resolution chain).
- [`migrations/`](../../migrations/) — three new tables.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Memory overview](https://mastra.ai/docs/memory/overview) | `Memory::libsql` / `Memory::postgres` |
| Mastra | working memory vs semantic recall | `WorkingMemoryShape` + `SemanticRecallConfig` |
| Mastra | thread / resource scoping | `ThreadId` / `ResourceId` / `Scope` |
| rig | [`EmbeddingsBuilder`](https://docs.rs/rig-core/latest/rig/embeddings/index.html) | embedder integration via `LlmRouter` |
| LangGraph | `Store` for per-user memory | informative — same shape |
| Solace Agent Mesh | no first-class memory | n/a |

## Open questions

- **Schema migrations for `WorkingMemoryShape::Schema`.** When the
  user evolves the schema, what happens to existing rows? Default
  v1: validation runs on read; rows that fail are surfaced as
  `MemoryError::Stale` and the agent decides. Better behaviour
  (auto-migrate via patch tool) deferred.
- **Vector store choice.** libsql + a vector extension vs sqlite
  + sqlite-vss vs pgvector for Postgres. Defaults: pgvector for
  Postgres, libsql native vector type for libsql.
- **Privacy / encryption at rest.** ADR 0020 owns encryption.
  Memory inherits the same policy — column-level encryption for
  PII fields, key per tenant.
- **Cross-agent working memory.** Mastra defaults to
  per-resource (shared across agents owned by the same user).
  We default the same way. ADR 0043's use case (cross-agent
  shared decision log) is an opt-in `Scope::Tenant` plus an
  agent-id-NULL row.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — registry.
- ADR [`0052`](0052-code-first-agent-dsl.md) — agent DSL that
  consumes memory.
- ADR [`0012`](0012-multi-llm-providers.md) — embedder routing.
- ADR [`0020`](0020-tenant-security-and-trust.md) — encryption /
  retention.
- Mastra memory: <https://mastra.ai/docs/memory/overview>
- rig embeddings:
  <https://docs.rs/rig-core/latest/rig/embeddings/index.html>
