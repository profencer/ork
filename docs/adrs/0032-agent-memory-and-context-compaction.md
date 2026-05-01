# 0032 — Agent memory and context compaction

- **Status:** Superseded by 0053
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0002, 0003, 0011, 0012, 0020, 0022
- **Supersedes:** —

## Context

A2A messages already give per-task conversation history (ADR [`0003`](0003-a2a-protocol-model.md)): every `Task` carries a `Vec<Message>` and the local agent loop in [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) replays those messages into a [`ChatRequest`](../../crates/ork-core/src/ports/llm.rs) on every iteration. That is sufficient for short request/response tasks. For long-running coding agents — the workload introduced by ADRs [`0028`](0028-shell-executor-and-test-runners.md), [`0029`](0029-workspace-file-editor.md), [`0030`](0030-git-operations.md), and [`0031`](0031-transactional-code-changes.md) — two gaps remain:

1. **Token-budget management within a task.** The agent loop in [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) clones the full `history` (line 344) into every `ChatRequest`, including all prior tool-result messages. A multi-hour coding session will overflow the model's context window long before it hits `config.max_tool_iterations`. There is no token estimator, no compaction step, and no signal that compaction happened. ADR [`0011`](0011-native-llm-tool-calling.md) noted "tool loop telemetry" as a `TODO(ADR-0022)` but said nothing about the size of the loop's working set.
2. **Cross-task semantic recall.** Every new A2A `Task` starts with an empty history. An agent that learned "this repo uses tokio with `full` features" or "the integration tests live under `crates/ork-x/tests/`" relearns it next session. SAM has no first-class concept here; ADR [`0002`](0002-agent-port.md) deliberately kept the `Agent` port stateless and the per-task `Context` ephemeral. We need a tenant-scoped, agent-scoped store that survives across tasks.

Both gaps are user-visible: (1) shows up as `OrkError::LlmProvider("context length exceeded")` mid-task, (2) shows up as the agent asking the same orientation question every session and burning tokens on it.

Cost telemetry is the third strand: ADR [`0022`](0022-observability.md) wants `ork_llm_tokens_total{provider,model,direction}` and a per-tenant cost rollup; both compaction (which trims the prompt) and memory injection (which expands it) move that number, so the same hook needs to feed the same metric.

Tenant scoping is non-negotiable: ADR [`0020`](0020-tenant-security-and-trust.md) makes `tenant_id` a hard isolation boundary at the database (RLS) and JWT (`tid_chain`) layer. Memories must inherit that boundary.

## Decision

ork **introduces** two new ports and a token-estimation surface, all driven from inside the `LocalAgent` tool loop:

1. `ContextCompactor` (port in `ork-agents`) — invoked when an estimated-token threshold is exceeded, returns a shorter `Vec<ChatMessage>` for the next iteration.
2. `AgentMemory` (port in `ork-core`) — durable, tenant- and agent-scoped key-value + semantic store; surfaced to the LLM as two native tools (`remember`, `recall`) and optionally auto-injected at task start.
3. `TokenEstimator` (port in `ork-llm`) — cheap, provider-agnostic token count for a `Vec<ChatMessage>` plus tool catalog, used by both compaction and cost telemetry.

The defaults make the system safe for existing callers (no behaviour change unless an agent opts in or the threshold is hit).

### `TokenEstimator` (ork-llm)

```rust
// crates/ork-llm/src/tokens.rs
#[async_trait]
pub trait TokenEstimator: Send + Sync {
    /// Best-effort token count for the wire payload `request` would
    /// produce. Cheap (≤ 1 ms typical); not authoritative — the
    /// authoritative count is the provider's `usage.prompt_tokens` in
    /// `ChatResponse`. Implementations that cannot compute a real
    /// estimate return `Ok(0)` and the caller treats it as "unknown".
    async fn estimate_request(&self, request: &ChatRequest) -> Result<u32, OrkError>;
}

pub struct TiktokenEstimator { /* cl100k_base / o200k_base lookup */ }
pub struct ProviderHintEstimator { /* uses LlmProvider::capabilities + 4-chars-per-token fallback */ }
```

