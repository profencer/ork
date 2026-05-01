# 0048 — Strategic pivot: ork as a code-first agent platform on rig

- **Status:** Accepted
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0001, 0002, 0003, 0011, 0012, 0017, 0047
- **Supersedes:** 0014 (already superseded by 0024), 0018, 0022, 0024, 0025, 0026, 0027, 0028, 0029, 0030, 0031, 0032, 0033, 0035, 0036, 0037, 0038, 0039, 0040, 0041, 0042, 0044, 0045, 0046

## Context

Two facts have come together this week and force a direction call:

1. **The 0028–0045 batch is a wishlist tree.** The
   [self-review](../../self-reviews/2026-04-29-adrs-0019-0045-and-pivot.md)
   on the 18 ADRs added in commit `a9fd121` found that ADR 0045 (the
   coding-team headline) sits on a 17-deep transitive dependency chain
   of `Proposed` ADRs, breaking the AGENTS.md §4 "one ADR at a time"
   rule. None of the verbs (shell, file editor, git, transactional
   diffs) ship; the nouns (personas, capabilities, team memory) ship
   on top of nothing. This batch was written under "prove this is
   more than a SAM port" pressure and produced rigor without value.
   The director's "too early" feedback maps to "I do not know what
   this is *for* in our org" — adding 18 more architecture docs does
   not move that needle.
