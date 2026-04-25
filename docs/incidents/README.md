# Incidents

Lightweight notes for bugs and surprises discovered while **dogfooding**
ork against its own demo workflows or live use. The point is to convert
real-world friction into committed regression tests instead of losing
the discovery between "I noticed it" and "it's written down."

See [`AGENTS.md`](../../AGENTS.md) §9 for how this folder fits into the
overall work loop.

## Lifecycle

1. **File a note** the moment you observe the bug. Use the template
   below. Keep it under 30 lines — it is a placeholder, not a report.
2. **Convert to a failing test** in the relevant crate's `tests/`
   directory. Reference this incident file in the test's doc comment.
3. **Fix in place** (link the incident from the commit message) **or
   write a new ADR** if the bug exposes an architectural gap.
4. **Delete the incident note** once the test lands. The git history
   preserves it; the live folder should only contain in-flight items.

The folder is intentionally small. If it grows past ~10 files, that is
a signal to triage rather than an invitation to add more structure.

## File naming

`YYYY-MM-DD-<short-slug>.md`, e.g. `2026-04-25-stuck-on-second-step.md`.

## Template

Copy `_template.md` (next to this README) when filing a new incident:

```markdown
# <one-line title>

- **Date observed:** YYYY-MM-DD
- **Reporter:** <name>
- **Affected crate(s):** ork-core, ork-cli, …
- **Severity:** blocker | high | medium | low

## What happened

Two or three sentences. Include the command, prompt, or workflow that
triggered it.

## Expected vs actual

- **Expected:** …
- **Actual:** …

## Reproducer

Smallest sequence (commands, config snippet, terminal excerpt) that
re-produces the issue. If the trigger lives in a session terminal, copy
the relevant lines verbatim with line numbers.

## Hypothesis (optional)

Where you think the bug lives, if you have a guess. One sentence.

## Resolution

Filled in when the regression test lands. Pattern:

- **Test:** `crates/<crate>/tests/<file>::<test_name>`
- **Fix:** commit / PR link, or "ADR-NNNN" if the bug triggered a new
  architectural decision.
- **Closed:** YYYY-MM-DD
```

## Examples of what belongs here

- ork hangs on a workflow step that should complete (the "stuck on
  second step" pattern).
- Streaming output stops mid-token but the agent reports success.
- A2A handshake fails against a specific external client and the
  failure mode is silent.
- A migration rollback corrupts a `task_status` row.

## Examples of what does **not** belong here

- Compile errors caught by the verification gate — fix and move on.
- Feature requests — open a `Proposed` ADR or add to the ADR backlog.
- Bugs already covered by an `Accepted` ADR's `Open questions` — link
  there instead.