Lives in `ork-llm` because (a) tokenisation is provider-flavoured (OpenAI's `cl100k_base` is wrong for Anthropic; Anthropic exposes a `count_tokens` endpoint we can hit), and (b) only `ork-llm` already takes a dep on a concrete tokeniser crate (`tiktoken-rs`). `ork-core` and `ork-agents` only see the trait via `Arc<dyn TokenEstimator>`, preserving the hexagonal invariant in §3 of [`AGENTS.md`](../../AGENTS.md).

The router in [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs) gains a `pub fn estimator(&self) -> Arc<dyn TokenEstimator>` that picks the right impl per resolved provider.

### `ContextCompactor` (ork-agents)

```rust
// crates/ork-agents/src/compactor.rs
#[async_trait]
pub trait ContextCompactor: Send + Sync {
    /// Returns a (possibly) shorter message list and a one-line
    /// summary of what was dropped/folded. Must preserve the system
    /// prompt at index 0 and the final user/tool message; everything
    /// in between is fair game.
    async fn compact(
        &self,
        agent_id: &AgentId,
        history: Vec<ChatMessage>,
        budget: u32,
    ) -> Result<Compacted, OrkError>;
}

pub struct Compacted {
    pub messages: Vec<ChatMessage>,
    pub dropped_count: usize,
    pub summary: Option<String>,
}
```

Three in-tree strategies, selected by `AgentConfig::compaction`:

| Strategy           | Behaviour                                                                                                  | Default for |
| ------------------ | ---------------------------------------------------------------------------------------------------------- | ----------- |
| `DropOldest`       | Drop oldest non-system messages until estimated tokens ≤ `budget * 0.7`. No LLM call.                      | unit tests  |
| `SummarizeOldestN` | Run a *separate* `chat` call against the same `LlmProvider` to summarise the oldest `N` messages into one. | coding agents (default) |
| `RollingSummary`   | Maintain a single sliding "summary so far" message that is updated each time compaction fires.             | research / chat agents |

The agent loop calls the compactor when `estimator.estimate_request(&request).await? > caps.max_context * agent.compaction_trigger_ratio` (default `0.8`, configurable per agent via `AgentConfig::compaction_trigger_ratio: f32`). Compaction runs at most once per loop iteration; if a compacted prompt still exceeds the budget the loop fails fast with `OrkError::Workflow("context_overflow_after_compaction")` rather than calling the provider.

`AgentConfig` (the existing struct used at [`crates/ork-agents/src/local.rs:314`](../../crates/ork-agents/src/local.rs)) gains:

```rust
pub struct AgentConfig {
    // ...existing fields...
    pub compaction: CompactionStrategy,        // default: SummarizeOldestN { n: 8 }
    pub compaction_trigger_ratio: f32,         // default: 0.8
    pub memory_autoload_top_k: Option<u32>,    // None disables autoload
    pub memory_autoload_query_template: Option<String>,
}
```

### `AgentMemory` (ork-core)

```rust
// crates/ork-core/src/ports/agent_memory.rs
#[async_trait]
pub trait AgentMemory: Send + Sync {
    async fn remember(&self, key: MemoryKey, fact: MemoryFact) -> Result<MemoryId, OrkError>;
    async fn recall(
        &self,
        key: MemoryScope,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<MemoryHit>, OrkError>;
    async fn forget(&self, key: MemoryScope, id: MemoryId) -> Result<(), OrkError>;
    async fn list(
        &self,
        key: MemoryScope,
        topic: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MemoryHit>, OrkError>;
}

pub struct MemoryScope {
    pub tenant_id: TenantId,        // mandatory; populated from RequestCtx
    pub agent_id: AgentId,          // mandatory; the agent the memory belongs to
    pub topic: Option<String>,      // optional namespacing within an agent
}

pub struct MemoryFact {
    pub body: String,               // ≤ 4 KiB; the raw fact text
    pub kind: MemoryKind,           // User | Project | Reference | Feedback
    pub source_task_id: Option<TaskId>,
    pub ttl: Option<Duration>,      // None = persistent
}

pub struct MemoryHit {
    pub id: MemoryId,
    pub fact: MemoryFact,
    pub score: f32,                 // similarity score; 1.0 for exact-key recall
    pub created_at: DateTime<Utc>,
}
```

The four `MemoryKind` values mirror the durable-memory typology already used by Claude Code (`user`, `project`, `reference`, `feedback`). The semantics are documented in `crates/ork-core/src/ports/agent_memory.rs` for self-contained orientation.

Two backing impls land in this ADR:

- `InMemoryAgentMemory` in `ork-core` (test default, `dashmap` + naive substring scoring).
- `PgVectorAgentMemory` in `ork-persistence` — a new `agent_memory` table plus a pgvector `embedding vector(1536)` column. Embeddings are computed via a new `EmbeddingProvider` port in `ork-llm` (one impl: OpenAI `text-embedding-3-small` through the existing OpenAI-compatible client). RLS policy on `agent_memory` is identical to the pattern in [`migrations/001_initial.sql`](../../migrations/001_initial.sql): `USING (tenant_id = current_setting('app.current_tenant_id')::uuid)`.

A new migration `migrations/009_agent_memory.sql` creates the table:

```sql
CREATE TABLE agent_memory (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id     TEXT NOT NULL,
    topic        TEXT,
    kind         TEXT NOT NULL CHECK (kind IN ('user','project','reference','feedback')),
    body         TEXT NOT NULL,
    embedding    vector(1536),
    source_task_id UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
    expires_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE agent_memory ENABLE ROW LEVEL SECURITY;
CREATE POLICY agent_memory_tenant_isolation ON agent_memory
    USING (tenant_id = current_setting('app.current_tenant_id')::uuid);
CREATE INDEX agent_memory_lookup ON agent_memory (tenant_id, agent_id, topic);
CREATE INDEX agent_memory_embedding ON agent_memory USING ivfflat (embedding vector_cosine_ops);
```

### Native tools: `remember` and `recall`

Two new entries in [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)'s `ToolExecutor` catalog, both internal-Rust tools (so the §3 invariant in [`AGENTS.md`](../../AGENTS.md) about "external tools via MCP" does not apply — these wrap a port, not an external system):

| Tool       | Schema                                                                       | Effect                                                       |
| ---------- | ---------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `remember` | `{ body: string, kind: enum, topic?: string, ttl_seconds?: u32 }`            | Calls `AgentMemory::remember`.                               |
| `recall`   | `{ query: string, top_k?: u32, topic?: string }` → `Vec<{ id, body, score }>` | Calls `AgentMemory::recall`.                                 |

Tools are gated by an RBAC scope `agents:memory:write` / `agents:memory:read` (ADR [`0021`](0021-rbac-scopes.md)). Both default to "on" for any agent in the agent's own tenant, "off" cross-tenant; that policy is enforced inside `AgentMemory` impls (re-checking inside the port, not just at the gateway, defends against a buggy caller forgetting to set `tenant_id` on the scope).

### Auto-injection at task start

When `AgentConfig::memory_autoload_top_k = Some(k)`, the `LocalAgent` runs `recall(query = first_user_message, top_k = k)` before the first `chat_stream` call and prepends the hits as a single `MessageRole::System` message of the form:

```
Relevant memories from prior sessions (top-3, scored):
- [project/score=0.91] this repo uses tokio with default features
- [reference/score=0.82] integration tests live in crates/ork-x/tests/
- [user/score=0.71] prefer terse code review comments
```

The injection is opt-in to keep current agent configs unchanged. The query template can be overridden per agent via `AgentConfig::memory_autoload_query_template` (a Tera template over the task input) for cases where the raw first message is a bad search query.

### Cost telemetry

The agent loop emits, after every iteration, a `tracing::info!(target = "ork.cost", ...)` event with:

```
{
  agent_id, task_id, tenant_id,
  iteration, prompt_tokens_estimated, prompt_tokens_actual,
  completion_tokens, est_cost_usd,
  compaction_fired: bool, dropped_count: usize, memory_hits_injected: usize
}
```

`est_cost_usd` is computed from a per-`(provider, model)` rate table loaded from `LlmConfig::cost_table` (already present as `ModelCapabilitiesEntry` per [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs); this ADR adds `prompt_per_million_usd` and `completion_per_million_usd` fields). ADR [`0022`](0022-observability.md) consumes the `ork.cost` target via its OpenTelemetry exporter and exposes `ork_agent_cost_usd_total{tenant,agent,model}` and `ork_agent_compactions_total{agent,strategy}`.

## Acceptance criteria

- [ ] Trait `TokenEstimator` defined at `crates/ork-llm/src/tokens.rs` with the signature in `Decision`.
- [ ] Concrete impls `TiktokenEstimator` and `ProviderHintEstimator` exported from `ork-llm`; `LlmRouter::estimator(&self)` returns the right impl per resolved provider.
- [ ] Trait `ContextCompactor` defined at `crates/ork-agents/src/compactor.rs` with the signature in `Decision`.
- [ ] Strategies `DropOldest`, `SummarizeOldestN`, `RollingSummary` implemented as `ContextCompactor` impls in `ork-agents`.
- [ ] `AgentConfig` extended with `compaction`, `compaction_trigger_ratio`, `memory_autoload_top_k`, `memory_autoload_query_template`; existing call sites compile against `AgentConfig::default()` without churn.
- [ ] `LocalAgent` agent loop in [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) calls `estimator.estimate_request(...)` before each `chat_stream`; when the estimate exceeds `caps.max_context * compaction_trigger_ratio` it calls `compactor.compact(...)` exactly once per iteration and replaces `history`.
- [ ] When compaction fires, the loop emits a `tracing::info!(target = "ork.cost", compaction_fired = true, ...)` event with `dropped_count` and the strategy name.
- [ ] If a compacted history still exceeds `caps.max_context`, the loop returns `OrkError::Workflow("context_overflow_after_compaction")` *without* calling the provider.
- [ ] Trait `AgentMemory` defined at `crates/ork-core/src/ports/agent_memory.rs` with the signature in `Decision`; `MemoryScope` carries a non-optional `TenantId`.
- [ ] `InMemoryAgentMemory` in `ork-core` passes the shared port-conformance test suite at `crates/ork-core/tests/agent_memory_smoke.rs`.
- [ ] `PgVectorAgentMemory` in `ork-persistence` passes the same suite under `#[cfg(feature = "postgres-tests")]`.
- [ ] Migration `migrations/009_agent_memory.sql` creates the `agent_memory` table with RLS enabled, the tenant-isolation policy, and the ivfflat embedding index shown in `Decision`.
- [ ] Cross-tenant recall test: `crates/ork-persistence/tests/agent_memory_rls.rs::cross_tenant_recall_returns_empty` writes a memory under tenant A, queries under tenant B, and asserts zero hits.
- [ ] `remember` and `recall` registered as native tools in [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs); their JSON schemas match `Decision`.
- [ ] RBAC scopes `agents:memory:read` and `agents:memory:write` declared in `crates/ork-api`'s scope catalog (ADR [`0021`](0021-rbac-scopes.md)).
- [ ] Auto-injection test: `crates/ork-agents/tests/local_memory_autoload.rs::injects_top_k_at_task_start` configures `memory_autoload_top_k = Some(3)`, seeds three memories, and asserts a system message of the documented shape lands at index 1 of the first `ChatRequest`.
- [ ] Per-iteration `ork.cost` event fields (`prompt_tokens_estimated`, `prompt_tokens_actual`, `completion_tokens`, `est_cost_usd`, `compaction_fired`, `memory_hits_injected`) emitted by the loop and asserted in `crates/ork-agents/tests/local_cost_telemetry.rs`.
- [ ] `LlmConfig::cost_table` in [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs) gains `prompt_per_million_usd` and `completion_per_million_usd`; missing entries make `est_cost_usd` `None`, not a panic.
- [ ] `EmbeddingProvider` port in `ork-llm` with one OpenAI-compatible impl; called by `PgVectorAgentMemory::remember`.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0032` added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after implementation lands.

## Consequences

### Positive

- Long-running coding agents stop dying with "context length exceeded" mid-session; the compactor degrades gracefully before the provider rejects the request.
- Cross-task knowledge survives. Agents stop re-asking the same orientation questions, which is the single biggest token sink in dogfooding sessions.
- Cost telemetry gets a real number (`est_cost_usd`) instead of "tokens used" alone, which is what the operator dashboard in ADR [`0022`](0022-observability.md) actually needs.
- The compactor is a port, so a future `LlmCompactor` (hand off to a smaller cheaper model for summarisation) is a one-impl change.
- pgvector lets us reuse the existing Postgres deployment; no new infra component lands with this ADR.

### Negative / costs

- A new dependency: `tiktoken-rs` (or equivalent) in `ork-llm`. Pure-Rust, but adds ~200 KB of token tables. Acceptable.
- pgvector requires the `vector` extension in Postgres. Documented as a deployment prerequisite; tests gate on a feature flag so CI without pgvector still passes.
- `SummarizeOldestN` makes a *second* LLM call per compaction, increasing latency and cost on the iteration where it fires. We accept the cost because the alternative (`OrkError`) is worse, and the strategy is per-agent overridable.
- Token estimation lies. `tiktoken` is exact for OpenAI but only an approximation for Anthropic, Gemini, and Llama. We mitigate by counting the *actual* `usage.prompt_tokens` from the provider's response and feeding it back into the next iteration's threshold check; the estimator only matters for the *first* iteration.
- `AgentMemory` is a new tenant-scoped data domain. ADR [`0020`](0020-tenant-security-and-trust.md)'s `TenantTxScope` must be honoured by every call site or RLS will be off; this is the same risk that exists for every tenant-scoped table today.
- Auto-injection uses tokens. An over-eager `top_k` can cost more than it saves. Default of `None` (off) keeps the choice explicit per agent.
- Compaction can drop information the model still needs (e.g., a tool result the agent will reference 20 turns later). `RollingSummary` mitigates but does not eliminate. We mark `compaction_fired = true` in telemetry so regressions traced to dropped context are identifiable.
- Embeddings cost money and round-trip latency on every `remember`. We do not embed inside the request path; `remember` writes the row first and an async worker (out of scope for this ADR; tracked in `Open questions`) backfills the embedding column. Until then, `recall` falls back to a `tsvector` text-search index.

### Neutral / follow-ups

- A future ADR may introduce a global "memory garbage collector" (drop facts whose `score` from `recall` never crosses a threshold for N days). Out of scope here.
- Coding-agent–specific memory shapes (e.g., file-level facts, "this test is flaky on macOS") are accommodated via `topic` and `kind`; if they grow into structured columns we will write a successor ADR.
- ADR [`0024`](0024-wasm-plugin-system.md) plugins may want to register additional `ContextCompactor` impls; the port is intentionally `Send + Sync + 'static` so the plugin host can hand back a `dyn ContextCompactor`.

## Alternatives considered

- **Provider-side context windowing only.** Rely on each provider's "auto-truncation" or extended-context endpoints. Rejected: it is not portable across ADR [`0012`](0012-multi-llm-providers.md)'s catalog, gives no signal back to ork that truncation happened, and does nothing for cross-task recall.
- **Single fixed strategy (`DropOldest`).** Simpler, no LLM round-trip. Rejected: dropping the middle of a coding session reliably loses the agent's working hypothesis. The cost of a summarisation call is well below the cost of a re-derivation cycle.
- **Memory as an MCP server (`mcp-memory`).** Would have re-used ADR [`0010`](0010-mcp-tool-plane.md) and avoided a new port. Rejected: tenancy enforcement would have to live in the MCP server, where we cannot enforce RLS or read `RequestCtx` directly. The risk of cross-tenant leakage is too high for a feature that is mandatory-tenant-scoped per ADR [`0020`](0020-tenant-security-and-trust.md). An MCP-backed *implementation* of `AgentMemory` remains possible (it would just need to receive the `tenant_id` over the wire and trust the caller, which is fine when the caller is ork itself).
- **Long-term memory only as files in artifact storage (ADR [`0016`](0016-artifact-storage.md)).** Reuses an existing surface. Rejected: artifacts are blob-shaped, lack semantic search, and have no tenant-scoped index for `(agent_id, topic)` lookups. Memory is structurally different and deserves its own port.
- **Token estimation in `ork-core`.** Keeps `ork-llm` thinner. Rejected: tokenisation is provider-specific and `ork-core` does not depend on any tokeniser crate. Putting it in `ork-llm` keeps the dependency boundary clean and lets the router pick the right tokeniser per resolved provider.
- **No `forget` / `list` methods on the port (only `remember` / `recall`).** Smaller surface. Rejected: operators need a way to delete a fact (GDPR-style erasure on the tenant boundary) and an admin needs to list facts during incident response.
- **Auto-injection on by default.** Aggressive, makes the feature more visible. Rejected: it changes the prompt for every existing agent silently; opt-in is safer and the cost-vs-benefit is per-agent.
- **Use a separate vector DB (Qdrant / Weaviate).** Best-in-class semantic search. Rejected for this ADR: it would add a new infra component and a new wire protocol; pgvector handles `top_k = 5` over millions of rows in the latency budget we care about. A successor ADR can swap the impl behind the port if measurements demand it.

## Affected ork modules

- [`crates/ork-llm/src/`](../../crates/ork-llm/) — new `tokens.rs` module, `EmbeddingProvider` port, OpenAI-compatible embedding impl.
- [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs) — `estimator()` accessor, per-resolved-provider tokeniser selection.
- [`crates/ork-agents/src/`](../../crates/ork-agents/) — new `compactor.rs` with the trait and three strategies.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) — wire estimation + compaction into the existing tool loop (~30 lines around the `loop {}` at line 324); emit `ork.cost` events; honour `memory_autoload_top_k`.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/ports/) — new `agent_memory.rs` with `AgentMemory`, `MemoryScope`, `MemoryFact`, `MemoryHit`, `MemoryKind`.
- [`crates/ork-core/src/`](../../crates/ork-core/) — `InMemoryAgentMemory` impl for tests + default wiring.
- [`crates/ork-persistence/src/postgres/`](../../crates/ork-persistence/src/postgres/) — `PgVectorAgentMemory` impl.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs) — `remember` and `recall` native tool registrations.
- [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs) — `prompt_per_million_usd` / `completion_per_million_usd` on the cost table.
- [`migrations/009_agent_memory.sql`](../../migrations/) — new migration.
- [`docs/adrs/README.md`](README.md) — index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Claude Code | `auto memory` system in the harness; `MEMORY.md` index + per-fact `.md` files keyed by `user`/`feedback`/`project`/`reference` | `AgentMemory` port + `MemoryKind` enum |
| LangGraph | `MemorySaver` checkpointer + `langgraph_checkpoint`'s thread-scoped store | per-task A2A history (already present in ADR [`0003`](0003-a2a-protocol-model.md)); compactor handles the budget |
| Letta (formerly MemGPT) | "core memory" / "archival memory" split with explicit `core_memory_*` and `archival_memory_*` tools | `remember` / `recall` native tools backed by `AgentMemory` |
| OpenAI Assistants v2 | Thread-level message store + automatic truncation strategy | A2A history + `ContextCompactor` strategies |
| Anthropic | `count_tokens` API | `ProviderHintEstimator` impl variant for the Anthropic-compatible client |

