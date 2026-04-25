# ADR loop metrics

[`metrics.csv`](metrics.csv) is a tiny ledger that captures one row per
ADR after it ships. The point is **not** project management — it is to
notice patterns in the ADR-driven work loop over a quarter or two:

- which ADRs slip the most between `Proposed` and `Accepted`,
- which ones consistently get re-rolled by code review,
- whether the loop is getting faster as conventions solidify,
- whether reviewer-finding counts correlate with anything (ADR phase,
  crate, author).

Keeping it as a flat CSV (rather than a database or a richer format)
keeps the cost of maintenance near zero. Open it in your editor of
choice; if you want a dashboard, generate it from this file.

## Schema

| Column | Type | Meaning |
| ------ | ---- | ------- |
| `adr` | string, four-digit | ADR number, e.g. `0011`. |
| `title` | string | Short kebab title, mirrors the ADR filename slug. |
| `proposed_at` | ISO-8601 date | Date the ADR file was first added with status `Proposed`. |
| `impl_started_at` | ISO-8601 date | Date the first implementing commit landed on `main` (or first branch commit if work was branch-isolated). |
| `impl_done_at` | ISO-8601 date | Date the ADR was flipped to `Accepted` / `Implemented`. |
| `reviewer_findings` | integer | Number of distinct findings raised by the required `code-reviewer` pass. Count `Critical`, `Major`, and `Minor`; do not count `Nit`. |
| `reroll_count` | integer | How many times the implementation had to be redone in a substantial way (e.g. spec drift caught mid-implementation, design rethink). 0 is the happy path. |
| `notes` | string, optional | One-liner. Useful for "spike preceded this", "blocked on 0009", "split into 0023a/b mid-flight". Use double-quotes if the note contains commas. |

## When to append

At the same moment you flip the ADR's status to `Accepted` /
`Implemented` and update [`README.md`](README.md). This is documented
in [`AGENTS.md`](../../AGENTS.md) §4 step 4.

## Example row

```csv
0011,native-llm-tool-calling,2026-04-23,2026-04-24,2026-04-25,7,1,"reviewer surfaced peer_* dispatch gap and UTF-8 truncation bug; one re-roll for cancellation handling"
```

## Reading the data

A few simple queries pay rent on this file. From the repo root:

```bash
# Average review-finding count across accepted ADRs
awk -F, 'NR>1 && $5!="" {sum+=$6; n++} END {print sum/n}' docs/adrs/metrics.csv

# ADRs that took longer than 7 days from proposal to accepted
awk -F, 'NR>1 && $3!="" && $5!="" {
  cmd = "echo $(( ( $(date -j -f %Y-%m-%d "$5" +%s) - $(date -j -f %Y-%m-%d "$3" +%s) ) / 86400 ))"
  cmd | getline d; close(cmd)
  if (d > 7) print $1, $2, d, "days"
}' docs/adrs/metrics.csv
```

(The second one is BSD-`date` flavoured for macOS; adjust for GNU
coreutils on Linux.)

## What this file is **not**

- It is **not** a status board. The status board is the index in
  [`README.md`](README.md).
- It is **not** a task tracker. Tasks live in the ADR's
  `Acceptance criteria` and in the in-session todo list.
- It is **not** load-bearing. If the file disappeared, the work loop
  would still function — you would just lose retrospective signal.
