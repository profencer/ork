# Developer log

Daily-ish narrative notes about what shipped, what surprised us, and
what is in flight. The point is to capture the **order** in which
things happened and the **why** behind each turn — context that ADRs
deliberately strip out and that incident notes are too narrow to hold.

See [`AGENTS.md`](../../AGENTS.md) for how this folder fits into the
overall work loop. The other long-form artifacts in this repo are:

| Folder | What it answers | Lifecycle |
| --- | --- | --- |
| [`docs/adrs/`](../adrs/) | Why we chose this design | Immutable once Accepted |
| [`docs/incidents/`](../incidents/) | This bug surprised us | Deleted once a regression test lands |
| [`docs/devlog/`](.) | What did we actually do today and what's next | Append-only; entries stay |

If a devlog entry would just be "implemented ADR-NNNN", skip it — the
ADR plus its `Reviewer findings` block already tells that story. Write
a devlog entry when:

- multiple work streams (or multiple ADRs) interleaved in one day,
- dogfooding surfaced bugs that span more than one incident note,
- a decision was made that **didn't** justify a new ADR but future
  agents would otherwise re-derive,
- the day ended with non-trivial uncommitted work and the next session
  needs to know where to pick up.

## File naming

`YYYY-MM-DD.md`. One file per day. If you split a day across worktrees
that genuinely diverge, use `YYYY-MM-DD-<slug>.md` (rare).

## Suggested shape

Free-form, but most useful entries cover, in order:

1. **Highlights** — one or two sentences on what the day shipped.
2. **What landed** — bullet list with crate / file / test references.
3. **What surfaced** — bugs found by dogfooding, with incident /
   ADR / test cross-links so a reader can trace each one to its fix.
4. **What is unfinished** — uncommitted scope, open questions, and the
   decision the *next* session should make first.
5. **Verification** — `cargo fmt` / `cargo clippy` / `cargo test`
   summary as of the end of day, plus any test that is intentionally
   skipped (and why).

Keep cross-links absolute (rooted at the repo) so the entry survives
file moves better than relative links.
