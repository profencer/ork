# AGENTS.md — orientation for AI coding agents and humans

> Read this first, every session. It is the durable contract between the
> repository and any agent (human or AI) that touches it. ADRs in
> [`docs/adrs/`](docs/adrs/) are the source of architectural truth; this file
> tells you how to *work* in the repo without re-deriving conventions from
> scratch each time.

## 1. What ork is

`ork` is a Rust workspace implementing a multi-agent orchestration platform.
The architecture is **A2A-first** (Google's Agent2Agent protocol),
**MCP-based** for external tools, and uses **Kong (HTTP/SSE)** + **Kafka
(async)** instead of the Solace broker.

The full architecture, glossary and reference diagram live in
[`docs/adrs/README.md`](docs/adrs/README.md). The decision graph,
phasing and per-ADR mapping to Solace Agent Mesh are also there.

## 2. Stack and layout

- **Language:** Rust 2024 edition, MSRV 1.85.
- **Async runtime:** `tokio` (full features).
- **HTTP:** `axum` 0.8 + `tower` / `tower-http`.
- **Persistence:** `sqlx` 0.8 (Postgres), `redis` 0.27.
- **Wire:** `reqwest` 0.12 (HTTP/JSON-RPC + SSE), `rskafka` 0.6 (pure-Rust Kafka),
  `rmcp` 0.16 (MCP client).
- **Crypto / push:** `aes-gcm`, `hkdf`, `p256`, `jsonwebtoken`.

Workspace crates (under [`crates/`](crates/)):

| Crate | Responsibility |
| ----- | -------------- |
| `ork-a2a` | A2A protocol types, JSON-RPC, SSE encode/decode |
| `ork-api` | HTTP/JSON-RPC server endpoints |
| `ork-cli` | `ork` CLI binary |
| `ork-core` | Workflow engine, runs, orchestration model |
| `ork-agents` | `Agent` port + local agent implementations |
| `ork-cache` | Cache abstractions (redis-backed) |
| `ork-eventing` | Kafka producer / consumer wrappers |
| `ork-integrations` | Tool executor, code tools, integration adapters |
| `ork-llm` | LLM provider abstractions and clients |
| `ork-mcp` | MCP client and tool catalog |
| `ork-persistence` | Postgres-backed repos + migrations |
| `ork-push` | Push notification signing and delivery |
| `ork-common` | Shared utilities, error types |

Top-level [`workflow-templates/`](workflow-templates/) holds reusable workflow
definitions; [`demo/`](demo/) holds end-to-end scripts; [`migrations/`](migrations/)
holds Postgres migrations.

## 3. Hard invariants — do not violate without a superseding ADR

1. **A2A-first.** Every agent (local or remote) satisfies the A2A surface:
   cards, tasks, typed parts, streaming, cancel, push. See
   [`0002`](docs/adrs/0002-agent-port.md), [`0003`](docs/adrs/0003-a2a-protocol-model.md).
2. **No Solace.** Sync traffic goes through Kong; async through Kafka.
   See [`0004`](docs/adrs/0004-hybrid-kong-kafka-transport.md).
3. **External tools via MCP.** Internal tools stay native Rust under
   `ToolExecutor`. No bespoke per-vendor SDKs in `ork-integrations` if an
   MCP server exists. See [`0010`](docs/adrs/0010-mcp-tool-plane.md).
4. **ADRs are immutable once Accepted.** To change one, write a superseding
   ADR. See [`0001`](docs/adrs/0001-adr-process-and-conventions.md).
5. **Hexagonal boundaries.** Domain logic in `ork-core` and `ork-agents`
   does not depend on `axum`, `sqlx`, `reqwest`, `rmcp`, or `rskafka`
   directly — it goes through ports defined in those crates.

## 4. The work loop

The repository is built one ADR at a time using this loop. Plans are
**not** committed; the ADR is the durable source of truth.

```
       write ADR              implement              review
          │                       │                    │
          ▼                       ▼                    ▼
  docs/adrs/NNNN-*.md ─►  in-session plan  ─►  code-reviewer subagent
       (Proposed)         (TodoWrite list)        (required gate)
          │                       │                    │
          └──────► Accepted ◄─────┴──── findings addressed ─┘
```

### Step 1 — Write the ADR

Copy [`docs/adrs/0000-template.md`](docs/adrs/0000-template.md). Fill **every**
section, including the new `Acceptance criteria` block (machine-checkable
items) and `Reviewer findings` (initially empty). Status starts as
`Proposed`. Add the row to [`docs/adrs/README.md`](docs/adrs/README.md).

For ambiguous ADRs, do a **spike first** (see §6).

For decisions that lock in long-term commitments, run the **adversarial
ADR review** (see §7) before flipping to `Accepted`.

### Step 2 — Implement

Open a fresh session with the ADR attached. The standard prompt template:

> *"Refer to [`docs/adrs/0001-adr-process-and-conventions.md`](docs/adrs/0001-adr-process-and-conventions.md)
> and implement [`docs/adrs/NNNN-...md`](docs/adrs/). Build the in-session
> task list from the ADR's `Acceptance criteria`. Do not modify the ADR
> file during implementation. When all criteria are met, run the
> verification gate (§5) and dispatch a `code-reviewer` subagent against
> the diff."*

Skills [`writing-ork-adrs`](.cursor/skills/writing-ork-adrs/SKILL.md) and
[`executing-ork-plans`](.cursor/skills/executing-ork-plans/SKILL.md) encode
this in machine-readable form.

### Step 3 — Review (required)

Implementation is **not done** until a `code-reviewer` subagent has been
run against the diff with the ADR as context, and its findings have been
either fixed in-session or recorded under the ADR's `Reviewer findings`
section with a justification or follow-up ADR reference. See
[`reviewing-ork-rust`](.cursor/skills/reviewing-ork-rust/SKILL.md).

### Step 4 — Flip status and update the index

Change ADR status from `Proposed` → `Accepted` (or `Implemented` for
ADRs whose implementation lands in the same change). Update the
[`docs/adrs/README.md`](docs/adrs/README.md) index row. Append a row to
[`docs/adrs/metrics.csv`](docs/adrs/metrics.csv) (see [`METRICS.md`](docs/adrs/METRICS.md)).

## 5. Verification gate

Before claiming any implementation task is complete, all of these must hold:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # if available
```

Plus: linter check on every file you modified (use the project linter
integration; in Cursor that is `ReadLints` over the modified paths).

If any of these are red, the task is not done.

## 6. Spike mode (for genuinely uncertain ADRs)

When a decision has ≥ 2 plausible architectures and you cannot pick on
paper, **spike before drafting the ADR**:

1. Create a worktree on `spike/<topic>` (see §8).
2. Prototype 1–2 designs in throwaway code. No tests required, no review.
3. Write a short findings note in the ADR draft under `Alternatives
   considered` — what you tried, what surprised you, why one option won.
4. Delete the spike branch (or archive under `spike/archive/`).

The spike is allowed to violate the verification gate; the ADR that
follows is not.

## 7. Adversarial ADR review (optional, before `Accepted`)

For ADRs with significant downstream commitments (new wire formats, new
crates, security-touching), run a **separate** subagent with this brief:

> *"You are a skeptic reviewing this ADR. Attack the decision: list
> alternatives the author dismissed too easily, consequences they
> understated, what breaks at 10× scale, what this commits the codebase
> to that may be regretted in 6 months. Cite specific sections. Do not
> defend the decision."*

Capture the findings inline in the ADR's `Alternatives considered` and
`Consequences/Negative` sections (or in `Open questions` if unresolved).
The author may rebut, but the rebuttal is recorded.

This is the **second pass**; the first pass (`code-reviewer` on the
implementation diff) is required and lives in §3 above.

## 8. Parallel work via git worktrees

ADRs that share no files can be implemented in parallel. Use git
worktrees so each session has an isolated working directory:

```bash
git worktree add ../ork-adr-0019 -b adr/0019-scheduled-tasks main
git worktree add ../ork-adr-0017 -b adr/0017-webui-chat-client main
```

Then run a fresh agent session per worktree. The verification gate
runs independently in each.

The [`using-git-worktrees`](https://github.com/) superpower skill
covers safety (uncommitted-change checks, branch naming) — use it.

## 9. Incidents → tests

Bugs found by **dogfooding** (running ork against its own demo
workflows) are first-class artifacts. The flow is:

1. File a 5-line note under [`docs/incidents/YYYY-MM-DD-<slug>.md`](docs/incidents/)
   using the template in [`docs/incidents/README.md`](docs/incidents/README.md).
2. Convert to a failing test in the relevant crate's `tests/` directory.
3. Either fix in place (link the incident from the commit) or write a
   new ADR if the bug exposes an architectural gap.

The incident note can be deleted after the test lands; it exists to
prevent the bug from being lost between "I noticed it" and "it's
written down."

## 10. Loop metrics

After each ADR ships, append a row to
[`docs/adrs/metrics.csv`](docs/adrs/metrics.csv). The schema and
intended use are documented in [`docs/adrs/METRICS.md`](docs/adrs/METRICS.md).
The goal is to spot patterns: which ADRs slip, which get re-rolled,
whether the loop is getting faster.

## 11. Project skills

Project-local skills live under [`.cursor/skills/`](.cursor/skills/) and
encode this contract for AI agents:

- [`writing-ork-adrs`](.cursor/skills/writing-ork-adrs/SKILL.md) — drafting an ADR
- [`executing-ork-plans`](.cursor/skills/executing-ork-plans/SKILL.md) — implementing one
- [`reviewing-ork-rust`](.cursor/skills/reviewing-ork-rust/SKILL.md) — reviewing the diff

There are no project hooks. A `postToolUse` reminder hook was tried
and removed — it duplicated the contract already encoded here and
burned tokens on every file write. Conventions live in this file and
in the skills above; nothing should be auto-injected per turn.

## 12. What this file does **not** cover

- Per-ADR detail — read the ADR.
- API contracts and wire formats — read the ADR that introduced them.
- How to run the demo — see [`demo/`](demo/) and its scripts.
- Operational runbooks — see [`docs/operations/`](docs/operations/).

If a convention is missing here that you re-explained in three
consecutive sessions, add it here.