## Open questions

- Where does embedding generation run? Inside the `remember` request path (simpler, but adds a network round-trip and a failure mode) or in an async `agent_memory_embed` worker (more code, but the user-visible `remember` stays cheap)? Default to async-worker; if the worker backlog grows, fall back to inline. Tracked as a follow-up implementation detail; not a blocker for this ADR.
- Should `recall` honour ADR [`0021`](0021-rbac-scopes.md) scopes per-`MemoryKind` (e.g., agents may write `feedback` only with admin scope)? Out of scope here; revisit when the audit log lands.
- TTL enforcement: scheduled via `pg_cron` (deployment dep) or via a Rust worker in `ork-cli`? Defer until the first operator hits the question.
- Cross-agent memory sharing within a tenant (agent A reads agent B's memories) is intentionally not allowed in v1. Some workflows (e.g., a "router" agent that delegates to specialists, ADR [`0006`](0006-peer-delegation.md)) might want it; if so, we will add an explicit `shared` `MemoryScope` field rather than weaken isolation.
- `RollingSummary` summary persistence: does the rolling summary itself become a `MemoryFact` (so it survives across tasks) or stay in-task only? Default: in-task only; promoting to memory is the agent's explicit choice via `remember`.

## References

- A2A spec: <https://github.com/google/a2a>
- Related ADRs: [`0002`](0002-agent-port.md), [`0003`](0003-a2a-protocol-model.md), [`0011`](0011-native-llm-tool-calling.md), [`0012`](0012-multi-llm-providers.md), [`0020`](0020-tenant-security-and-trust.md), [`0021`](0021-rbac-scopes.md), [`0022`](0022-observability.md)
- pgvector: <https://github.com/pgvector/pgvector>
- tiktoken-rs: <https://crates.io/crates/tiktoken-rs>
- Anthropic `count_tokens`: <https://docs.anthropic.com/en/api/messages-count-tokens>
- Letta memory model: <https://docs.letta.com/concepts/memory>
