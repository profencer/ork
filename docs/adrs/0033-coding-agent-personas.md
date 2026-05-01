# 0033 — Coding agent personas and solo reference

- **Status:** Superseded by 0052
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0002, 0005, 0006, 0007, 0011, 0017, 0025, 0027, 0028, 0029, 0030, 0031, 0032, 0034, 0036, 0038, 0042, 0045
- **Supersedes:** —

## Context

ADRs [`0028`](0028-shell-executor-and-test-runners.md) –
[`0032`](0032-agent-memory-and-context-compaction.md) gave ork the
*verbs* a coding agent needs: a sandboxed shell with structured
test-runner output, a transactional file editor, git operations, a
rollback boundary, and tenant-scoped memory with context compaction.
What is still missing is the *agent shape* that puts those verbs in a
loop: a configured-and-named entity with a system prompt, a tool
allow-list, an LLM model assignment, an edit format, and a phase tag —
the difference between "ork can run `cargo test`" and "ork has a
`solo_coder` agent that can fix a failing test from a clean checkout."

The team orchestrator described in the upcoming ADR [`0045`] composes
multiple such agents into a developer team (architect plans, executor
edits, reviewer reviews, tester tests). For that orchestrator to pick
agents at runtime, agents must advertise their *role*, the languages
they handle, the tools they expect, and the phase they belong to in a
shape ADR [`0042`]'s capability discovery can consume. Today the only
metadata an agent publishes is the free-form `skills[]` array on its
`AgentCard` ([`crates/ork-a2a/src/card.rs`](../../crates/ork-a2a/src/card.rs),
shaped in ADR [`0005`](0005-agent-card-and-devportal-discovery.md)) —
that is sufficient for human browsing in DevPortal but too loose for
machine routing.

A second concrete consumer is sitting on the doorstep: the
*plan-verification* protocol owned by the upcoming ADR [`0038`].
Every team flow gates an architect's plan through N independent peer
reviewers before the executor is allowed to touch source. Those peer
reviewers are themselves agents (one per A2A endpoint), and they need a
contract — `(plan, repo_context) → verdict` — that does not depend on
the team orchestrator existing yet. They also need to be reusable from
*solo* flows, where a single coding agent voluntarily asks one or more
remote `plan_verifier` peers to review its own plan before editing.

We have also been running into a churn point at the prompt layer:
[`AgentConfig`](../../crates/ork-agents/src/local.rs) holds a
`system_prompt: String` and a `tools: Vec<String>` allow-list, but
there is no place to declare *what role this agent is meant to play*,
*what languages it understands*, or *what edit format* (unified-diff
vs. whole-file vs. search-replace block) its prompt was tuned for.
Every new coding agent we have stood up in the demo
([`demo/langgraph-agent/`](../../demo/langgraph-agent/)) has rebuilt
that metadata informally inside the prompt string itself, which makes
it un-discoverable.

This ADR fixes both gaps in the smallest way that does not pre-commit
the team orchestrator's design: a typed *persona* descriptor, an agent
card extension that advertises it, four reference personas (one of
them — `plan_verifier` — fully fleshed out because ADR [`0038`] needs
it now), and a solo workflow template plus a demo script that drives
the whole thing end-to-end against the web UI from ADR
[`0017`](0017-webui-chat-client.md).

## Decision

ork **introduces** a `CodingPersona` descriptor and a small registry
of reference personas in `ork-agents`. A persona is the configuration
that turns a generic [`LocalAgent`](../../crates/ork-agents/src/local.rs)
into a named coding agent: it bundles the system prompt, the tool
allow-list, a reference to a model profile (ADR [`0034`]), an edit
format (ADR [`0029`](0029-workspace-file-editor.md)), and a default
phase (ADR [`0038`]). Personas are advertised on the agent card via a
new ork extension URI so ADR [`0042`]'s capability discovery can pick
them up without further wire changes.

A **solo coding agent** is the degenerate team-of-one case: a single
`LocalAgent` instantiated from the `solo_coder` persona, optionally
coupled to one or more remote `plan_verifier` peers per ADR [`0038`].
Team composition (which persona does what, when, and how their diffs
are merged) is owned by ADR [`0045`] and is **out of scope** here.

### `CodingPersona` (ork-agents)

