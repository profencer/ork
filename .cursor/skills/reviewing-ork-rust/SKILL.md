---
name: reviewing-ork-rust
description: Reviews Rust code in the ork workspace against ADR contracts and project invariants. Use when reviewing a diff, PR, or implementation under crates/ork-*, or when the user asks for code review against an ADR in docs/adrs/.
---

# Reviewing ork Rust code

The required code-review pass after any ADR implementation, per
[`AGENTS.md`](../../../AGENTS.md) §3 step 3. Also useful for ad-hoc
PR reviews when the change is non-trivial.

## When to use this skill

- "Review this diff against ADR-NNNN."
- "Code review for crates/ork-x changes."
- Dispatched as a `code-reviewer` subagent at the end of an
  implementation session.

## Inputs you need

Before reviewing, make sure you have:

1. The diff (current branch vs `main`, or the staged/unstaged changes).
2. The ADR being implemented (`docs/adrs/NNNN-...md`), specifically
   its `## Decision` and `## Acceptance criteria` sections.
3. The list of files modified.

If any of these are missing, ask the dispatcher before reviewing.

## Severity levels

Match the levels used in [`docs/adrs/METRICS.md`](../../../docs/adrs/METRICS.md):

| Level | Meaning |
| ----- | ------- |
| **Critical** | Bug, security issue, or invariant violation. Must be fixed before the ADR can be Accepted. |
| **Major** | Design or correctness concern that should be fixed before merge but isn't a hard blocker. |
| **Minor** | Maintainability, clarity, or low-risk improvement. May be deferred. |
| **Nit** | Style or preference. Does not count toward `reviewer_findings` in metrics.csv. |

## Review checklist

Run through these in order. Each item needs a one-line verdict in your
final report.

### A. ADR contract compliance

- [ ] Every `## Acceptance criteria` checkbox in the ADR is satisfied
      by the diff. Cite the file:line that satisfies each one.
- [ ] No deviation from `## Decision` (trait signatures, types,
      endpoint paths, topic names match exactly).
- [ ] If the diff goes beyond the ADR scope, flag it as **Major** —
      scope creep belongs in a follow-up ADR, not this one.

### B. Project invariants (from AGENTS.md §3)

- [ ] **A2A-first.** Any new agent code satisfies the A2A surface
      (cards, tasks, parts, streaming, cancel, push) per ADRs 0002 /
      0003.
- [ ] **No Solace.** No `solace`, `solace-pubsub`, or SAC-style
      imports. Sync over Kong, async over Kafka.
- [ ] **MCP for external tools.** New external integrations go through
      `rmcp` (or Kong-routed HTTP if no MCP server exists). No bespoke
      vendor SDKs in `ork-integrations`.
- [ ] **Hexagonal boundaries.** `ork-core` and `ork-agents` do **not**
      import `axum`, `sqlx`, `reqwest`, `rmcp`, or `rskafka` directly.
      Grep the diff for these crate names in those two paths.

### C. Rust correctness and idioms

- [ ] No `unwrap()` / `expect()` in non-test code without a comment
      explaining why the invariant holds.
- [ ] `Result` propagation uses `?`; bespoke error mapping has a
      reason.
- [ ] `async fn` doesn't hold non-`Send` types across `.await` if
      called from a `tokio::spawn` context.
- [ ] No silent error-swallowing (`let _ = thing.await;` without a
      comment).
- [ ] `Arc<Mutex<...>>` only where shared mutation is genuinely
      needed; prefer `Arc<...>` + interior immutability or channels.
- [ ] String allocations on hot paths use `Cow` or `&str` where it
      doesn't hurt readability.

### D. Concurrency and cancellation

- [ ] Anywhere `try_join_all` / `join_all` is used, per-task error
      reporting is intentional (cf. ADR-0011's review).
- [ ] Long-running futures support cancellation (`tokio::select!`
      with a cancel signal, or `Drop` semantics that cleanly cancel).
- [ ] No unbounded channels (`mpsc::unbounded_channel`) without a
      justification.

### E. Tests

- [ ] At least one test exists per acceptance criterion that touches
      runtime behaviour. Smoke tests live at
      `crates/<crate>/tests/<name>_smoke.rs`.
- [ ] Tests do not depend on real network unless gated behind a
      cargo feature (e.g. `reference-server-it`).
- [ ] Property tests (`proptest` or hand-rolled) for byte-level
      operations like UTF-8 truncation, JSON-RPC framing, JWS encoding.
- [ ] Integration tests use `wiremock` for HTTP mocking, `testcontainers`
      for Docker-backed scenarios.

### F. Wire-format and persistence safety

- [ ] Public JSON/wire formats are versioned or backward-compatible.
      A field rename without an alias is **Critical** if the ADR didn't
      authorise a breaking change.
- [ ] Any new SQL migration has a clear up/down (`migrations/` dir).
- [ ] Push-notification payloads, JWS, and crypto code use the
      crates pinned in `Cargo.toml` (no ad-hoc `ring` or `openssl`).

### G. Documentation and observability

- [ ] New public types and functions carry doc comments.
- [ ] New crates have a `lib.rs` summary or `README.md`.
- [ ] `tracing` spans wrap any new async boundary; structured fields
      use `?` or `%` formatting consistently.
- [ ] No `println!` or `eprintln!` outside binaries / tests.

## Output format

Return your findings as a markdown report with this shape:

```markdown
# Code review — ADR-NNNN

## Acceptance criteria coverage

- [x] Criterion 1 — satisfied at `crates/ork-x/src/foo.rs:42-58`
- [ ] Criterion 2 — **not satisfied** (see Critical #1)

## Findings

### Critical

1. **<one-line title>** — `crates/ork-x/src/foo.rs:73`
   <2-3 sentences>. Suggested fix: <one sentence>.

### Major

1. ...

### Minor

1. ...

### Nit

1. ...

## Recommendation

Either:
- **Approve** — all acceptance criteria satisfied, no Critical/Major findings.
- **Approve with follow-ups** — Major findings are non-blocking and
  documented for the ADR's `## Reviewer findings` table.
- **Request changes** — Critical findings present.
```

## Report length

Aim for **dense, citation-heavy** rather than long. A good review for a
mid-size ADR is 1-2 pages. Cite `file:line` for every concrete claim.
Vague findings ("this could be cleaner") are not helpful — say what,
where, and what would be cleaner.
