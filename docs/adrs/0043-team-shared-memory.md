# 0043 — Team-scoped shared memory and decision log

- **Status:** Proposed
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0016, 0020, 0022, 0032, 0033, 0034, 0040, 0042, 0045
- **Supersedes:** —

## Context

ADR [`0032`](0032-agent-memory-and-context-compaction.md) gave each
agent a tenant- and *agent*-scoped memory bucket: the planner learns
"this repo uses tokio with `full` features" and the next planner
session recalls it. That works inside one agent's lifetime. It does
not work for a **team** of cooperating agents — the workload that the
upcoming team orchestrator (ADR [`0045`]) and the capability-tagged
discovery surface from ADR
[`0042`](0042-capability-discovery.md) are designed to compose:
architect, executor, reviewer, tester, possibly more, all working on
the same task.

Two concrete failure modes show up in dogfooding the moment more than
one persona touches a task:

1. **Context rediscovery.** The architect spends three iterations
   establishing "we decided to keep `WorkspaceHandle` clone-safe and
   not share editor handles across sub-agents". The reviewer is
   dispatched, has no access to that decision (it lives in the
   architect's per-agent
   [`AgentMemory`](../../crates/ork-core/src/ports/agent_memory.rs)
   under
   [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)),
   and either re-derives it from the diff or — worse — proposes a
   change that violates it. The executor has the same problem when
   asked to follow up next iteration.
2. **"What we already tried."** Three sub-agents, three independent
   attempts to fix the same flaky test. Each one reads the failure,
   chooses an approach, and burns ~6 K tokens in tool calls before
   committing. None of them sees the prior attempts. The team
   orchestrator can stitch turns together inside a single dispatch
   loop, but it has no durable place to record "we tried bumping the
   timeout, did not help; we tried serialising the test, did not
   help" so that the *next* sub-agent it dispatches can read that
   before starting.

The architecture already has the right shape for the fix:

- ADR [`0032`](0032-agent-memory-and-context-compaction.md)'s
  `AgentMemory` port and its `PgVectorAgentMemory` impl give us the
  durable, tenant-scoped, semantically-searchable store. The only
  thing to change is the **scope key** — `(TenantId, AgentId)`
  becomes `(TenantId, TeamId)`.
- ADR [`0034`](0034-per-model-capability-profiles.md)'s
  `ModelProfile` already gates auto-injection of per-agent memory
  via `memory_autoload_top_k`. Team-shared auto-priming reuses the
  same flag pattern.
- ADR [`0020`](0020-tenant-security-and-trust.md)'s
  `app.current_tenant_id` and the `TenantTxScope` middleware already
  enforce tenant isolation at the SQL layer; team scoping nests
  inside that boundary.
- ADR [`0016`](0016-artifact-storage.md)'s artifact lifecycle and
  retention model is the right precedent for what to do with a
  team's bucket *after* the team disbands (audit / GDPR-style
  retention windows).

What's missing is (a) the canonical definition of `TeamId` —
ADRs [`0039`](0039-agent-tool-call-hooks.md), [`0044`] and [`0045`]
all reference it but no ADR currently owns the type — and (b) the
team-shared analogue of `AgentMemory` together with a *first-class
decision log*, because the "what we decided and why" entries are
structurally different from free-form recall hits and deserve their
own append-only shape.

## Decision

ork **introduces** a canonical `TeamId` type, a `TeamMemory` port
that reuses the storage abstraction defined by ADR
[`0032`](0032-agent-memory-and-context-compaction.md) at a different
scope, and an append-only **decision log** with first-class
`supersedes` semantics. Four native tools surface the port to LLMs.
Auto-priming pulls the most-recent decisions and top-K relevant notes
into a sub-agent's initial context, gated by a per-`ModelProfile`
flag. Lifetime is tied to the team; completed-team retention reuses
ADR [`0016`](0016-artifact-storage.md)'s retention plumbing.

### `TeamId` (canonical home)

```rust
// crates/ork-core/src/models/team.rs
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TeamId(pub uuid::Uuid);

impl TeamId {
    pub fn new() -> Self { Self(uuid::Uuid::new_v4()) }
}
```