```rust
// crates/ork-agents/src/persona.rs

use std::sync::Arc;

use ork_core::ports::agent::AgentId;

/// A reusable role descriptor for coding agents. Combined with a
/// concrete `LocalAgent` instance to produce a runnable, named agent.
#[derive(Clone, Debug)]
pub struct CodingPersona {
    pub id: PersonaId,                 // stable; e.g. "ork.persona.solo_coder"
    pub role: PersonaRole,
    pub display_name: String,          // shown in DevPortal / web UI
    pub languages: Vec<Language>,      // ordered by preference
    pub tool_catalog: ToolCatalog,     // declarative; resolved per-host
    pub system_prompt: SystemPrompt,
    pub edit_format: EditFormat,
    pub default_phase: PersonaPhase,   // see ADR 0038
    pub model_profile_ref: ModelProfileRef, // see ADR 0034
    pub default_compaction_trigger_ratio: f32, // forwarded to ADR 0032
    pub default_memory_autoload_top_k: Option<u32>, // forwarded to ADR 0032
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PersonaRole {
    Architect,
    Executor,
    Reviewer,
    Tester,
    Docs,
    Security,
    PlanVerifier,
    SoloCoder,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PersonaPhase {
    Plan,
    Verify,                            // ADR 0038's plan-verification gate
    Explore,
    Edit,
    Test,
    Review,
    Commit,
    AnyPhase,                          // solo agents that bridge phases
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum EditFormat {
    /// Whole-file rewrites via `write_file` (ADR 0029).
    WholeFile,
    /// Unified-diff hunks via `apply_patch` (ADR 0029).
    UnifiedDiff,
    /// Aider-style search/replace blocks; resolved by the executor to
    /// `update_file` calls.
    SearchReplace,
    /// No edits permitted; read-only personas (verifier, reviewer-stub).
    ReadOnly,
}

#[derive(Clone, Debug)]
pub struct ToolCatalog {
    /// Native tool names (ADR 0011 catalog) the persona may call.
    /// `None` entries are required tools missing on the host — the
    /// registry rejects the persona at boot rather than producing a
    /// silently-degraded agent.
    pub required: Vec<String>,
    pub optional: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SystemPrompt {
    /// Tera template; receives `{{ persona, repo, languages, tools }}`.
    pub template: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelProfileRef {
    /// Resolved by ADR 0034's profile registry. Implementations that
    /// pre-date 0034 fall back to `LlmRouter` defaults.
    pub profile: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Language {
    Rust, Python, TypeScript, JavaScript, Go, Java, Sql, Yaml, Markdown, Other,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersonaId(pub &'static str);
```

The descriptor is pure data: no `dyn Agent`, no `LlmProvider`, no
`ShellExecutor`. The instantiation step lives in a `PersonaInstaller`
that takes a `CodingPersona` plus the host's wired ports and produces
an `Arc<dyn Agent>` registered under an `AgentId` of the operator's
choice (so a tenant may run two `solo_coder` instances side-by-side
under different ids).

### Persona registry and `LocalAgent` integration

```rust
// crates/ork-agents/src/persona.rs (continued)

pub struct PersonaRegistry {
    inner: HashMap<PersonaId, Arc<CodingPersona>>,
}

impl PersonaRegistry {
    pub fn with_defaults() -> Self;            // registers the four references below
    pub fn register(&mut self, p: CodingPersona);
    pub fn get(&self, id: &PersonaId) -> Option<Arc<CodingPersona>>;
    pub fn list(&self) -> Vec<Arc<CodingPersona>>;
}

pub struct PersonaInstaller {
    pub registry: Arc<PersonaRegistry>,
    pub llm_router: Arc<LlmRouter>,            // ADR 0012
    pub tool_catalog: Arc<dyn ToolExecutor>,   // ADR 0011
    pub memory: Option<Arc<dyn AgentMemory>>,  // ADR 0032
}

impl PersonaInstaller {
    /// Build a `LocalAgent` from a persona id and an `AgentId`.
    pub fn install(
        &self,
        persona: &PersonaId,
        agent_id: AgentId,
        overrides: PersonaOverrides,           // optional per-instance prompt extras, tool tweaks
    ) -> Result<Arc<dyn Agent>, OrkError>;
}
```

`AgentConfig` (already at
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs))
gains an additive optional field:

```rust
pub struct AgentConfig {
    // ...existing fields...
    pub persona: Option<PersonaId>,
}
```

When `persona` is set, the installer derives `system_prompt`, `tools`,
`compaction_trigger_ratio`, and `memory_autoload_top_k` from the
persona — but operator overrides on the existing fields still win, so
no current call site changes shape.

### Agent card extension (ADR 0005 surface)

A new ork extension URI is reserved:

```
https://ork.dev/a2a/extensions/coding-persona
```

Card payload (added to the `extensions` array on the agent card —
same surface ADR [`0005`](0005-agent-card-and-devportal-discovery.md)
already uses for `transport-hint` and `tenant-required`):

```json
{
  "uri": "https://ork.dev/a2a/extensions/coding-persona",
  "params": {
    "persona_id": "ork.persona.solo_coder",
    "role": "solo_coder",
    "languages": ["rust", "python"],
    "edit_format": "unified_diff",
    "default_phase": "any_phase",
    "tools": {
      "required": ["read_file", "code_search", "run_command", "run_tests",
                   "write_file", "apply_patch", "git_status", "git_diff", "git_commit"],
      "optional": ["recall", "remember"]
    }
  }
}
```

