---
name: writing-ork-adrs
description: Drafts an Architecture Decision Record for the ork repository following docs/adrs/ conventions. Use when the user asks to author, propose, or refine an ADR for ork, or asks to add a decision document under docs/adrs/.
---

# Writing ork ADRs

Codifies the workflow established in [`AGENTS.md`](../../../AGENTS.md) §4
step 1 and the long-form process in
[`docs/adrs/0001-adr-process-and-conventions.md`](../../../docs/adrs/0001-adr-process-and-conventions.md).

## When to use this skill

- "Draft an ADR for X."
- "Propose a new decision under `docs/adrs/`."
- "Refine ADR-NNNN before flipping to Accepted."
- "Run the adversarial review pass on ADR-NNNN."

## Quick start

1. Read [`docs/adrs/0000-template.md`](../../../docs/adrs/0000-template.md)
   and [`docs/adrs/README.md`](../../../docs/adrs/README.md) (the index
   tells you the next free `NNNN`).
2. Copy the template to `docs/adrs/NNNN-<kebab-slug>.md`.
3. Fill **every** section. The mandatory sections are listed below.
4. Add the row to the index in `docs/adrs/README.md`.
5. Status starts at `Proposed`.

## Mandatory sections (in order)

| Section | Notes |
| ------- | ----- |
| Header block | Status, Date, Deciders, Phase, Relates to, Supersedes |
| `## Context` | Cite ≥ 1 ork file path with a markdown link |
| `## Decision` | Present-tense imperative; name traits/types/topics; include code or YAML if it crystallises the decision |
| `## Acceptance criteria` | Machine-checkable boxes; the implementing session builds its TODO list from this |
| `## Consequences` | Sub-sections: Positive / Negative / Neutral |
| `## Alternatives considered` | Bullet list with reasons for rejection |
| `## Affected ork modules` | Crate paths with markdown links |
| `## Reviewer findings` | Empty until implementation review lands; table format |
| `## Prior art / parity references` | Optional now; required for parity-driven ADRs (replaces the older `Mapping to SAM` section) |
| `## Open questions` | List, even if just "none" |
| `## References` | Spec URLs, related ADRs, external docs |

## Acceptance-criteria style

Items must be checkable by reading the diff or running a command.
**Good** items:

- [ ] Trait `Foo` defined at `crates/ork-x/src/foo.rs` with the signature
      shown in `Decision`.
- [ ] `crates/ork-x/tests/foo_smoke.rs::happy_path` passes.
- [ ] `cargo test -p ork-x foo::` is green.
- [ ] `docs/adrs/README.md` index row updated.

**Bad** items (do not write these):

- [ ] "Add appropriate error handling."
- [ ] "Refactor as needed."
- [ ] "Make sure tests pass." (cover by the verification gate, do not
      restate)

The verification gate (cargo fmt / cargo test --workspace / lints
clean) is implicit — do not add it as an acceptance criterion. It is
documented in [`AGENTS.md`](../../../AGENTS.md) §5.

## File naming and status

- Filename: `NNNN-<kebab-slug>.md`, four-digit zero-padded, lowercase
  hyphens. Do not reuse numbers.
- Status lifecycle: `Proposed` → `Accepted` (or `Implemented`) →
  optionally `Superseded by NNNN` or `Deprecated`.
- Once `Accepted`, the file is **immutable**. To change a decision,
  write a new ADR with `Supersedes: NNNN` and flip the original to
  `Superseded by NNNN`. Broken links inside an Accepted ADR are
  acceptable historical context.

## Hard invariants to respect

The ADR cannot violate these without being a superseding ADR with
explicit justification. They are restated from
[`AGENTS.md`](../../../AGENTS.md) §3:

1. A2A-first for every agent.
2. No Solace; Kong (HTTP/SSE) + Kafka (async) only.
3. External tools via MCP; native Rust tools stay under `ToolExecutor`.
4. Hexagonal boundaries: `ork-core` and `ork-agents` do not import
   `axum`, `sqlx`, `reqwest`, `rmcp`, or `rskafka` directly.

If the ADR you are drafting bumps into one of these, surface that in
`## Consequences/Negative` and consider whether you actually need a
superseding ADR rather than a new one.

## Spike-first flag

If the decision has ≥ 2 plausible architectures and the right answer
is not obvious from desk research, **spike before finalising the
draft**. See [`AGENTS.md`](../../../AGENTS.md) §6. Capture the spike
findings in `## Alternatives considered`.

## Adversarial review (before flipping to Accepted)

For ADRs with significant downstream commitments (new wire formats,
new crates, security-touching), dispatch a fresh subagent with this
brief:

> *"You are a skeptic reviewing this ADR. Attack the decision: list
> alternatives the author dismissed too easily, consequences they
> understated, what breaks at 10× scale, what this commits the
> codebase to that may be regretted in 6 months. Cite specific
> sections. Do not defend the decision."*

Capture findings in `## Alternatives considered` (alternatives the
skeptic surfaced) and `## Consequences/Negative` (under-stated costs).
The author may rebut, but the rebuttal is recorded inline.

This is documented in [`AGENTS.md`](../../../AGENTS.md) §7. Use the
`generalPurpose` subagent type with a fresh context.

## Self-review checklist before opening a PR

- [ ] At least one ork file path cited via markdown link in `## Context`.
- [ ] Every `## Acceptance criteria` item is concrete and checkable.
- [ ] `## Alternatives considered` has at least two real alternatives,
      not strawmen.
- [ ] If the ADR introduces a wire-format change, `## Decision` carries
      a normative JSON / YAML / Rust snippet.
- [ ] If the ADR introduces a new trait, `## Decision` carries the
      trait signature inline.
- [ ] Status is `Proposed`, date is today, ADR number is the next free
      one in the index.
- [ ] `docs/adrs/README.md` index row added.