2. **Two external projects converged on the shape ork has been
   approximating from the wrong end.** Mastra
   ([`mastra.ai/docs`](https://mastra.ai/docs)) ships a code-first
   TypeScript platform — a single `new Mastra({ agents, workflows,
   tools, storage, vectors, observability, mcpServers })` registers
   everything; `mastra dev` boots a Hono server that exposes
   `/api/agents/:id`, `/api/workflows/:id`, plus a Studio UI at
   `localhost:4111` that lets you chat with agents, run workflows,
   inspect traces, edit memory, replay eval scorers. Workflows are
   typed (zod) builders with `.then` / `.branch` / `.parallel` /
   `.dountil` / `.dowhile` / `.foreach` / `.map` and native
   `suspend()`/`resume()`. `rig-core` (already in this workspace via
   ADR [`0047`](0047-rig-as-local-agent-engine.md)) ships the Rust
   equivalent of the inner loop: typestate `AgentBuilder`,
   `CompletionModel`, typed `Tool` (`Args: Deserialize`), `Extractor`
   for structured outputs, `PromptHook`, `Pipeline`/`Op`, embeddings
   and vector-store companion crates.

The combination is the missing direction. Ork already ships the hard
parts of the substrate — A2A surface
([`0002`](0002-agent-port.md), [`0003`](0003-a2a-protocol-model.md)),
multi-LLM routing ([`0012`](0012-multi-llm-providers.md)), MCP plane
([`0010`](0010-mcp-tool-plane.md)), generic gateways
([`0013`](0013-generic-gateway-abstraction.md)), Web UI gateway
([`0017`](0017-webui-chat-client.md)), artifacts
([`0016`](0016-artifact-storage.md)), embeds
([`0015`](0015-dynamic-embeds.md)) — but the *user-facing shape* is
YAML-flavoured workflow templates and CLI plumbing, not a
code-first Rust API a developer composes against. Mastra's success
("8.5k GitHub stars in 12 months, raised $20M Series A") proves the
code-first composition shape is what teams want; it is also the
shape that lets us delete the 0028–0045 backlog without losing
anything load-bearing.

## Decision

ork **pivots to a Mastra-shaped, code-first agent platform with rig
as the per-agent engine**. The substrate ADRs that ship code (0001,
0002, 0003, 0006, 0007, 0010, 0011, 0012, 0013, 0015, 0016, 0017)
stay in force. The proposed ADRs listed under `Supersedes` above are
**superseded as a batch**: their problem statements remain valid
historical context but their proposed shapes are replaced by the new
ADR set 0049–0057.

Concretely the new platform shape is:

```rust
// src/main.rs in a hypothetical user project
use ork::{OrkApp, Agent, Workflow, Tool, Memory, Mcp};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = OrkApp::builder()
        .agent(weather_agent())
        .workflow(weather_workflow())
        .tool(weather_tool())
        .mcp_server("docs", mcp::stdio("npx", ["-y", "wikipedia-mcp"]))
        .memory(Memory::libsql("file:./ork.db"))
        .observability(Otel::default())
        .build()?;

    app.serve().await         // ork dev / ork start equivalent
}
```

Each piece is its own ADR (0049–0057). They map onto rig as the inner
engine and onto ork-existing crates (ork-core, ork-llm, ork-mcp,
ork-persistence) as the outer protocol/persistence layer; nothing in
the substrate ADR set is re-shaped. ADR
[`0047`](0047-rig-as-local-agent-engine.md) Phase A is the first
brick (engine swap). Phases B–E in 0047 are folded into the new ADRs
(MCP via rig::tool::rmcp ↔ 0051, native tools as rig::Tool ↔ 0051,
hooks ↔ 0054 + 0058 observability, Extractor ↔ 0052).

The new ADR set is below; cross-references and the supersedes
mapping are in `Affected ork modules` and `Prior art / parity
references` further down.

| ADR  | Title | Mastra concept | rig piece |
| ---- | ----- | -------------- | --------- |
| 0049 | `OrkApp` central registry | `new Mastra({...})` | n/a |
| 0050 | Code-first Workflow DSL | `createWorkflow` / `createStep` | `Pipeline` / `Op` (informative) |
| 0051 | Code-first Tool DSL | `createTool` | `rig::tool::Tool` |
| 0052 | Code-first Agent DSL | `new Agent({...})` | `rig::agent::AgentBuilder` |
| 0053 | Memory: working + semantic | `new Memory({...})` | `EmbeddingsBuilder` + companion crates |
| 0054 | Live scorers + offline evals | `createScorer` / `evaluate` | `Extractor` for judge models |
| 0055 | Studio: local dev UI | Studio at `:4111` | n/a |
| 0056 | REST + SSE server surface | Hono `/api/agents/:id` | n/a |
| 0057 | `ork dev` / `ork build` / `ork start` CLI | `mastra dev/build/start` | n/a |

### Substrate that stays in force

- A2A protocol surface and `Agent` port — [`0002`](0002-agent-port.md),
  [`0003`](0003-a2a-protocol-model.md). Every ork agent (including the
  new code-first builder in 0052) still satisfies the A2A surface so
  remote A2A clients ([`0007`](0007-remote-a2a-agent-client.md)) and
  delegation ([`0006`](0006-peer-delegation.md)) keep working unchanged.
- LLM router and provider catalog — [`0012`](0012-multi-llm-providers.md).
  rig sees one `CompletionModel` per request; ork's router stays the
  source of truth (per ADR [`0047`](0047-rig-as-local-agent-engine.md)).
- MCP plane — [`0010`](0010-mcp-tool-plane.md). Still the canonical way
  to reach external tools; ADR 0051 wraps it in the new code-first DSL.
- Hybrid Kong + Kafka transport — [`0004`](0004-hybrid-kong-kafka-transport.md).
  Out-of-process protocol conversion stays the default for production;
  the code-first surface is the developer-facing shape, not the wire.
- Artifacts and dynamic embeds — [`0016`](0016-artifact-storage.md),
  [`0015`](0015-dynamic-embeds.md). Both already integrated; the new
  ADRs reference them, do not re-derive them.
- WebUI gateway — [`0017`](0017-webui-chat-client.md). Studio (0055) is
  a complementary developer surface; the WebUI chat client is the
  end-user surface. Both can coexist on different routes.

### Substrate that stays Proposed but is reframed

- ADR [`0008`](0008-a2a-server-endpoints.md) (A2A server endpoints) —
  the new server surface (0056) auto-generates additional routes
  (`/api/agents/:id/generate`, `/api/workflows/:id/run`); the A2A
  endpoints become *one* of the routes the server emits. 0008's wire
  contract is unchanged.
- ADR [`0019`](0019-scheduled-tasks.md) — Mastra has cron-style
  triggers; this ADR's shape stays valid and lands as part of 0050.
- ADR [`0020`](0020-tenant-security-and-trust.md), [`0021`](0021-rbac-scopes.md)
  — production-essential and not Mastra-specific. Ship as-is when
  pilot demand justifies; the code-first DSL inherits the same
  tenant scoping.
- ADR [`0023`](0023-migration-and-rollout-plan.md) — process ADR; refresh
  as part of 0048 acceptance to point at 0049–0057 instead of the
  superseded batch.
- ADR [`0034`](0034-per-model-capability-profiles.md) — still real
  tuning gap; ship under the new `Agent` builder's `model` resolver.
- ADR [`0043`](0043-team-shared-memory.md) — concept folds into the
  Memory ADR (0053) as a multi-resource scope; 0043's standalone
  shape is superseded by 0053 but its problem statement informs 0053.

### What is dropped, deliberately

- **Multi-agent autonomous coding** — ADRs 0033, 0038, 0040, 0041,
  0042, 0044, 0045. The self-review's verdict applies: aspirational,
  unpriced, head-to-head with $1B-funded products, and the substrate
  cannot support the demo today. If a real customer needs this in
  6 months, write it then with their data, not now in a vacuum.
- **Coding-only verbs** — 0028 (shell), 0029 (file editor), 0030
  (git), 0031 (transactional diffs). They were written for the
  coding-agent pivot. In the Mastra-shape, these become **ordinary
  tools** declared with `createTool` (0051), implemented in user
  code or a community crate, not first-class platform ADRs. The
  platform's job is to make declaring a tool that runs `git apply`
  trivial; it is not to specify what every tool does.
- **WASM plugin sandbox** — 0024. Wasmtime + WIT is a year of work
  with no current customer. Native Rust plugins via the same
  `createTool` shape cover the use case until a security model
  forces sandboxing.
- **Topology classifier** — 0026. The classifier is itself an agent;
  building it is a user concern, not a platform concern. The
  platform offers `Agent`, `Workflow.branch`, and `Extractor`-style
  typed outputs; users compose a classifier from those.
- **LSP diagnostics, repo map, nested workspaces** — 0037, 0040,
  0041. Coding-agent-specific. Same disposition as 0028–0030.

## Acceptance criteria

This ADR is meta/strategy. Its acceptance criteria are documentation
and index hygiene; the implementation work lives in 0049–0057.

- [x] All ADRs listed under `Supersedes` (the proposed batch) have
      their **status field** updated in-file: each reads
      `Superseded by 0048` **or** `Superseded by` the finer-grained
      successor named in the ADR index — `0050`, `0052`, `0053`, or
      `0054` — where that is sharper than a batch-only stamp.
      Their bodies are **not** otherwise edited; they stand as
      historical context. **`0014`** remains `Superseded by` [`0024`](0024-wasm-plugin-system.md)
      only (already superseded before this batch).
- [x] [`README.md`](README.md) ADR index reflects the supersedes
      mapping in the `Status` column for each affected row.
- [x] [`README.md`](README.md) ADR index has rows for ADRs
      0048–0057 at the adopted phase (0048 **Accepted** at merge;
      0049–0057 **Proposed** until their own ADR loops complete).
- [x] [`README.md`](README.md) decision graph drops the 0033/0038/
      0040/0041/0042/0043/0044/0045 cluster and adds the 0048 →
      0049–0057 fan-out plus the rig (0047) → 0051/0052/0053/0054
      edges.
- [x] [`README.md`](README.md) `Mapping-to-SAM summary` section is
      retitled `Prior art / parity references summary` and gains
      Mastra columns where applicable. (The SAM mapping is still
      true historically; it just stops being the only frame.)
- [x] A short note appended to [`AGENTS.md`](../../AGENTS.md) §1
      ("What ork is") clarifying that ork is now a "code-first
      agent platform with an A2A wire surface", not a "SAM port
      without Solace". The §3 invariants (A2A-first, no Solace, MCP
      for external tools, hexagonal boundaries) are unchanged.
- [x] [`docs/adrs/metrics.csv`](metrics.csv) row appended for 0048.
- [x] No file under `crates/` is edited as part of accepting this
      ADR. Implementation is gated on 0049–0057 each shipping their
      own diffs.

## Consequences

### Positive

- The 18-ADR backlog from `a9fd121` collapses to a 10-ADR runway
  with one direction. The next-thing-to-build for any contributor
  becomes legible: read 0048, then 0049, then ship one ADR.
- ork inherits two strong external priors. Mastra has validated the
  *user-visible shape* (code-first registration, Studio, REST+SSE
  surface, scorers); rig has validated the *engine shape* (typestate
  agent builder, typed tools, structured outputs). We are not
  inventing either tier — we are gluing them with ork's A2A surface
  and tenant model.
- Director's "too early" question has a concrete answer. The
  pilotable artifact is "an ork project on a developer's laptop
  that registers their internal workflow, runs against on-prem
  Postgres+Kafka, and is observable in Studio." That is buildable
  in weeks against shipped substrate, not the 17-deep dependency
  chain in 0045.
- ADR 0047 (rig adoption) was written defensively ("rig stays
  *under* the port"). After this pivot rig is no longer a
  defensive engine swap; it is the answer to several open ADR
  questions (0025 → rig::Extractor, 0039 → rig::PromptHook, 0032's
  loop → rig's history) and 0047's "Phasing toward rig-native"
  becomes the implementation roadmap, not a follow-up that may not
  happen.
- The "differentiator" slide rewrites itself: ork is the *only*
  Rust-native, A2A-first, code-first agent platform that runs
  on-prem against a customer's existing Kong + Kafka + Postgres
  footprint. Mastra is JavaScript and SaaS-leaning; rig is engine-
  only with no platform; LangChain/LangGraph are Python; SAM is
  Solace-coupled. None of these are a Rust on-prem code-first
  agent platform.

### Negative / costs

- **The pivot is a positioning shift, not a deletion.** Code under
  `crates/ork-{core,agents,llm,mcp,api,webui}/` continues to ship
  the substrate; only the *narrative around what's next* changes.
  But the ADR-set shift signals to readers that prior proposed
  ADRs are off-roadmap, and anyone holding a half-built feature in
  one of those (none today, by inspection of `git status`) would
  have to re-justify it under 0049–0057.
- **The Mastra parity story locks ork into chasing Mastra's
  ergonomics.** If Mastra adds a primitive (e.g., real-time voice
  pipelines, agent networks v2), the question "does ork have it?"
  starts to follow us. Mitigation: 0048's parity table commits to
  *concepts*, not feature-by-feature parity. Items not in the
  table are explicitly out of scope until a customer asks.
- **rig-core is a 0.x dependency.** ADR 0047 already documents
  this. Doubling down on rig (0051, 0052, 0054 all assume rig
  primitives) raises the cost of a rig regression. Mitigation:
  every ADR 0049+ has a "rig surface used" line in `References`
  and a "what we'd do if rig drops the surface" line in
  `Open questions`.
- **Studio (0055) and the auto-generated server (0056) are the
  most user-visible pieces and the highest specification risk.** A
  shipped `ork dev` that mostly works but hot-reloads poorly will
  set tone for first impressions. Mitigation: 0055's acceptance
  criteria require parity with `mastra dev`'s observable behaviour
  (chat with an agent, run a workflow, inspect a trace) before
  shipping the cosmetic dashboards.
- **ICP / buyer tension.** On-prem Rust + Kong/Kafka + compliance
  tracks often align with infra/security sponsors; Mastra-shaped DX
  and Studio align with product-engineering velocity. Both can be
  true, but the pitch must not assume a single buyer persona without
  pilot evidence (see adversarial review capture in `Open questions`).
- **Phase 4 runway vs near-term pilot.** Full delivery across
  0049–0057 is a multi-quarter surface; a thin vertical (one agent +
  one workflow + minimal REST) may land in weeks on existing
  substrate. Without an explicit minimal-ADR subset, platform work
  can crowd out named pilot gaps — capacity sequencing is a
  follow-up on every 0049+ ADR, not just this meta doc.
- **Drops 18 written ADRs of "potential" surface.** Rebutted: per
  the self-review they were liabilities, not assets. The work
  remains in git history and can be revived if a customer ever
  asks.

### Neutral / follow-ups

- The Mastra-style API is the **developer-facing** surface; the
  A2A wire surface is the **production** surface. Both stay. A
  user can register an agent with `OrkApp::builder().agent(...)`
  and immediately have it (a) callable via `app.local_agents()`,
  (b) reachable from another mesh node via A2A JSON-RPC, (c)
  exposed at `/api/agents/:id/generate` over Kong, (d) usable in
  Studio. That trinity is what makes this Rust + on-prem + A2A
  position defensible.
- ADR `0023-migration-and-rollout-plan` should be refreshed once
  0049–0057 land; today its sequence assumes the 0028–0045 batch.
- ADR `0008-a2a-server-endpoints` (still Proposed) needs a small
  delta against 0056 — not superseded, but they share routes.
- A future ADR (0058+) can address vector-store first-class
  integration once 0053 ships and a customer needs more than the
  in-memory baseline.

## Alternatives considered

- **Stay on the SAM-parity narrative; ship 0028–0045 in order.**
  Rejected. The self-review priced the dependency chain at 17 deep
  with no shipped value at any cut point and no customer attached.
  The director's feedback is the trigger to admit this is not the
  right roadmap, not to build harder.
- **Pivot to Mastra parity *without* rig — write our own engine.**
  Rejected. ADR 0047 already chose rig for the engine on its own
  merits and the spike validated the bridge surface. Rewriting the
  engine while pivoting the platform multiplies risk and removes
  rig's monthly free updates from the calculus.
- **Pivot to rig + roll our own platform shape (no Mastra parity).**
  Rejected. The prior batch did produce a pile of `Proposed` ADRs
  without incremental shipping — that is partly a **process /
  scoping** failure, not proof that *every* non-Mastra platform shape
  must repeat it. Mastra's shape still won on **evidence of
  adoption** for the code-first registry pattern versus inventing
  our own nouns from scratch; if we had paired a smaller ADR funnel
  with a different frame, the counterfactual is unknowable.
  Practically: copying Mastra's *concepts* is the chosen shortcut
  until a customer proves an incompatible frame.
- **Stay narrowly on autonomous-coding agents and bet that
  Cursor/Claude Code/Devin all stumble.** Rejected on competitive
  grounds (the self-review priced this); also rejected on
  positioning grounds (a Rust on-prem A2A coding agent is a
  narrower market than a Rust on-prem A2A *platform* on which
  coding agents are one possible workload).
- **Wrap Mastra (the JavaScript runtime) instead of building
  Rust-native parity.** Rejected. Defeats the on-prem Rust pitch,
  imports Node.js operational footprint, breaks the hexagonal
  invariant in ADR `0001` §3 about which deps `ork-core` may
  touch, and blocks the rig adoption that ADR 0047 already paid
  for.

## Affected ork modules

This ADR is documentation-only. The new ADRs change code:

- [`docs/adrs/`](.) — 0048 lands; 0049–0057 follow; ADRs in the
  `Supersedes` list get their status flipped in their headers
  only.
- [`docs/adrs/README.md`](README.md) — index, decision graph,
  parity table refresh.
- [`AGENTS.md`](../../AGENTS.md) — §1 line refresh.

Affected crates listed by dependent ADR (no edits in this PR):

- [`crates/ork-agents/`](../../crates/ork-agents/) — engine swap
  (already 0047), agent DSL (0052), tool DSL (0051).
- [`crates/ork-core/`](../../crates/ork-core/) — workflow DSL
  (0050), `OrkApp` registration (0049), memory port (0053).
- [`crates/ork-api/`](../../crates/ork-api/) — auto-generated REST
  + SSE surface (0056), Studio mount (0055).
- [`crates/ork-cli/`](../../crates/ork-cli/) — `ork dev` / `build`
  / `start` (0057).
- [`crates/ork-persistence/`](../../crates/ork-persistence/) —
  memory backend (0053), workflow snapshots (0050), scorer table
  (0054).
- New surface crate (proposed in 0049): `crates/ork-app/` or
  `crates/ork-runtime/` — the user-facing entry crate.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass. For
this meta-ADR the review is over the documentation diff (0048 +
index + status flips) rather than over Rust code; the reviewer's job
is to confirm the supersedes mapping is internally consistent and
that no Accepted ADR was incorrectly marked.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Minor | `metrics.csv` lacked a `0048` row at review time (`docs/adrs/metrics.csv`). | Appended row at acceptance; counts 3 Minor findings from this pass. |
| Minor | README `Phases` table listed **0017** in Phase 2 while the index (and ADR 0017 header) said Phase 4 (`docs/adrs/README.md:97`, `:123`; `docs/adrs/0017-webui-chat-client.md:5`). | Aligned **0017** to Phase **2** in the index and ADR header to match the post-pivot phase table. |
| Minor | Acceptance §190–193 literally required every superseded body to read `Superseded by 0048`; repo correctly uses finer successors **0050 / 0052 / 0053 / 0054** where sharper (`docs/adrs/0048-pivot-to-code-first-rig-platform.md:190–198`). | Criterion text updated to match the successor convention; README index + file headers remain canonical. |
| Nit | `0033` called "dropped" in `What is dropped` while its header is `Superseded by 0052` (`docs/adrs/0048-pivot-to-code-first-rig-platform.md:161–166` vs `0033-coding-agent-personas.md:3`). | Acknowledged, deferred — nearest landing zone vs rhetorical "dropped"; no status change. |
| Informational | Adversarial review (AGENTS.md §7) — Mastra-as-anchor evidence, rig 0.x horizon, buyer/ICP fit, Phase 4 vs pilot sequencing. | Non-blocking pass 2026-05-02; bullets added under `Consequences / Negative`, `Alternatives considered`, and `Open questions` below. |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Mastra docs](https://mastra.ai/docs) | the platform shape this ADR adopts |
| Mastra | [Mastra class reference](https://mastra.ai/reference/core/mastra-class) | ADR 0049 `OrkApp` |
| Mastra | [Workflows overview](https://mastra.ai/docs/workflows/overview) | ADR 0050 |
| Mastra | [Tools (createTool)](https://mastra.ai/reference/tools/create-tool) | ADR 0051 |
| Mastra | [Agents reference](https://mastra.ai/reference/agents/agent) | ADR 0052 |
| Mastra | [Memory overview](https://mastra.ai/docs/memory/overview) | ADR 0053 |
| Mastra | [Evals overview](https://mastra.ai/docs/evals/overview) | ADR 0054 |
| Mastra | [Studio + dev server](https://mastra.ai/reference/cli/mastra) | ADRs 0055 + 0057 |
| Mastra | [Server overview](https://mastra.ai/docs/server/mastra-server) | ADR 0056 |
| rig | [`rig::agent`](https://docs.rs/rig-core/latest/rig/agent/index.html) | ADRs 0047, 0052 |
| rig | [`rig::tool`](https://docs.rs/rig-core/latest/rig/tool/index.html) | ADR 0051 |
| rig | [`rig::extractor`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html) | ADRs 0052 (output schema), 0054 (judge model) |
| rig | [`rig::pipeline`](https://docs.rs/rig-core/latest/rig/pipeline/index.html) | ADR 0050 — informative only; ork keeps its own engine |
| rig | [`rig::embeddings`](https://docs.rs/rig-core/latest/rig/embeddings/index.html) | ADR 0053 |
| Solace Agent Mesh | (the original parity target) | retired as the primary frame; ADR 0001's process invariants stay |

### Phasing after the pivot

The README phase table (Phase 1–4) is authoritative after this ADR
merges. It reads:

| Phase | ADR range | Theme |
| ----- | --------- | ----- |
| 1 | 0001–0009 | Foundations: ADR process, A2A surface, transport, discovery |
| 2 | 0010–0017 | Substrate: MCP, LLM, gateways, embeds, artifacts, WebUI |
| 3 | 0019–0023, 0034, 0047 | Production hardening: scheduling, security, RBAC, rollout, model profiles, rig engine |
| 4 | 0048–0057 | Code-first platform: pivot ADR + the Mastra-shaped surface |

The README index rows and decision graph mirror this table.

## Open questions

- **Naming.** `OrkApp` vs `OrkRuntime` vs simply `App` for the
  central registry. ADR 0049 makes the call; 0048 is consistent
  with whichever 0049 picks.
- **Should 0046 (evaluation harness) be folded into 0054 (live
  scorers + offline evals) or kept as a sibling?** This ADR marks
  0046 as superseded by 0048 at the batch level; 0054 is the
  successor doc. The cleaner shape is "0054 supersedes 0046";
  0048's batch-supersede is shorthand. ADR 0054's header should
  carry the explicit `Supersedes: 0046`.
- **Cron / triggers** in 0019 — should they be a top-level ADR
  primitive or a `Workflow` builder method? Mastra encodes them at
  the Workflow level. Probably folds into 0050; deferred to 0050's
  draft.
- **A2A surface coupling.** Every ADR 0049+ assumes new agents
  satisfy the A2A surface "for free" by going through the existing
  `Agent` port. A spike during 0052 should confirm the rig
  builder produces an A2A-compatible agent without ergonomic
  pain. If it does not, 0052 must commit to a thin adapter; if
  the pain is severe enough, 0048 has to revisit the A2A-first
  invariant under AGENTS.md §3.
- **Mastra as category anchor.** Stars/VC momentum are weak evidence
  of *product* fit for our ICP; TS-first Mastra may set a DX bar
  Rust cannot match feature-for-feature. Mitigation remains
  *concept* parity (Decision table), not screenshot parity — but
  we should validate with pilots, not narrative alone.
- **rig `0.x` in ~18 months.** If `AgentBuilder`, `Tool`, or
  `Extractor` surfaces churn, user-facing 0051/0052/0054 APIs absorb
  breakage unless we commit to a pinning/fork policy and semver
  story for `ork` consumers. Defer detail to each child ADR's
  `Open questions`.
- **Buyer persona / procurement.** Infra-first (on-prem, Kong/Kafka)
  sponsors may differ from teams chasing Mastra-like iteration speed;
  document per-pilot champion role rather than assuming one "ork
  buyer."
- **Minimal ADR subset for first champion pilot.** Phase 4 lists
  0049–0057 as a runway; sequencing *must-have for a paid/champion*
  vs *table stakes platform* belongs in 0049 kick-off and rollout
  refresh (`0023`), else platform polish can crowd out named pilot
  gaps.
- **Adversarial review (AGENTS.md §7).** Completed 2026-05-02
  (non-blocking). Themes above subsume the original four attack
  prompts; no further pass required for *this* doc merge.

## References

- Mastra documentation: <https://mastra.ai/docs>
- Mastra CLI reference: <https://mastra.ai/reference/cli/mastra>
- Mastra server adapters announcement:
  <https://mastra.ai/blog/mastra-server-adapters>
- Mastra Studio (in CLI reference): <https://mastra.ai/reference/cli/mastra>
- rig-core docs: <https://docs.rs/rig-core/latest/rig/>
- ADR [`0001`](0001-adr-process-and-conventions.md) — process; this
  ADR follows it.
- ADR [`0002`](0002-agent-port.md), [`0003`](0003-a2a-protocol-model.md)
  — A2A surface preserved.
- ADR [`0047`](0047-rig-as-local-agent-engine.md) — rig adoption;
  this ADR makes it the engine for the new platform shape, not just
  a `LocalAgent` swap.
- Self-review that triggered the pivot:
  [`self-reviews/2026-04-29-adrs-0019-0045-and-pivot.md`](../../self-reviews/2026-04-29-adrs-0019-0045-and-pivot.md).
