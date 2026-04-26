# Claude Code orientation for ork

This repo's contract for AI agents lives in [AGENTS.md](AGENTS.md). Read it
every session — it is the source of truth for architecture, the work loop,
the verification gate, and project invariants.

@AGENTS.md

## Claude-Code-specific notes

- Project skills live under [.claude/skills/](.claude/skills/), mirroring
  [.cursor/skills/](.cursor/skills/). Invoke them via the Skill tool when
  drafting an ADR (`writing-ork-adrs`), implementing one
  (`executing-ork-plans`), or reviewing a diff (`reviewing-ork-rust`).
- The mandatory post-implementation review (AGENTS.md §3 step 3) is
  dispatched via the `code-reviewer` subagent defined at
  [.claude/agents/code-reviewer.md](.claude/agents/code-reviewer.md).
- No project hooks. AGENTS.md §11 explains why a `postToolUse` reminder
  hook was tried and removed; do not re-introduce one.