The wire shape is intentionally a flat snapshot of `CodingPersona` —
not the full `system_prompt` (those can be large and tenant-private)
and not the `model_profile_ref` (operator config, not a discovery
attribute). ADR [`0042`]'s discovery service consumes this extension
and indexes by `(role, languages, edit_format, default_phase)` for
team-orchestrator routing in ADR [`0045`].

The shape is **forward-compatible**: ADR [`0042`] may add fields to
`params`; existing consumers ignore unknown keys per the A2A
extension spec. New `PersonaRole` variants are an additive enum.

### `plan_verifier` persona (fleshed-out reference)

`plan_verifier` is the only non-stub non-solo persona this ADR
implements end-to-end, because ADR [`0038`]'s plan-verification
protocol depends on it. The protocol details (handoff, aggregation,
gate semantics, retry budget) are owned by ADR [`0038`]; this ADR
defines the *persona shape and a reference implementation*.

#### Wire contract

The verifier is a stateless A2A peer. It receives a single A2A `Task`
whose initial message carries one typed `DataPart`
(ADR [`0003`](0003-a2a-protocol-model.md)) of the shape:

```json
{
  "kind": "data",
  "data": {
    "schema": "https://ork.dev/schemas/plan-verification/v1",
    "plan": {
      "objective": "...",
      "steps": [{ "id": "s1", "intent": "...", "files": ["..."], "tools": ["..."] }],
      "acceptance_criteria": ["..."]
    },
    "repo_context": {
      "repo": "<workspace name>",
      "head_commit": "<sha>",
      "languages": ["rust"],
      "memory_excerpt_ids": ["..."]
    },
    "review_brief": "Optional caller-supplied focus, e.g. 'pay particular attention to RLS'."
  }
}
```

It returns a single `DataPart` with the structured verdict:

```json
{
  "kind": "data",
  "data": {
    "schema": "https://ork.dev/schemas/plan-verdict/v1",
    "verdict": "approve" | "request_changes" | "reject",
    "score": 0.0,
    "findings": [
      {
        "severity": "blocker" | "major" | "minor" | "nit",
        "step_id": "s1",
        "path": "/steps/0/files/2",
        "summary": "...",
        "evidence": "Optional excerpt or file:line citation"
      }
    ],
    "suggestions": [
      { "step_id": "s1", "rewrite": "Replace step s1 with..." }
    ],
    "verifier_id": "ork.persona.plan_verifier",
    "model": "<resolved model name>"
  }
}
```

The Rust types live next to the persona:

```rust
// crates/ork-agents/src/personas/plan_verifier.rs

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanVerificationRequest {
    pub plan: Plan,
    pub repo_context: RepoContext,
    pub review_brief: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanVerdict {
    pub verdict: PlanVerdictKind,
    pub score: f32,                       // 0.0..=1.0
    pub findings: Vec<PlanFinding>,
    pub suggestions: Vec<PlanSuggestion>,
    pub verifier_id: PersonaId,
    pub model: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanVerdictKind { Approve, RequestChanges, Reject }
```

The verdict envelope deliberately stops here — *aggregation* of
multiple verdicts (quorum, weighted, cascading, "any blocker fails")
is ADR [`0038`]'s problem.

#### Reference implementation

`crates/ork-agents/src/personas/plan_verifier.rs` ships a `LocalAgent`
preconfigured with:

- **System prompt:** verifier-tuned scaffold that asks for a structured
  verdict and forbids edit suggestions outside the input plan's scope.
  The prompt template renders the plan, the repo context, and the
  review brief verbatim, and instructs the model to call the
  `submit_plan_verdict` tool.
