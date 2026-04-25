# 0001 — ADR process and repository conventions

- **Status:** Accepted
- **Date:** 2026-04-24
- **Deciders:** ork core maintainers
- **Phase:** 1
- **Relates to:** all subsequent ADRs

## Context

ork is about to absorb a substantial pile of architectural changes (A2A, mesh transport, plugins, gateways, MCP, embeds, artifacts). Without a lightweight ADR practice, those decisions will scatter across PR descriptions and design docs in [`future-a2a.md`](../../future-a2a.md). The current repository has a long [`README.md`](../../README.md) and an inline plan in [`future-a2a.md`](../../future-a2a.md) but no ADR convention. Solace Agent Mesh similarly relies on [`docs/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/docs) prose, which makes intent hard to diff later.

We want a format that is:

- short enough that engineers actually write them;
- traceable to specific Rust files and SAM equivalents (the parity work is comparative by nature);
- immutable once accepted, with explicit `Supersedes` chains.

## Decision

ork adopts **MADR-lite** ADRs under `docs/adrs/`.

- File naming: `NNNN-<kebab-slug>.md` with zero-padded four-digit number; `0000-template.md` is the canonical template.
- Every ADR uses the section list in [`0000-template.md`](0000-template.md): `Status`, `Date`, `Deciders`, `Phase`, `Relates to`, `Supersedes`, `Context`, `Decision`, `Consequences` (Positive / Negative / Neutral), `Alternatives considered`, `Affected ork modules`, `Mapping to SAM`, `Open questions`, `References`.
- Status lifecycle: **Proposed → Accepted → (Superseded by NNNN | Deprecated)**. ADRs are never deleted or rewritten in place after `Accepted`. To revise an accepted decision, write a new ADR that supersedes it and update the original's status to `Superseded by NNNN`.
- Every ADR **must** cite at least one ork file path and one SAM equivalent. This makes the parity matrix machine-readable later (the [`README.md`](README.md) `Mapping to SAM` table aggregates them).
- The [`README.md`](README.md) index is the source of truth for which ADRs are accepted; PRs that add an ADR also add the index row.
- ADRs that touch wire-protocol formats (`0003`, `0004`, `0008`, `0009`) include a normative JSON or YAML snippet under `Decision`.
- ADRs that introduce a new crate or trait include the trait signature inline.

## Consequences

### Positive

- New maintainers can read `docs/adrs/` top-to-bottom and understand why ork looks the way it does.
- The supersedes chain gives us an audit log without rewriting history.
- The `Mapping to SAM` table doubles as a feature-parity tracker.

### Negative / costs

- Writing an ADR adds friction to architectural PRs (this is the intended trade-off).
- Maintaining the [`README.md`](README.md) index requires one more file edit per ADR PR.

### Neutral / follow-ups

- A future ADR may decide to autogenerate the parity matrix from the per-ADR `Mapping to SAM` tables.

## Alternatives considered

- **No ADRs, prose in PR descriptions only.** Rejected: PR descriptions are not searchable for non-committers and do not survive squash merges cleanly.
- **Confluence / Notion external store.** Rejected: separates decisions from the code they govern; breaks `cargo doc`-style local browsing.
- **Full MADR with all optional sections.** Rejected as too heavyweight for a single-team repo at this stage. We can graduate later.

## Affected ork modules

- `docs/adrs/` — new directory.
- [`README.md`](../../README.md) — add a one-line pointer to `docs/adrs/` (handled in a follow-up doc PR).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| ADR practice | none — SAM has no ADRs | This file establishes the convention. |

## Open questions

- Do we want a `Threat model` section for security-touching ADRs (`0020`, `0021`)? Currently rolled into `Consequences/Negative`.

## References

- [MADR](https://adr.github.io/madr/)
- [Michael Nygard — Documenting architecture decisions](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