`TeamId` is **created by the team orchestrator** at team formation
(ADR [`0045`]; this ADR ships the type, [`0045`] ships the
constructor call site). It is scoped under `TenantId` (ADR
[`0020`](0020-tenant-security-and-trust.md)): the pair `(TenantId,
TeamId)` is the unique identity. A team's lifetime is at least the
lifetime of the originating user task; orchestrators may reuse a
`TeamId` across follow-up tasks within the same conversation but
**must not** reuse one across distinct tenants.

This ADR is the canonical home for the type. ADR
[`0039`](0039-agent-tool-call-hooks.md) (tool-call hooks),
[`0044`] (planned), and [`0045`] (planned) reference it; they import
from `ork_core::models::team::TeamId` rather than redeclaring it.

### `TeamMemory` port

`TeamMemory` is the team-scoped twin of ADR
[`0032`](0032-agent-memory-and-context-compaction.md)'s
`AgentMemory`. Two memory shapes live inside one team bucket: free-form
**notes** (same shape as 0032's per-agent memory) and an append-only
**decision log**.

```rust
// crates/ork-core/src/ports/team_memory.rs
#[async_trait]
pub trait TeamMemory: Send + Sync {
    // ---- free-form notes (mirror AgentMemory's recall surface) ----

    async fn remember(
        &self,
        scope: TeamScope,
        author: AgentId,
        fact: MemoryFact,        // reused from ADR 0032
    ) -> Result<MemoryId, OrkError>;

    async fn recall(
        &self,
        scope: TeamScope,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<MemoryHit>, OrkError>;

    async fn list_notes(
        &self,
        scope: TeamScope,
        topic: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MemoryHit>, OrkError>;

    async fn forget_note(&self, scope: TeamScope, id: MemoryId) -> Result<(), OrkError>;

    // ---- decision log (append-only) ----

    async fn log_decision(
        &self,
        scope: TeamScope,
        author: AgentId,
        decision: DecisionDraft,
    ) -> Result<DecisionId, OrkError>;

    async fn list_decisions(
        &self,
        scope: TeamScope,
        filter: DecisionFilter,
    ) -> Result<Vec<DecisionEntry>, OrkError>;

    async fn get_decision(
        &self,
        scope: TeamScope,
        id: DecisionId,
    ) -> Result<DecisionEntry, OrkError>;
}

pub struct TeamScope {
    pub tenant_id: TenantId,           // mandatory, populated from RequestCtx (ADR 0020)
    pub team_id: TeamId,
    pub topic: Option<String>,         // optional namespacing within a team
}

pub struct DecisionDraft {
    pub summary: String,               // ≤ 280 chars; one-line headline
    pub rationale: String,             // ≤ 8 KiB; why this, not the alternatives
    pub alternatives_considered: Vec<String>,  // each ≤ 1 KiB
    pub supersedes: Option<DecisionId>,         // pointer to the entry this revises
    pub tags: Vec<String>,
}

pub struct DecisionEntry {
    pub id: DecisionId,
    pub team_id: TeamId,
    pub tenant_id: TenantId,
    pub author_agent_id: AgentId,
    pub summary: String,
    pub rationale: String,
    pub alternatives_considered: Vec<String>,
    pub supersedes: Option<DecisionId>,
    pub superseded_by: Option<DecisionId>,     // back-pointer, set when a later entry names this one
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
}

pub struct DecisionFilter {
    pub tag: Option<String>,
    pub author_agent_id: Option<AgentId>,
    pub since: Option<DateTime<Utc>>,
    pub include_superseded: bool,      // default false
    pub limit: u32,
}
```

`MemoryFact`, `MemoryHit`, `MemoryId`, `MemoryKind` are reused
verbatim from ADR
[`0032`](0032-agent-memory-and-context-compaction.md) — this ADR
introduces no new note shape. The only difference is the scope key.

**Append-only semantics for the decision log.** `log_decision`
inserts a new row. There is no `update_decision` or
`delete_decision`. Revising a decision requires a *new* entry whose
`supersedes` field names the prior one; the impl sets the
`superseded_by` back-pointer on the named entry inside the same
transaction. This preserves an auditable history of how a team's
mind changed over time, which is the whole point of a decision log
(it is the analogue of how this repo's own ADRs work — see
[`docs/adrs/0001-adr-process-and-conventions.md`](0001-adr-process-and-conventions.md)).

### Backing impls

Two impls land with this ADR. Both reuse the storage abstraction
already established by ADR
[`0032`](0032-agent-memory-and-context-compaction.md):

- `InMemoryTeamMemory` in `ork-core` (test default, `dashmap` +
  naive substring scoring; identical pattern to
  `InMemoryAgentMemory`).
- `PgTeamMemory` in `ork-persistence` — reuses the same
  pgvector-enabled Postgres deployment as 0032, with two new tables
  (`team_memory_notes` mirrors 0032's `agent_memory` shape;
  `team_decision_log` is the append-only log). Embeddings on the
  notes table use the existing `EmbeddingProvider` port from
  [`0032`](0032-agent-memory-and-context-compaction.md). **No new
  storage backend.**

A new migration `migrations/010_team_memory.sql` creates both tables:

```sql
CREATE TABLE team_memory_notes (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    team_id         UUID NOT NULL,
    topic           TEXT,
    kind            TEXT NOT NULL CHECK (kind IN ('user','project','reference','feedback')),
    body            TEXT NOT NULL,
    embedding       vector(1536),
    author_agent_id TEXT NOT NULL,
    source_task_id  UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE team_memory_notes ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_memory_notes_tenant_isolation ON team_memory_notes
    USING (tenant_id = current_setting('app.current_tenant_id')::uuid);
CREATE INDEX team_memory_notes_lookup ON team_memory_notes (tenant_id, team_id, topic);
CREATE INDEX team_memory_notes_embedding ON team_memory_notes USING ivfflat (embedding vector_cosine_ops);

CREATE TABLE team_decision_log (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    team_id                  UUID NOT NULL,
    author_agent_id          TEXT NOT NULL,
    summary                  TEXT NOT NULL CHECK (char_length(summary) <= 280),
    rationale                TEXT NOT NULL,
    alternatives_considered  JSONB NOT NULL DEFAULT '[]'::jsonb,
    tags                     TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    supersedes               UUID REFERENCES team_decision_log(id) ON DELETE SET NULL,
    superseded_by            UUID REFERENCES team_decision_log(id) ON DELETE SET NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE team_decision_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_decision_log_tenant_isolation ON team_decision_log
    USING (tenant_id = current_setting('app.current_tenant_id')::uuid);
CREATE INDEX team_decision_log_team ON team_decision_log (tenant_id, team_id, created_at DESC);
CREATE INDEX team_decision_log_supersedes ON team_decision_log (supersedes);
```

### Native tools

Four entries are added to
[`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)'s
`ToolExecutor` catalog. All four are internal-Rust tools (the §3
"external tools via MCP" invariant in
[`AGENTS.md`](../../AGENTS.md) does not apply — these wrap a port,
not an external system).

| Tool                  | Schema                                                                                                  | Effect                                  |
| --------------------- | ------------------------------------------------------------------------------------------------------- | --------------------------------------- |
| `team_remember`       | `{ body: string, kind: enum, topic?: string, tags?: string[], ttl_seconds?: u32 }`                      | `TeamMemory::remember`                  |
| `team_recall`         | `{ query: string, top_k?: u32, topic?: string }` → `Vec<{ id, body, score, author_agent_id }>`          | `TeamMemory::recall`                    |
| `team_log_decision`   | `{ summary: string, rationale: string, alternatives_considered?: string[], supersedes?: uuid, tags?: string[] }` | `TeamMemory::log_decision`     |
| `team_list_decisions` | `{ tag?: string, author_agent_id?: string, since?: timestamp, include_superseded?: bool, limit?: u32 }` → `Vec<DecisionEntry>` | `TeamMemory::list_decisions` |

### Access policy

The agent's task must be a member of the team — i.e. it must have
been dispatched by the team orchestrator with a `RequestCtx` that
carries the matching `(tenant_id, team_id)` pair. The `TeamMemory`
impl re-checks team membership inside every call, not only at the
gateway, so a buggy caller that forgets to set `team_id` cannot
leak across teams. (Same defence-in-depth pattern ADR
[`0032`](0032-agent-memory-and-context-compaction.md) applies for
tenant.)

Within a team:

- **Reads.** Any member reads all notes and all decisions.
- **Note writes.** Any member writes notes (`team_remember`,
  `forget_note` for the author's own notes only).
- **Decision-log writes.** Configurable per team via a new
  `TeamConfig::decision_write_policy` enum:
  - `Open` — any member may call `team_log_decision`.
  - `Restricted { roles: Vec<CodingRole> }` — only members whose
    `coding-persona` extension role (ADR
    [`0033`](0033-coding-agent-personas.md)) is in the set may log
    decisions. The orchestrator rejects non-matching callers with
    `OrkError::Forbidden("decision_log_role_restricted")`.

The **default** is `Restricted { roles: [Architect, Reviewer] }`.
Rationale: the decision log is the team's durable record of what was
decided and why; allowing every executor to commit entries inflates
the log with implementation choices and makes it harder to read at
priming time. Architect / Reviewer roles are the intentional decision
points in ADR [`0033`](0033-coding-agent-personas.md)'s persona set.
Notes remain `Open` so that any sub-agent can record "what we
already tried" without negotiating role.

The role check uses the persona descriptor from ADR
[`0033`](0033-coding-agent-personas.md); RBAC scopes
`team:memory:read` / `team:memory:write` /
`team:decisions:write` are declared in `crates/ork-api`'s scope
catalog (ADR [`0021`](0021-rbac-scopes.md)) for the gateway-level
gate.

### Auto-priming on sub-agent dispatch

When the team orchestrator (ADR [`0045`]) dispatches a sub-agent,
it optionally prepends a single `MessageRole::System` message to
the sub-agent's initial `ChatRequest` containing the most-recent
*N* decisions and top-*K* notes relevant to the dispatch prompt:

```
Team context (team_id=…):
- Decisions (most recent 5):
  • [arch/2026-04-28] Keep WorkspaceHandle clone-safe; do not share
    editor handles across sub-agents. (rationale: prevents the
    cross-thread aliasing bug in incident 2026-04-22)
  • [reviewer/2026-04-28] Reject diffs that introduce blocking I/O
    in async fns. (supersedes earlier "warn-only" decision)
  …
- Relevant notes (top-3, scored):
  • [project/score=0.91] tried bumping the test timeout, did not help
  • [reference/score=0.83] integration tests live in crates/ork-x/tests/
  • [feedback/score=0.71] reviewer prefers terse comments
```

Volume is gated by `ModelProfile` flags (ADR
[`0034`](0034-per-model-capability-profiles.md), §"per-team-prime
budgets"):

```rust
pub struct ModelProfile {
    // ...existing fields...
    pub team_prime_recent_decisions: Option<u32>,   // None = disabled; default Some(5)
    pub team_prime_top_k_notes: Option<u32>,        // None = disabled; default Some(3)
}
```

Defaults are **on** for frontier-tier profiles (≥ 64 K context) and
**off** for tight-context profiles (the orchestrator falls back to
on-demand `team_recall` instead). Same opt-in posture as 0032's
`memory_autoload_top_k`: existing per-agent profiles compile
without churn (the new fields default to `None`).

The auto-prime query for notes uses the dispatch prompt as the
search query; for decisions it is a chronological "most recent N
not superseded" pull, not a semantic search, because the team needs
the latest *position* on each topic, not the most-relevant.

### Lifetime and garbage collection

A team's bucket lives at least as long as the team's task. Three
configurable lifetimes apply, mirroring ADR
[`0016`](0016-artifact-storage.md)'s artifact retention model:

- **Active.** While the originating workflow run is in a
  non-terminal state, the bucket is fully readable and writable.
- **Frozen (post-completion retention).** When the workflow run
  completes (success or failure), the bucket transitions to
  read-only and a `frozen_at` timestamp is set on every row. A
  configurable retention window (`TeamConfig::retention`, default
  90 days) keeps the bucket queryable for post-hoc audit and for
  follow-up tasks within the same conversation.
- **Purged.** After retention expires, a scheduled job (the same
  worker that runs ADR [`0016`](0016-artifact-storage.md)'s
  artifact GC) deletes both tables' rows for the team. The
  `frozen_at` and `expires_at` timestamps are inherited fields on
  every row, not a separate index, so the GC query is `WHERE
  expires_at < now()`.

The retention window is per-team and configured at team formation
(orchestrator's responsibility per ADR [`0045`]); operators can
override the default via `TenantSettings`.

### Boundary with ADR 0032 (per-agent memory)

These two stay separate ports and separate tables:

| Surface                | Scope                  | Use case                                                                |
| ---------------------- | ---------------------- | ----------------------------------------------------------------------- |
| `AgentMemory` (0032)   | `(TenantId, AgentId)`  | Agent-private long-term memory: "I've seen this repo before, here's how I personally orient." Survives across tasks for *that* agent. |
| `TeamMemory` (this ADR) | `(TenantId, TeamId)`  | Team-shared working memory + decision log: "we (this team) decided X, we tried Y." Lives for the team's lifetime. |

A fact does *not* automatically promote from `AgentMemory` to
`TeamMemory` or vice versa. Promotion is the agent's explicit
choice via `team_remember`. This keeps the two boundaries clean and
preserves agent-private signals (e.g., user preferences for an
individual reviewer agent) from leaking into a team's shared log.

### Boundary with ADR 0040 (repo map)

ADR [`0040`](0040-repo-map.md)'s repo map answers *"what is in the
workspace right now?"* — directory tree plus top-level symbol
signatures, derived mechanically from the source. `TeamMemory`
answers *"what does this team intend to do, and what have they
agreed?"* — intent and decisions, contributed by agents.

| Source           | Answers                          | Lifetime            |
| ---------------- | -------------------------------- | ------------------- |
| Repo map (0040)  | Workspace structural ground truth | Tied to workspace state; invalidated on edit |
| `TeamMemory`     | Team intent / decisions          | Tied to team lifetime + retention window     |

The two compose: a sub-agent's auto-prime concatenates the repo map
(for "what exists") with the team's recent decisions and top-K
notes (for "what we already concluded"). Neither surface tries to
do the other's job.

## Acceptance criteria

- [ ] `TeamId` defined at `crates/ork-core/src/models/team.rs` with the
      signature shown in `Decision` and re-exported from
      `ork-core`'s `models` prelude.
- [ ] Trait `TeamMemory` defined at
      `crates/ork-core/src/ports/team_memory.rs` with the signature
      shown in `Decision`; `TeamScope` carries non-optional
      `TenantId` and `TeamId`.
- [ ] `DecisionDraft`, `DecisionEntry`, `DecisionFilter`,
      `DecisionId` types defined in the same module.
- [ ] `InMemoryTeamMemory` in `ork-core` passes the shared
      port-conformance test suite at
      `crates/ork-core/tests/team_memory_smoke.rs`.
- [ ] `PgTeamMemory` in `ork-persistence` passes the same suite
      under `#[cfg(feature = "postgres-tests")]`.
- [ ] Migration `migrations/010_team_memory.sql` creates the
      `team_memory_notes` and `team_decision_log` tables with RLS
      enabled, the tenant-isolation policies, and the indexes
      shown in `Decision`.
- [ ] Cross-team isolation test:
      `crates/ork-persistence/tests/team_memory_isolation.rs::cross_team_recall_returns_empty`
      writes a note under team A, queries under team B (same
      tenant), and asserts zero hits.
- [ ] Cross-tenant isolation test:
      `crates/ork-persistence/tests/team_memory_rls.rs::cross_tenant_recall_returns_empty`
      writes a note under tenant A, queries under tenant B (same
      team UUID by accident), and asserts zero hits.
- [ ] Append-only test:
      `crates/ork-core/tests/team_decision_log.rs::supersedes_sets_back_pointer`
      logs decision A, logs decision B with `supersedes: A`, and
      asserts (i) A's `superseded_by` equals B's id, (ii) the
      original A row is unchanged otherwise.
- [ ] `team_remember`, `team_recall`, `team_log_decision`, and
      `team_list_decisions` registered as native tools in
      [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs);
      their JSON schemas match `Decision`.
- [ ] RBAC scopes `team:memory:read`, `team:memory:write`, and
      `team:decisions:write` declared in `crates/ork-api`'s scope
      catalog (ADR [`0021`](0021-rbac-scopes.md)).
- [ ] `TeamConfig::decision_write_policy` enum (`Open` |
      `Restricted { roles }`) defined in
      `crates/ork-core/src/models/team.rs`; default is
      `Restricted { roles: [Architect, Reviewer] }`.
- [ ] Restricted-policy test:
      `crates/ork-core/tests/team_decision_log.rs::restricted_policy_rejects_executor`
      configures `Restricted { roles: [Architect, Reviewer] }` and
      asserts a caller with persona role `Executor` receives
      `OrkError::Forbidden("decision_log_role_restricted")`.
- [ ] `ModelProfile` extended (in `crates/ork-llm` per ADR
      [`0034`](0034-per-model-capability-profiles.md)) with
      `team_prime_recent_decisions: Option<u32>` and
      `team_prime_top_k_notes: Option<u32>`; existing profiles
      compile against `ModelProfile::default()` without churn.
- [ ] Auto-prime test:
      `crates/ork-agents/tests/team_prime.rs::injects_decisions_and_notes_at_dispatch`
      configures a profile with
      `team_prime_recent_decisions = Some(5)` and
      `team_prime_top_k_notes = Some(3)`, seeds five decisions and
      three notes, dispatches a sub-agent, and asserts a single
      `MessageRole::System` message of the documented shape lands
      at index 1 of the sub-agent's first `ChatRequest`.
- [ ] Lifetime test:
      `crates/ork-persistence/tests/team_memory_retention.rs::frozen_after_completion_purged_after_retention`
      transitions a team to `Frozen`, asserts writes return
      `OrkError::Forbidden("team_frozen")`, advances simulated
      time past the retention window, runs the GC worker, and
      asserts both tables have zero rows for that team.
- [ ] Membership defence test:
      `crates/ork-core/tests/team_memory_smoke.rs::membership_required`
      calls `TeamMemory::recall` with a `RequestCtx` whose
      `team_id` field is `None` and asserts
      `OrkError::Forbidden("team_membership_required")` (the impl
      re-checks, not only the gateway).
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0043`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- A team of cooperating sub-agents stops rediscovering its own
  decisions every dispatch. The architect's "we keep
  `WorkspaceHandle` clone-safe" survives into the reviewer's
  context for free.
- "What we already tried" becomes durable. The N-th sub-agent
  fixing a flaky test reads the prior N-1 attempts and either
  builds on them or chooses a genuinely different approach.
- The decision log gives operators a queryable record of why a
  team made each call — directly analogous to how the ADR set
  itself works for the human team building ork.
- Reuses 0032's storage abstraction: no new backend, no new
  embedding pipeline, no new infra component lands with this ADR.
  Only two new tables and one trait.
- `supersedes` semantics let teams revise without losing history,
  which is the whole reason humans like ADRs over wikis.
- `TeamId` becomes the canonical reference for ADRs
  [`0039`](0039-agent-tool-call-hooks.md), [`0044`], and [`0045`];
  the type lives in one place instead of being redeclared.

### Negative / costs

- A new tenant-scoped data domain. ADR
  [`0020`](0020-tenant-security-and-trust.md)'s `TenantTxScope`
  must be honoured by every call site or RLS will be off — same
  risk that exists for 0032 and every other tenant-scoped table.
  We mitigate by re-checking `team_id` membership inside the
  port, not only at the gateway.
- Auto-priming costs tokens. An over-eager
  `team_prime_recent_decisions` can dwarf the user's actual
  prompt on tight-context models. We mitigate by gating volume
  per `ModelProfile` and disabling priming entirely for the
  smallest profiles.
- Decision-log restricted-write policy depends on the
  `coding-persona` role being correctly attached to the calling
  agent (ADR [`0033`](0033-coding-agent-personas.md)). A
  misconfigured persona can either lock a team out of its own log
  (overly restrictive) or let an executor commit to it (overly
  permissive). The default `Restricted { Architect, Reviewer }`
  is the tighter side of the trade; operators can opt for `Open`
  if their team's persona shape disagrees.
- Append-only semantics mean the table grows monotonically until
  retention expires. For a long-running team that logs hundreds
  of decisions the auto-prime "most recent 5" pull is fine, but
  `list_decisions(include_superseded = true)` can return a long
  list. We index `(tenant_id, team_id, created_at DESC)` for the
  common case and accept that exhaustive history dumps are rare.
- `TeamId` collisions across tenants are structurally impossible
  (uuid v4) but logically possible if an orchestrator manually
  reuses an id. The tenant-scoped membership check is the
  load-bearing safety net.
- A single team bucket is *not* a substitute for cross-team
  knowledge sharing. Two teams working on related features will
  not see each other's notes. This is intentional for v1 (see
  Open questions) — cross-team is a different trust boundary.

### Neutral / follow-ups

- Promotion paths (agent-private memory ↔ team memory ↔ artifact)
  are out of scope. The agent decides when to call
  `team_remember` vs `remember`; we may revisit if dogfooding
  shows a stable pattern.
- Cross-team / org-level memory ("things this tenant always
  decides") is a future ADR. The cleanest implementation would
  add a third scope `(TenantId,)` rather than weakening team
  isolation.
- Rich diff between the prior and superseding decision (showing
  exactly what changed) is a UX nicety, not part of the port.
  Defer to the Web UI surface in ADR
  [`0017`](0017-webui-chat-client.md).
- A "team retro" tool that summarises decisions and notes at team
  disband is a plausible follow-up; it would consume `list_*`
  via the port and write the summary as an artifact (ADR
  [`0016`](0016-artifact-storage.md)).

## Alternatives considered

- **Store team-shared facts inside `AgentMemory` under a synthetic
  agent id like `team:<uuid>`.** Smallest possible diff. Rejected:
  re-uses an `AgentId`-shaped column for a `TeamId`, weakens type
  safety, and gives no place for the decision-log shape (which is
  not a free-form note). The port-shape divergence between
  free-form recall and append-only decisions is real and deserves
  its own surface.
- **One unified table with a `kind` discriminator (`note` |
  `decision`).** Fewer tables. Rejected: the decision log has
  required structured columns (`summary`, `rationale`,
  `alternatives_considered`, `supersedes`, `superseded_by`) that
  are foreign to a note. A single table either makes those columns
  nullable (loses type safety) or adds a JSONB blob (loses
  queryability). Two tables stay honest about the two shapes.
- **No decision log; only free-form notes.** Aligned with 0032,
  smallest surface. Rejected: the load-bearing benefit of a team
  ADR-style log is the audit trail of *changes of mind* via
  `supersedes`. A free-form note bag does not preserve that.
- **Decision log as an MCP server.** Reuses ADR
  [`0010`](0010-mcp-tool-plane.md). Rejected for the same reason
  ADR [`0032`](0032-agent-memory-and-context-compaction.md)
  rejected memory-as-MCP: tenancy and team-membership enforcement
  must live behind the same RLS boundary as everything else, and
  MCP servers receive `tenant_id` over the wire and trust the
  caller. We may revisit if a need to mount external decision
  logs (e.g., team chat archives) emerges.
- **Auto-prime by default, opt-out.** Aggressive. Rejected: it
  silently changes prompts for every existing agent and balloons
  cost on tight-context profiles. The `ModelProfile`-gated
  default mirrors 0032's `memory_autoload_top_k = None` posture.
- **`Open` decision-write policy as the default.** More permissive,
  smaller config footprint. Rejected: dogfooding the persona set
  in [`0033`](0033-coding-agent-personas.md) suggests executors
  generate ~5× more "interesting moments" than they generate
  decisions worth recording in the log. An open default trains
  the team to write low-signal entries, which trains the
  auto-prime to inject low-signal context, which trains the
  models to ignore the prime entirely. `Restricted` is a stronger
  starting point.
- **Tie team lifetime to a single A2A `Task` and discard on
  completion.** Simplest GC. Rejected: follow-up tasks within the
  same conversation are exactly the case where team memory pays
  off most. The retention window (matched to ADR
  [`0016`](0016-artifact-storage.md)'s artifact pattern) handles
  the audit case as well.
- **Place `TeamId` in `ork-agents` instead of `ork-core`.** Closer
  to the orchestration code that creates teams. Rejected:
  `ork-core` is the canonical home for tenant-scoped identity
  types (`TenantId`, `AgentId`, `TaskId`); `TeamId` belongs in
  the same neighbourhood, and downstream crates including
  `ork-persistence` and `ork-api` need it.

## Affected ork modules

- [`crates/ork-core/src/models/`](../../crates/ork-core/) — new
  `team.rs` defining `TeamId`, `TeamConfig`,
  `DecisionWritePolicy`, decision-log payload types.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/) — new
  `team_memory.rs` with `TeamMemory`, `TeamScope`,
  `DecisionDraft`, `DecisionEntry`, `DecisionFilter`.
- [`crates/ork-core/src/`](../../crates/ork-core/) —
  `InMemoryTeamMemory` impl for tests + default wiring.
- [`crates/ork-persistence/src/postgres/`](../../crates/ork-persistence/) —
  `PgTeamMemory` impl, reuses the `EmbeddingProvider` from
  ADR [`0032`](0032-agent-memory-and-context-compaction.md).
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs) —
  four new native tool registrations
  (`team_remember`, `team_recall`, `team_log_decision`,
  `team_list_decisions`).
- [`crates/ork-agents/src/`](../../crates/ork-agents/) —
  team-membership extraction from `RequestCtx`; auto-prime
  message construction at dispatch (the orchestrator-side
  injection point introduced in detail by ADR [`0045`]).
- [`crates/ork-llm/src/`](../../crates/ork-llm/) — `ModelProfile`
  gains `team_prime_recent_decisions` and
  `team_prime_top_k_notes` (per ADR
  [`0034`](0034-per-model-capability-profiles.md)).
- [`crates/ork-api/src/`](../../crates/ork-api/) — RBAC scope
  catalog entries for `team:memory:read|write`,
  `team:decisions:write`.
- [`migrations/010_team_memory.sql`](../../migrations/) — new
  migration.
- [`docs/adrs/README.md`](README.md) — index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| ork's own ADR set | [`docs/adrs/0001-adr-process-and-conventions.md`](0001-adr-process-and-conventions.md) — immutable-once-accepted decisions with explicit supersedes pointer | `team_decision_log` table, `supersedes` / `superseded_by` columns |
| LangGraph | "shared scratchpad" / shared-state pattern in multi-agent graphs | `team_memory_notes` accessible to all team members |
| AutoGen | "GroupChat" history visible to every participant | `TeamMemory::recall` / `list_notes` over a shared bucket |
| Letta (formerly MemGPT) | "core memory" as agent-shared blocks | per-team free-form notes; structurally similar but team- not agent-keyed |
| Solace Agent Mesh | n/a — SAM has no first-class team-shared memory; agents share only via the artifact service | this ADR is net-new ground beyond SAM parity |

## Open questions

- **Cross-team within a tenant.** Two teams working on related
  features cannot see each other's notes. If dogfooding shows
  this is a real friction, the cleanest fix is a third scope
  `(TenantId,)` (tenant-wide knowledge), not weakening team
  isolation. Defer until a concrete case lands.
- **Decision-log size cap.** Should we cap the table at N decisions
  per team and force compaction (the same pattern as ADR
  [`0032`](0032-agent-memory-and-context-compaction.md)'s
  `ContextCompactor`) above that? Likely yes for very long-running
  teams; the right cap depends on auto-prime cost in production.
- **Embedding the decision log.** Notes are embedded for `recall`;
  decisions are pulled chronologically. If a team accumulates
  enough decisions that "find the decision about X" becomes a
  real query, we will add an embedding column to
  `team_decision_log`. Out of scope for v1.
- **Author attribution under role-restricted writes.** When the
  decision-write policy is `Restricted` and a non-architect
  agent wants to *contribute* to a decision (alternatives it
  considered, evidence), it currently has to call
  `team_remember` instead. A "submit-for-promotion" workflow
  could let an executor draft and an architect commit; deferred
  pending evidence it is needed.
- **Retention defaults under tenant compliance regimes.** Some
  tenants require 7-year retention; others require 30-day
  purge. The 90-day default is a starting point — operators
  override via `TenantSettings`. We may need a tenant-level
  compliance profile that sets retention across all bucketed
  data domains (artifacts, agent memory, team memory) at once.

## References

- ork ADR [`0016`](0016-artifact-storage.md) — artifact lifecycle
  and retention pattern
- ork ADR [`0020`](0020-tenant-security-and-trust.md) — tenant
  isolation, RLS, `TenantTxScope`
- ork ADR [`0022`](0022-observability.md) — telemetry consumer
  for `team.*` events
- ork ADR [`0032`](0032-agent-memory-and-context-compaction.md) —
  per-agent memory; this ADR extends the same provider
  abstraction at a different scope
- ork ADR [`0033`](0033-coding-agent-personas.md) — persona role
  set used by the role-restricted decision-write policy
- ork ADR [`0034`](0034-per-model-capability-profiles.md) —
  `ModelProfile`, auto-prime gating
- ork ADR [`0040`](0040-repo-map.md) — workspace-state surface
  (boundary)
- ork ADR [`0042`](0042-capability-discovery.md) — capability
  discovery, used by team formation
- ork ADR `0045` (planned) — team orchestrator; consumer of
  `TeamId` and the auto-prime path
- LangGraph multi-agent: <https://langchain-ai.github.io/langgraph/concepts/multi_agent/>
- AutoGen GroupChat: <https://microsoft.github.io/autogen/stable/user-guide/agentchat-user-guide/>
- pgvector: <https://github.com/pgvector/pgvector>