- **Tool catalog (read-only):** `read_file`, `code_search`,
  `get_diagnostics` (a thin wrapper over `cargo check --message-format
  json` exposed via ADR [`0028`](0028-shell-executor-and-test-runners.md)'s
  shell), `get_repo_map` (a structural overview tool that wraps
  `list_tree` + `list_repos` from
  [`code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)),
  and `recall` (ADR [`0032`](0032-agent-memory-and-context-compaction.md),
  read-only). No `write_file`, no `apply_patch`, no `git_*`, no
  `run_command`. The tool catalog is the enforcement boundary; the
  prompt only restates it.
- **Edit format:** `EditFormat::ReadOnly` — the persona is
  installation-time-rejected by `PersonaInstaller` if the host has any
  write tools wired for it.
- **Default phase:** `PersonaPhase::Verify`.
- **Model profile:** `ork.profiles.verifier_default` (ADR [`0034`]);
  intended to default to a smaller / cheaper model than the
  architect's, since verification is not the load-bearing reasoning
  step for cost.
- **Structured-output forcing:** the persona ships a single ADR-0011
  tool, `submit_plan_verdict`, whose JSON Schema mirrors `PlanVerdict`.
  The agent loop terminates on the first call to `submit_plan_verdict`.
  This is the same pattern ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md) uses
  for its run-time output verifier — but for **plans** rather than
  **outputs**, hence the separate ADR. The two are complementary, not
  alternatives: 0025 verifies what an agent produced (a step's typed
  output); 0033's `plan_verifier` reviews what an agent *intends to
  do* before it runs the tools.

#### Solo opt-in to plan verification

Solo flows opt into cross-verification via three composable knobs:

1. **Profile:** `ModelProfileRef::with_plan_verifiers(&[...])` (ADR
   [`0034`]) lists the A2A endpoints of `plan_verifier` peers the solo
   agent should consult before editing.
2. **Persona:** `solo_coder` ships with a `verify_plan_step` opt-in
   default (off) that, when enabled, inserts a `Verify` phase between
   `Plan` and `Edit` driven by ADR [`0038`].
3. **Hook:** the run config exposes a `pre_edit_hook` that, when set
   to `verify_plan`, achieves the same thing without a persona swap.

The semantics of how many verifiers to run, when to halt on
disagreement, and how to surface the verdicts in chat are owned by
ADR [`0038`]. Solo flows that do not opt in skip the `Verify` phase
entirely.

#### Architect → executor handoff in team flows

In team flows owned by ADR [`0045`], the architect's plan is handed
off to the executor through N independent `plan_verifier` peers per
ADR [`0038`]. Each verifier returns a `PlanVerdict`; ADR [`0038`]
aggregates and decides whether to (a) hand the plan to the executor
unchanged, (b) round-trip with the architect for a revision, or (c)
fail the run. This ADR's responsibility ends at *one verifier
producing one verdict*; everything else is ADR [`0038`].

### Reference personas shipped in this ADR

| Persona id                       | Role           | Shape in this ADR                         |
| -------------------------------- | -------------- | ----------------------------------------- |
| `ork.persona.solo_coder`         | `SoloCoder`    | Full implementation; edit format = `UnifiedDiff`. |
| `ork.persona.architect`          | `Architect`    | Full implementation; edit format = `ReadOnly`; produces a `Plan`. |
| `ork.persona.executor`           | `Executor`     | Full implementation; edit format = `UnifiedDiff`; consumes a `Plan`. |
| `ork.persona.plan_verifier`      | `PlanVerifier` | Full implementation; edit format = `ReadOnly`; produces `PlanVerdict`. |
| `ork.persona.reviewer.stub`      | `Reviewer`     | Stub: id and card-extension shape only; body is a TODO panic. Wired by ADR [`0045`]. |
| `ork.persona.tester.stub`        | `Tester`       | Stub. |
| `ork.persona.security.stub`      | `Security`     | Stub. |
| `ork.persona.docs.stub`          | `Docs`         | Stub. |

The four "full" personas (`solo_coder`, `architect`, `executor`,
`plan_verifier`) ship runnable system prompts, tool catalogs, and
default model profiles. The four stubs ship descriptors only — they
exist to (a) reserve persona ids so ADR [`0045`] does not litigate
naming again, and (b) surface in DevPortal as "planned" so the
roadmap is visible.

### Solo workflow template

A new template at
[`workflow-templates/coding-agent-solo.yaml`](../../workflow-templates/)
drives the solo flow:

```yaml
name: coding-agent-solo
version: 1
description: One coding agent fixes a defect from clean checkout to green commit.
inputs:
  - { name: repo,    type: string, required: true }
  - { name: branch,  type: string, default: main }
  - { name: brief,   type: string, required: true }
  - { name: verify,  type: bool,   default: false }   # opt-in plan verification
steps:
  - id: plan
    agent: ork.persona.solo_coder
    phase: plan
    prompt_template: "Plan a fix for: {{ inputs.brief }}"
  - id: verify
    when: "{{ inputs.verify }}"
    agent: ork.persona.plan_verifier   # may be remote; addressed via ADR 0038
    phase: verify
    input: "{{ steps.plan.output }}"
  - id: explore
    agent: ork.persona.solo_coder
    phase: explore
  - id: edit
    agent: ork.persona.solo_coder
    phase: edit
  - id: test
    agent: ork.persona.solo_coder
    phase: test
  - id: review
    agent: ork.persona.solo_coder
    phase: review
  - id: commit
    agent: ork.persona.solo_coder
    phase: commit
```

The template uses persona ids — *not* concrete `agent_id`s — so the
operator can wire a specific deployment of `solo_coder` to it without
changing the template. The `verify` step uses ADR [`0038`]'s gate
semantics; under the hood, the gate may dispatch to one or more
remote verifier peers and aggregate the verdicts before allowing
`explore` to start. The aggregation is **not** owned here.

### Demo: end-to-end script

A new script
[`demo/scripts/stage-10-coding-agent-solo.sh`](../../demo/scripts/)
runs the full flow against a toy crate
([`demo/toy-crate/`](../../demo/) — created by the script if absent):

1. Bootstrap a clean checkout via the existing
   [`demo/scripts/lib.sh`](../../demo/scripts/lib.sh).
2. Boot ork-api with `solo_coder` and a local `plan_verifier`
   pre-installed.
3. Submit a "fix this failing test" task through the web UI gateway
   from ADR [`0017`](0017-webui-chat-client.md).
4. Stream the run in the browser; assert via `curl` that the final
   task state is `Completed` and `git log -1` shows a green commit.

Expected deltas land in
[`demo/expected/stage-10.txt`](../../demo/expected/) and
[`demo/expected/stage-10-tree.txt`](../../demo/expected/), matching
the convention already used by stages 0–9.

### Out of scope

- **Team composition.** Which personas run in what order, how their
  outputs feed each other, and how diffs from multiple executors are
  reconciled — owned by ADR [`0045`]. This ADR ships the *parts*; ADR
  [`0045`] ships the *assembly*.
- **Multi-agent diff aggregation.** Two executors editing overlapping
  hunks is ADR [`0045`]; one executor editing through the
  transactional bundle of ADR
  [`0031`](0031-transactional-code-changes.md) is already covered.
- **Verification protocol semantics.** Quorum, weighting, halt
  conditions, retry budget — ADR [`0038`].
- **Capability-based persona discovery.** The wire surface is reserved
  here; the indexing service is ADR [`0042`].
- **Model profile registry.** ADR [`0034`].

## Acceptance criteria

- [ ] Type `CodingPersona` defined at `crates/ork-agents/src/persona.rs`
      with the fields shown in `Decision`; derives `Clone + Debug`.
- [ ] Enums `PersonaRole`, `PersonaPhase`, `EditFormat`, `Language`
      and the newtypes `PersonaId`, `ModelProfileRef`, `ToolCatalog`,
      `SystemPrompt` defined in the same module.
- [ ] `PersonaRegistry::with_defaults()` registers the four full
      personas (`solo_coder`, `architect`, `executor`,
      `plan_verifier`) and the four stubs
      (`reviewer.stub`, `tester.stub`, `security.stub`, `docs.stub`).
- [ ] `PersonaInstaller::install` returns
      `Err(OrkError::Validation("missing required tool: ..."))` when
      the host lacks any of `tool_catalog.required` for the persona
      — verified by
      `crates/ork-agents/tests/persona_install.rs::missing_required_tool_rejected`.
- [ ] `PersonaInstaller::install` rejects with `OrkError::Validation`
      when the persona's `EditFormat` is `ReadOnly` but the wired
      tool catalog includes any of `write_file`, `apply_patch`,
      `git_commit`, or `run_command` — verified by
      `crates/ork-agents/tests/persona_install.rs::read_only_persona_rejects_write_tools`.
- [ ] `AgentConfig::persona: Option<PersonaId>` field added; existing
      call sites compile against `AgentConfig::default()` unchanged.
- [ ] `Agent::card()` for any agent built via `PersonaInstaller`
      includes an extension entry with
      `uri = "https://ork.dev/a2a/extensions/coding-persona"` and the
      params shape shown in `Decision` — verified by
      `crates/ork-agents/tests/persona_card.rs::extension_present`.
- [ ] `PersonaCardExtension` deserialiser at
      `crates/ork-a2a/src/extensions/coding_persona.rs` round-trips
      the example payload in `Decision` — verified by
      `crates/ork-a2a/tests/extensions_coding_persona.rs::roundtrip`.
- [ ] `PlanVerificationRequest`, `PlanVerdict`, `PlanVerdictKind`,
      `PlanFinding`, `PlanSuggestion` defined at
      `crates/ork-agents/src/personas/plan_verifier.rs` with serde
      derives matching the JSON shape in `Decision`.
- [ ] `submit_plan_verdict` registered as a native tool in
      [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
      with a JSON Schema that mirrors `PlanVerdict`.
- [ ] `plan_verifier` integration test
      `crates/ork-agents/tests/plan_verifier_smoke.rs::approves_trivial_plan`
      submits a one-step plan and asserts a `PlanVerdict { verdict: Approve, .. }`
      response, with the verifier driven by a stub `LlmProvider`.
- [ ] `plan_verifier` integration test
      `crates/ork-agents/tests/plan_verifier_smoke.rs::rejects_plan_touching_protected_path`
      submits a plan that proposes editing `.git/config` and asserts
      `verdict: Reject` with at least one `Blocker`-severity finding.
- [ ] `plan_verifier` cannot call any write or shell-exec tool —
      verified by
      `crates/ork-agents/tests/plan_verifier_smoke.rs::no_write_tools_in_catalog`,
      which inspects the descriptor list returned by the installed
      agent's `LocalAgent::tool_descriptors()`.
- [ ] `solo_coder` persona ships with `tool_catalog.required`
      containing `read_file`, `code_search`, `run_command`,
      `run_tests`, `write_file`, `apply_patch`, `git_status`,
      `git_diff`, `git_commit`; `optional` contains `recall`,
      `remember`.
- [ ] `architect` persona has `EditFormat::ReadOnly` and produces a
      `Plan` shape compatible with `PlanVerificationRequest::plan`
      (round-trip test in
      `crates/ork-agents/tests/architect_plan_shape.rs`).
- [ ] [`workflow-templates/coding-agent-solo.yaml`](../../workflow-templates/)
      created with the steps shown in `Decision`; loaded by the
      workflow compiler without errors —
      `cargo test -p ork-core compiler::loads_solo_template`.
- [ ] [`demo/scripts/stage-10-coding-agent-solo.sh`](../../demo/scripts/)
      runs end-to-end against a toy crate, reaches `TaskState::Completed`,
      and produces a single git commit on a feature branch; the
      script's output matches
      [`demo/expected/stage-10.txt`](../../demo/expected/).
- [ ] Stub personas (`reviewer.stub`, `tester.stub`, `security.stub`,
      `docs.stub`) panic with `unimplemented!("ADR 0045")` if
      installed; their *descriptors* round-trip through the registry
      without panic.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0033`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- The "agent shape" for coding work is no longer a free-form prompt
  string baked into a config TOML; it is a typed descriptor with
  fields the team orchestrator (ADR [`0045`]) can route on and the
  capability discovery service (ADR [`0042`]) can index.
- ADR [`0038`]'s plan-verification protocol can land in the next
  ADR cycle without litigating verifier shape — `PlanVerdict` is
  already on the wire.
- Solo coding flows become first-class and demoable end-to-end:
  one persona, one workflow template, one demo script, visible in
  the web UI from ADR [`0017`](0017-webui-chat-client.md).
- The `plan_verifier` persona is reusable from solo and team flows
  identically — same wire contract, same tool catalog, same prompt.
- Persona ids reserved here let ADR [`0045`] and ADR [`0042`] design
  against fixed names, avoiding the "what do we call this thing"
  thrash that has slowed down the demo-script work.
- The persona descriptor is intentionally pure data, so a future
  ADR-0024 WASM plugin can ship a persona without ork-side code
  changes.

### Negative / costs

- We are committing to a wire shape for the
  `coding-persona` extension and the `PlanVerdict` JSON. Both are
  versioned via the `schema` URL on the data part, but breaking
  changes require a new schema and a deprecation window.
- Adding a typed persona layer is one more indirection between
  `AgentConfig` and a running `LocalAgent`. New contributors will
  need to understand "AgentConfig vs persona vs installer" — a
  three-layer mental model where today's is two.
- The `plan_verifier` runs an LLM call per verification. In team
  flows with N verifiers per plan (ADR [`0038`]), the cost adds up.
  Mitigation: the persona's default `model_profile_ref` points at a
  smaller model than the architect's, and ADR [`0038`] caps N. Solo
  flows skip verification by default.
- `EditFormat` is a closed enum. A future format (e.g. AST-typed
  edits) is an additive variant, but bumping it touches every
  persona that switches on it. We accept this; the cost of an open
  string-typed format is worse (typos, undiscoverable defaults).
- Stub personas in code is a code smell. They exist purely so ADR
  [`0045`] can land in a separate session without renaming half the
  registry. The acceptance criteria explicitly mark them
  `unimplemented!`; they are not a hidden half-shipped feature.
- The persona registry is in-process. Cross-process discovery is ADR
  [`0042`]; an operator running two ork-api processes today must
  configure both. This is unchanged from the existing `AgentConfig`
  flow.
- The `submit_plan_verdict` structured-output trick is the same
  pattern as ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  `submit_verdict`. Two near-identical tools is mild duplication;
  collapsing them is a follow-up once both have shipped.

### Neutral / follow-ups

- ADR [`0034`] (model profiles) consumes `ModelProfileRef`. Until it
  lands, `PersonaInstaller` falls back to `LlmRouter` defaults and
  treats the profile name as advisory.
- ADR [`0036`] (planned: tool-catalog manifests for personas) may
  replace the in-tree `tool_catalog: ToolCatalog` field with a
  reference to a manifest file; the field is shaped to absorb that
  swap.
- ADR [`0042`] (capability discovery) consumes the
  `coding-persona` extension and is expected to add filterable
  fields to `params`; the wire contract is forward-compatible for
  additive keys.
- ADR [`0045`] (team orchestrator) consumes the four full personas
  and replaces the four stubs with full implementations.
- A future ADR may surface persona selection in the web UI gateway
  (ADR [`0017`](0017-webui-chat-client.md)) so an end user can pick
  "solo" vs "team" without editing a workflow template.
- Human-in-the-loop approval for plan verdicts (ADR
  [`0027`](0027-human-in-the-loop.md)) is a natural extension —
  `request_changes` could escalate to a human reviewer instead of
  re-prompting the architect. Out of scope here.

## Alternatives considered

- **No persona type — keep configuring agents through
  `AgentConfig` and prompt strings.** Rejected: the team orchestrator
  (ADR [`0045`]) and the capability discovery service (ADR
  [`0042`]) both need to route on role/language/edit-format. A typed
  descriptor is cheaper than parsing prompts at routing time, and it
  is also what makes "list available coding agents" sensible in the
  web UI.
- **Personas as YAML files in `workflow-templates/personas/`,
  loaded at runtime.** Rejected for v1: the four reference personas
  carry tool-catalog invariants (e.g. `plan_verifier` must not have
  `write_file`) that we want type-checked at compile time. A
  follow-up may add a manifest loader for tenant-defined personas;
  the four references stay in-tree because they are part of the
  contract this ADR ships.
- **One generic `verifier` persona used for both step-output
  verification (ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md))
  and plan verification (this ADR + ADR [`0038`]).** Rejected: the
  inputs are different shapes (a step's typed output vs. a plan), the
  prompts are different (rubric-driven vs. risk-and-coverage-driven),
  and the gate semantics are different (run-time vs. plan-time). One
  persona that covers both would be confusingly polymorphic. Sharing
  the structured-output trick (`submit_*` tool) is fine; sharing the
  whole persona is not.
- **Make `plan_verifier` part of ADR [`0038`] instead of this ADR.**
  Rejected: ADR [`0038`] is the *protocol* (how to dispatch, when to
  halt, how to aggregate). The persona is *one peer* in that
  protocol. Coupling them blocks landing the persona for solo flows
  before the protocol is fully designed, and it makes the persona
  shape protocol-specific when it should be reusable.
- **Skip stub personas; let ADR [`0045`] introduce them when it
  lands.** Rejected: ADR [`0042`]'s capability discovery wants to
  index the persona id space at build time; adding the names later
  forces every consumer of the discovery service to handle a moving
  target. Reserving the names now is cheap and unblocks 0042.
- **Express persona capabilities via existing `skills[]` on the
  agent card (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)).**
  Rejected: `skills` is human-facing free-form text designed for
  DevPortal browsing. Overloading it for machine routing leaks human
  copy into a discovery index, and ADR [`0005`] explicitly recommends
  extension URIs for ork-specific machine-readable metadata.
- **Solo workflow as a CLI subcommand instead of a workflow
  template.** Rejected: every other end-to-end flow in
  [`workflow-templates/`](../../workflow-templates/) is YAML, and
  ADR [`0019`](0019-scheduled-tasks.md) wants schedulable runs. A CLI
  shortcut can wrap the template later; the template is the source of
  truth.
- **Use ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  `Verifier` port for plan verification.** Rejected: the `Verifier`
  port runs *inside* the workflow engine after a producing step. Plan
  verification runs *across A2A peers* per ADR [`0038`]; coupling it
  to the in-engine port loses the cross-mesh property. The two
  surfaces stay distinct; a non-A2A in-engine plan checker can still
  be authored as a `Verifier` impl if a future use case wants that.

## Affected ork modules

- New: [`crates/ork-agents/src/persona.rs`](../../crates/ork-agents/) —
  `CodingPersona`, `PersonaRole`, `PersonaPhase`, `EditFormat`,
  `Language`, `PersonaId`, `ToolCatalog`, `SystemPrompt`,
  `ModelProfileRef`, `PersonaRegistry`, `PersonaInstaller`,
  `PersonaOverrides`.
- New: [`crates/ork-agents/src/personas/`](../../crates/ork-agents/) —
  `solo_coder.rs`, `architect.rs`, `executor.rs`,
  `plan_verifier.rs`, plus the four `*.stub.rs` modules.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
  — `AgentConfig::persona: Option<PersonaId>`; resolution order in
  `LocalAgent::new`.
- [`crates/ork-agents/src/lib.rs`](../../crates/ork-agents/src/lib.rs)
  — re-export the persona surface.
- New: [`crates/ork-a2a/src/extensions/coding_persona.rs`](../../crates/ork-a2a/) —
  serde struct + URI constant for the
  `https://ork.dev/a2a/extensions/coding-persona` extension; matching
  unit tests at `crates/ork-a2a/tests/extensions_coding_persona.rs`.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
  — register the `submit_plan_verdict` native tool plus `get_diagnostics`
  and `get_repo_map` (thin wrappers over existing readers/runners).
- [`workflow-templates/coding-agent-solo.yaml`](../../workflow-templates/)
  — new template.
- [`demo/scripts/stage-10-coding-agent-solo.sh`](../../demo/scripts/)
  — new script + matching `demo/expected/stage-10*.txt`.
- [`docs/adrs/README.md`](README.md) — ADR index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Aider | `--edit-format` flag, prompt scaffolding per language | `EditFormat` enum + per-persona system prompt template |
| OpenHands | `Agent` subclasses with declarative tool lists | `CodingPersona` + `ToolCatalog` |
| Claude Code | `.claude/agents/*.md` subagent files (role, prompt, allowed tools) | `CodingPersona` registered in `PersonaRegistry`; same shape, typed |
| LangGraph | `create_react_agent(model, tools, system_prompt)` factory | `PersonaInstaller::install` |
| Solace Agent Mesh | `agent` profiles in `*.yaml` | `CodingPersona` descriptor + reference set |
| GitHub Copilot Workspace | architect / executor split, plan-then-edit | `architect` + `executor` personas; verification gate via ADR [`0038`] |
| Multi-agent debate / verifier ensembles | "ask N independent judges, aggregate verdicts" | `plan_verifier` peer + ADR [`0038`] aggregation |

## Open questions

- **Persona inheritance / composition.** Should personas be
  composable (e.g. `solo_coder` = `architect ∪ executor`)? Stance:
  no, for v1 — composition is convenient but makes prompt provenance
  ambiguous. Revisit when ADR [`0045`] needs it.
- **Per-tenant persona overrides.** Tenants will want to customise
  `system_prompt` and `tool_catalog`. The `PersonaOverrides` struct
  is shaped for this but the override surface (config file?
  DevPortal? API?) is undecided. Defer until ADR [`0045`] surfaces a
  concrete need.
- **Persona versioning.** Today personas are identified by a string
  id; a future deployment that ships two versions of `solo_coder`
  (e.g. `v1`, `v2`) will need either id-suffixing or an explicit
  `version` field. We will add the field when the second version
  lands; the wire shape already accepts unknown keys.
- **Where does the `plan_verifier`'s read-only tool catalog get
  its data?** `read_file` / `code_search` are tenant-scoped
  per ADR [`0028`](0028-shell-executor-and-test-runners.md);
  `recall` per ADR [`0032`](0032-agent-memory-and-context-compaction.md).
  Cross-tenant verifier deployments (an external "ork verifier as a
  service") would have to receive a redacted `repo_context`. Out of
  scope; flagged in ADR [`0042`] follow-ups.
- **Streaming verdicts.** Does a `plan_verifier` stream partial
  findings via A2A SSE while it inspects the repo, or only emit the
  final `PlanVerdict`? Stance: terminal-only for v1 (matches ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md)); ADR
  [`0038`] may add a streaming variant if its UX demands it.
- **Architect plan schema.** This ADR pins the `plan` shape inside
  `PlanVerificationRequest` to what the architect already produces
  in the demo. ADR [`0045`] may want a richer plan (per-step DAG,
  rollback boundaries) — at which point this ADR's
  `https://ork.dev/schemas/plan-verification/v1` URL bumps to `v2`.

## References

- A2A spec — extensions: <https://github.com/google/a2a>
- Aider edit formats:
  <https://aider.chat/docs/more/edit-formats.html>
- Claude Code subagents (project-local skills + agents):
  <https://docs.claude.com/en/docs/claude-code/sub-agents>
- OpenHands agent abstractions:
  <https://docs.all-hands.dev/modules/usage/agents>
- LangGraph `create_react_agent`:
  <https://langchain-ai.github.io/langgraph/reference/prebuilt/>
- Multi-agent verification (Google Research, *Towards a Science of
  Scaling Agent Systems*, April 2025):
  <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/>
- Related ADRs: [`0002`](0002-agent-port.md),
  [`0005`](0005-agent-card-and-devportal-discovery.md),
  [`0006`](0006-peer-delegation.md),
  [`0007`](0007-remote-a2a-agent-client.md),
  [`0011`](0011-native-llm-tool-calling.md),
  [`0017`](0017-webui-chat-client.md),
  [`0025`](0025-typed-output-validation-and-verifier-agent.md),
  [`0027`](0027-human-in-the-loop.md),
  [`0028`](0028-shell-executor-and-test-runners.md),
  [`0029`](0029-workspace-file-editor.md),
  [`0030`](0030-git-operations.md),
  [`0031`](0031-transactional-code-changes.md),
  [`0032`](0032-agent-memory-and-context-compaction.md), 0034
  (forthcoming), 0036 (forthcoming), 0038 (forthcoming), 0042
  (forthcoming), 0045 (forthcoming).
