---
name: executing-ork-plans
description: Implements an accepted or proposed ADR in the ork repository following the work loop in AGENTS.md. Use when the user asks to implement, execute, or build out an ADR under docs/adrs/, or refers to an ADR by number with intent to write code.
---

# Executing ork plans

Codifies the implementation loop in [`AGENTS.md`](../../../AGENTS.md) §4
steps 2-4. The ADR is the durable plan; this skill is how an agent
turns it into code without introducing drift.

## When to use this skill

- "Implement ADR-NNNN."
- "Execute the plan in `docs/adrs/NNNN-...md`."
- Any prompt that pairs an ADR reference with implementation intent.

## Hard rules (non-negotiable)

1. **Do not modify the ADR file** during implementation. The ADR is the
   spec; if the spec is wrong, stop and either propose a fix to the
   user or write a superseding ADR. Editing in place violates the
   immutability invariant in
   [`docs/adrs/0001-adr-process-and-conventions.md`](../../../docs/adrs/0001-adr-process-and-conventions.md).
2. **Build the TODO list from the ADR's `## Acceptance criteria`.**
   One TodoWrite item per criterion. Do not invent extra items unless
   they are sub-tasks of a criterion.
3. **Run the verification gate** (§4 below) before claiming any item
   is complete.
4. **Dispatch the `code-reviewer` subagent** before declaring the work
   done. This is the required gate in
   [`AGENTS.md`](../../../AGENTS.md) §3 step 3.
5. **Append a row to `docs/adrs/metrics.csv`** when flipping the ADR
   status. See [`METRICS.md`](../../../docs/adrs/METRICS.md).

## Workflow

### Step 1 — Read the ADR cover-to-cover

Read the entire ADR. Pay particular attention to:

- `## Decision` — the contract you are implementing.
- `## Acceptance criteria` — your TODO list source.
- `## Affected ork modules` — the files you will touch.
- `## Open questions` — unresolved items; flag any blockers immediately.

If the ADR was written before this skill existed and lacks
`## Acceptance criteria`, either ask the user for criteria or derive
them yourself from `## Decision` and `## Affected ork modules`, then
proceed. Do not add the criteria to the ADR itself unless explicitly
asked.

### Step 2 — Build the TODO list

One `TodoWrite` item per acceptance-criteria checkbox, in order. Mark
the first as `in_progress`. If a criterion has substantial sub-steps
(e.g. "trait + 3 implementations + 2 tests"), expand it into multiple
in-session todos.

### Step 3 — Implement, TDD-first where it fits

The `superpowers:test-driven-development` skill applies for any new
behaviour. The general shape is: failing test → minimal impl → green
→ refactor → commit. For pure refactors or rename ADRs, TDD is a
weaker fit; use judgement.

Follow the workspace conventions:

- Domain logic in `ork-core` / `ork-agents` does **not** import
  `axum`, `sqlx`, `reqwest`, `rmcp`, or `rskafka`. If you need one,
  the dependency goes through a port defined in the appropriate
  adapter crate.
- Integration tests live alongside the crate as `crates/<crate>/tests/<name>.rs`.
  Smoke tests use the suffix `_smoke.rs`.
- Public API additions get a doc comment.

### Step 4 — Verification gate

Before marking any TODO complete:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus: run the linter against every modified file (`ReadLints` over the
modified paths in Cursor). All three must be green. If `cargo clippy`
is not configured for this crate, skip it and note that in the
session.

This gate is documented in [`AGENTS.md`](../../../AGENTS.md) §5.

### Step 5 — Required code review

Once all acceptance criteria are met and the verification gate is
green, dispatch a `code-reviewer` subagent:

> *"Review the diff against `docs/adrs/NNNN-...md`. Verify each
> acceptance criterion is met. Surface any issues by severity:
> Critical (must-fix), Major (should-fix), Minor (nice-to-fix), Nit
> (style only). Be specific: cite file:line."*

For each finding:

- **Critical / Major** — fix in-session, then re-run the verification
  gate.
- **Minor** — fix in-session if cheap, otherwise log under the ADR's
  `## Reviewer findings` table with `Acknowledged, deferred` and a
  follow-up reference.
- **Nit** — judgement call.

Update the ADR's `## Reviewer findings` table with one row per
finding (this is the **only** edit to the ADR file allowed during
implementation, and it does not violate immutability because the
section is reserved for post-impl annotations).

### Step 6 — Flip status and update the index

When the verification gate passes after review fixes:

1. Edit the ADR header: `Status: Proposed` → `Status: Accepted` (or
   `Implemented` if the ADR description and code landed together).
2. Edit `docs/adrs/README.md` — change the status badge in the index
   row.
3. Append a row to `docs/adrs/metrics.csv` per the schema in
   [`METRICS.md`](../../../docs/adrs/METRICS.md).

### Step 7 — Commit

The user controls when to commit. Do **not** create commits without
explicit instruction. When asked, group the diff into logical commits:
the ADR status flip + index update + metrics row is its own commit
called something like `docs(adr): mark NNNN as Accepted`.

## Parallel ADRs

If you are running multiple independent ADRs in parallel (see
[`AGENTS.md`](../../../AGENTS.md) §8), use git worktrees so each
session has an isolated working directory. The
`superpowers:using-git-worktrees` skill covers safety. The
verification gate runs **per worktree**.

## Common failure modes to avoid

- **Editing the ADR mid-implementation** because the spec feels wrong.
  Stop instead. Either write a superseding ADR or surface the gap to
  the user.
- **Skipping the code-reviewer pass** because the change feels small.
  Skip nothing: the gate exists because past sessions surfaced real
  bugs in "small" changes (see ADR-0011's review findings).
- **Inventing acceptance criteria not in the ADR** to expand scope.
  Stay inside the box; expansions belong in a follow-up ADR.
- **Forgetting the metrics row.** It is the cheapest part of the loop
  and the only retrospective signal.
