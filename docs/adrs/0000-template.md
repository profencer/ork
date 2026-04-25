# NNNN — <Decision title>

- **Status:** Proposed | Accepted | Implemented | Superseded by NNNN | Deprecated
- **Date:** YYYY-MM-DD
- **Deciders:** <names or roles>
- **Phase:** 1 | 2 | 3 | 4
- **Relates to:** NNNN, NNNN
- **Supersedes:** NNNN (if applicable)

## Context

What problem is this ADR addressing? What in ork makes the status quo
painful? Cite at least one ork file path with a markdown link, for
example [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs).

## Decision

The chosen approach, in present-tense imperative language ("ork
**introduces** … ork **adopts** …"). Be concrete: name traits, types,
modules, topics, endpoints, tables. Include short code or YAML snippets
when they crystallise the decision.

## Acceptance criteria

Machine-checkable items the implementation must satisfy. The
implementing session builds its task list directly from this section
(see [`AGENTS.md`](../../AGENTS.md) §4). Every box must be ticked
before flipping the ADR to `Accepted`.

- [ ] Trait / type `<Name>` defined at `crates/ork-<x>/src/<path>.rs` with the
      signature shown in `Decision`.
- [ ] Integration test `crates/ork-<x>/tests/<name>_smoke.rs` passes.
- [ ] `cargo test -p ork-<x> <module>::` is green.
- [ ] Verification gate (`cargo fmt --all -- --check`,
      `cargo test --workspace`, lint clean) passes.
- [ ] Public API documented in the relevant crate's `lib.rs` or
      `README.md`.
- [ ] CHANGELOG entry added under `## Unreleased` (if a CHANGELOG exists).
- [ ] [`README.md`](README.md) ADR index row updated.
- [ ] [`metrics.csv`](metrics.csv) row appended (see [`METRICS.md`](METRICS.md)).

Tailor the list to this ADR; replace placeholders. Items that do not
apply may be removed, but the rationale should be obvious from
`Affected ork modules`.

## Consequences

### Positive

- …

### Negative / costs

- …

### Neutral / follow-ups

- …

## Alternatives considered

- **Option A** — why rejected.
- **Option B** — why rejected.

If the adversarial ADR-review pass (see [`AGENTS.md`](../../AGENTS.md) §7)
surfaced additional alternatives, list them here with the rebuttal.

## Affected ork modules

- [`crates/ork-core/...`](../../crates/ork-core/) — what changes here.
- [`crates/ork-api/...`](../../crates/ork-api/) — what changes here.
- New crate / module, if any.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).
Each finding gets one of:

- **Fixed in-session** — link to the commit / PR that addressed it.
- **Acknowledged, deferred** — link to the follow-up ADR or issue.
- **Rejected** — short justification.

Leave empty until the implementation lands.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

Optional. Use this section when the decision restates or replaces a
concept from another framework (commonly Solace Agent Mesh, but also
LangGraph, ADK, BeeAI, etc.). Earlier ADRs (0002 – 0011) used a fixed
`Mapping to SAM` table; that format is still acceptable but no longer
required for net-new decisions.

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| <name> | path or link | <name> |

## Open questions

- …

## References

- A2A spec: <url>
- Related ADRs: NNNN
- External docs: <url>
