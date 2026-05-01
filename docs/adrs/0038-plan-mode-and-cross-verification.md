# 0038 — Plan mode and A2A plan cross-verification

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0003, 0006, 0007, 0011, 0016, 0017, 0020, 0022, 0025, 0027, 0029, 0030, 0033, 0034, 0035, 0037, 0039, 0040, 0042, 0045
- **Supersedes:** —

## Context

The `LocalAgent` loop in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
hands the model a single tool catalog and lets it call any of those
tools at any iteration. On a frontier hosted model that is roughly
fine — `gpt-5` rarely overwrites `Cargo.toml` because it failed to
parse `cargo check` output. On the weak local models ADR
[`0034`](0034-per-model-capability-profiles.md) targets it is the
single largest source of run failures: a 7B model that misreads a
diagnostic immediately reaches for `apply_patch` or `git_commit`,
mutates the workspace, and the rollback boundary in ADR
[`0031`](0031-transactional-code-changes.md) ends up doing the
heavy lifting *after* damage is done. The same loop also has no
"think before you touch" affordance for *frontier* models when an
operator wants belt-and-braces — every step of an agent run looks
like every other step.

Frontier coding harnesses converged on the same answer: a separate
**plan / explore phase** in which the model has only read-only
tools, must surface a structured plan, and only proceeds to mutating
tools after that plan is approved. Claude Code's `EnterPlanMode` /
`ExitPlanMode`, opencode's `--plan` flag, Cline's "Plan vs Act"
toggle, Cursor's agent-mode planning step, and Aider's
`/architect` mode are all variants of the same idea: separate
*deciding what to do* from *doing it*.

ork has every prerequisite to land plan mode:

- ADR [`0011`](0011-native-llm-tool-calling.md) already mediates the
  tool catalog; filtering it per phase is a one-line change at the
  catalog seam.
- ADR [`0033`](0033-coding-agent-personas.md) already carries a
  `PersonaPhase` discriminator on every step.
- ADR [`0029`](0029-workspace-file-editor.md), ADR
  [`0028`](0028-shell-executor-and-test-runners.md), and ADR
  [`0030`](0030-git-operations.md) already partition the native tool
  set into clearly-mutating vs. clearly-read-only categories.
- ADR [`0032`](0032-agent-memory-and-context-compaction.md)
  separates `recall` (read) from `remember` (write).
- ADR [`0037`](0037-lsp-diagnostics.md) and ADR [`0040`] (planned —
  repo map) provide grounded read-only context that makes plans
  worth reviewing.

What single-process harnesses cannot do — and what ork can — is
*compose plan reviewers across processes, profiles, and tenants*.
Because every ork agent is A2A-first (ADR
[`0002`](0002-agent-port.md), ADR [`0003`](0003-a2a-protocol-model.md))
and addressable via peer delegation (ADR
[`0006`](0006-peer-delegation.md), ADR
[`0007`](0007-remote-a2a-agent-client.md)), a plan emitted from
plan mode is just an A2A `DataPart` and is dispatchable to N
independent `plan_verifier` peers (ADR
[`0033`](0033-coding-agent-personas.md)) before Execute is allowed
to start. This generalises [`AGENTS.md`](../../AGENTS.md) §7's
adversarial ADR-review pass — currently a *design-time* affordance —
into a *runtime* gate that can run on every coding task. It is the
platform-distinct feature: a single-user CLI cannot run a verifier
on a different vendor's hosted frontier model belonging to a
different tenant in parallel with two local quantised models, and
have a typed verdict aggregated and audited at the end. ork can.

The closest existing surfaces are deliberately *not* this:

- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)
  validates a step's *typed output* after the producer ran. This
  ADR validates a step's *plan* before the producer touches anything.
  Same shape (verifier agent, structured verdict, repair loop);
  different gate location.
- ADR [`0027`](0027-human-in-the-loop.md) inserts a *human* between
  steps. This ADR inserts *peer agents* — possibly chained with
  HITL, but distinct from it.
- ADR [`0039`] (planned — hooks) is a per-tool-call guardrail;
  hooks can *trigger* this gate but they don't perform the review.
- Post-execution code review of a finished diff (the `code-reviewer`
  subagent in [`AGENTS.md`](../../AGENTS.md) §3) reviews *changes*;
  this gate reviews *intent*.

This ADR specifies both pieces in a single document because they
co-evolve: the plan schema is what plan mode emits and what
verifiers consume, so committing one without the other locks in a
half-spec.

## Decision

ork **introduces** (a) a binary `AgentPhase` discriminator on the
`LocalAgent` loop with phase-aware tool catalog masking, (b) a
`propose_plan` native tool that terminates Plan and emits a typed
A2A `DataPart` carrying the plan, (c) an A2A
**plan cross-verification gate** that dispatches that plan to one or
more `plan_verifier` peers (ADR
[`0033`](0033-coding-agent-personas.md)), (d) a typed verdict shape
constrained-decoded per ADR
[`0035`](0035-constrained-decoding.md), (e) configurable
aggregation policies, and (f) explicit timeout / invalid-output /
HITL fall-throughs. The gate is opt-in per profile, persona, hook,
or workflow; team flows (ADR [`0045`]) recommend on, solo flows
default off.

### `AgentPhase` and tool-catalog masking

```rust
// crates/ork-agents/src/phase.rs

/// Runtime phase of the agent loop. Distinct from
/// `PersonaPhase` (ADR 0033), which is a *design-time* role
/// label used for routing; `AgentPhase` is the *loop state* used
/// to gate the tool catalog at each iteration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    Plan,
    Execute,
}

impl AgentPhase {
    /// Map a persona's design-time phase to a runtime phase.
    /// Plan / Verify / Explore are read-only; Edit / Test / Review
    /// / Commit are mutating; AnyPhase defers to the model
    /// profile's `default_phase_for_coding_tasks`.
    pub fn from_persona_phase(
        p: PersonaPhase,
        profile_default: AgentPhase,
    ) -> AgentPhase {
        match p {
            PersonaPhase::Plan
            | PersonaPhase::Verify
            | PersonaPhase::Explore => AgentPhase::Plan,
            PersonaPhase::Edit
            | PersonaPhase::Test
            | PersonaPhase::Review
            | PersonaPhase::Commit => AgentPhase::Execute,
            PersonaPhase::AnyPhase => profile_default,
        }
    }
}
```

`AgentConfig` (already at
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs))
gains an additive `phase: AgentPhase` field defaulting to
`AgentPhase::Execute` — preserving today's behaviour for every call
site that does not opt in.

Each native tool registered through
[`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
declares a `ToolReadOnlyClass` at registration:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReadOnlyClass {
    /// Pure observation: never mutates the workspace, the
    /// repository, the memory store, or the outside world.
    ReadOnly,
    /// Anything else.
    Mutating,
}

pub struct ToolDescriptor {
    // ...existing fields...
    pub read_only_class: ToolReadOnlyClass,
}
```

`LocalAgent::tool_descriptors()` filters by class when
`AgentConfig::phase == AgentPhase::Plan`: `ReadOnly` tools plus the
single phase-transition tool `propose_plan` are kept; everything
else is dropped from the catalog the LLM ever sees. Filtering at
the catalog seam (not at the executor) means a misbehaving model
*cannot reference* a masked tool name — there is no failure mode
where the model emits `apply_patch` and the executor refuses; the
model never knew `apply_patch` existed for that turn.

Concretely, in Plan phase the catalog is reduced to:

| Tool | Source ADR | Why it's read-only |
| ---- | ---------- | ------------------ |
| `read_file` | 0029 | Pure read |
| `list_tree` | 0029 / `code_tools.rs` | Pure read |
| `code_search` | `code_tools.rs` | Pure read |
| `git_status`, `git_diff` | 0030 | Read-only git ops |
| `get_diagnostics` | 0037 | Wraps `cargo check`-class — observation only |
| `get_repo_map` | 0040 (planned) | Repo structure snapshot |
| `recall` | 0032 | Memory read |
| `propose_plan` | this ADR | Phase-transition emitter |

Everything else — `write_file`, `apply_patch`, `update_file`,
`run_command`, `run_tests`, `git_commit`, `git_apply`, `git_branch`,
`remember`, peer-delegation tools whose target catalog includes
mutators — is dropped. The classification is a property of the
*tool*, not of a per-persona allow-list, so a future ADR adding a
new tool gets phase-mode treatment automatically.

### Phase transition: `propose_plan`

Plan mode terminates on a single explicit tool call:

```rust
// crates/ork-integrations/src/code_tools.rs (registration)

ToolDescriptor {
    name: "propose_plan",
    description: "Submit the proposed plan and end Plan phase.",
    parameters: PROPOSE_PLAN_SCHEMA,        // see Plan schema below
    read_only_class: ToolReadOnlyClass::ReadOnly,
}
```

When the model calls `propose_plan(plan)`:

1. The arguments are deserialised against `Plan` (Rust type below)
   and validated. Failure → ADR
   [`0035`](0035-constrained-decoding.md)'s repair loop, capped at
   one retry; second failure terminates the step with
   `OrkError::Validation("plan_payload_invalid")`.
2. The validated `Plan` is emitted as a typed A2A `DataPart` (ADR
   [`0003`](0003-a2a-protocol-model.md)) on the current task's SSE
   stream so the web UI (ADR [`0017`](0017-webui-chat-client.md))
   and any subscribed peer can observe it.
3. If cross-verification is enabled (resolution chain below), the
   plan is dispatched to one or more `plan_verifier` peers and the
   gate awaits verdicts.
4. On `approve` (or aggregated equivalent), the loop flips to
   `AgentPhase::Execute`, re-resolves the tool catalog (no longer
   masked), injects the approved plan plus all verifier verdicts
   into the system-prompt context as ADR
   [`0033`](0033-coding-agent-personas.md)'s persona expects, and
   continues. On `request_changes` or `reject`, see *Failure modes*
   below.

A persona installed with `EditFormat::ReadOnly` (ADR
[`0033`](0033-coding-agent-personas.md)) — the `plan_verifier`
itself, and any other read-only persona — never transitions out of
Plan: `propose_plan` is replaced with `submit_plan_verdict` (ADR
[`0033`](0033-coding-agent-personas.md)) and the loop terminates on
verdict submission.

### Default-on policy

The decision "should this run start in Plan?" is owned by the model
profile, with three override layers:

1. **Profile default** — ADR
   [`0034`](0034-per-model-capability-profiles.md)'s `ModelProfile`
   gains an additive field
   `default_phase_for_coding_tasks: AgentPhase`. Built-in defaults:

   | Profile id | default_phase_for_coding_tasks |
   | --- | --- |
   | `ork.profiles.frontier_planner` | `plan` |
   | `ork.profiles.frontier_executor` | `execute` |
   | `ork.profiles.frontier_verifier` | `plan` |
   | `ork.profiles.local_coder_small` | `plan` |
   | `ork.profiles.local_coder_medium` | `plan` |
   | `ork.profiles.local_general` | `plan` |

   Weak / local-coder profiles default to Plan (the failure-mode
   guard), frontier executor defaults to Execute (the established
   regime that already works). The frontier planner sits in Plan
   because that's its job; the verifier never leaves it.

2. **Persona override** — ADR
   [`0033`](0033-coding-agent-personas.md)'s `CodingPersona` may
   pin `AgentPhase` per persona regardless of profile, via
   `default_phase: PersonaPhase` mapped through
   `AgentPhase::from_persona_phase`. Architect personas resolve to
   Plan, executor personas to Execute, the `solo_coder` persona to
   `AnyPhase` (i.e. profile decides).

3. **Workflow / hook override** — A `WorkflowStep` may set
   `phase: plan | execute` directly, and ADR [`0039`] (planned)'s
   `pre_step_hook` may force Plan. Both win over profile and
   persona.

### Plan A2A `DataPart` schema

Plans flow on the wire as a typed `DataPart`, identifiable by the
schema URL on the data envelope (ADR
[`0003`](0003-a2a-protocol-model.md)):

```json
{
  "kind": "data",
  "data": {
    "schema": "https://ork.dev/schemas/plan/v1",
    "plan_id": "01HXYZ...ULID",
    "originator_agent_id": "ork.agent.architect.local",
    "goal": "Make `crates/ork-core` compile after the refactor in #1234.",
    "rationale": "Three call sites still pass &str where the new API expects PathBuf.",
    "risk_notes": "Touching engine.rs may invalidate cached compile fingerprints; verify with cargo check.",
    "affected_paths": [
      "crates/ork-core/src/workflow/engine.rs",
      "crates/ork-core/src/models/workflow.rs",
      "crates/ork-core/tests/engine_smoke.rs"
    ],
    "steps": [
      {
        "id": "s1",
        "intent": "Rename `WorkflowExecutorComponent::resolve_input` to `resolve_input_path` and adjust three callers.",
        "tools": ["apply_patch", "run_tests"],
        "affected_paths": ["crates/ork-core/src/workflow/engine.rs"],
        "acceptance": "cargo test -p ork-core engine:: passes."
      }
    ],
    "context_refs": {
      "repo_head_commit": "abcdef0",
      "repo_map_artifact_id": "01HXYZ...artifact",
      "memory_excerpt_ids": ["ork.memory.architect/main:42"],
      "diagnostics_snapshot_artifact_id": "01HXYZ...diag"
    }
  }
}
```

The Rust types live with the agent loop:

```rust
// crates/ork-agents/src/plan.rs

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub plan_id: PlanId,                 // ULID
    pub originator_agent_id: AgentId,
    pub goal: String,
    pub rationale: String,
    pub risk_notes: String,
    pub affected_paths: Vec<String>,     // top-level union; redundant with steps[]
    pub steps: Vec<PlanStep>,
    pub context_refs: PlanContextRefs,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub intent: String,
    pub tools: Vec<String>,              // names from the Execute-phase catalog
    pub affected_paths: Vec<String>,
    pub acceptance: String,              // human-readable success criterion
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PlanContextRefs {
    pub repo_head_commit: Option<String>,
    pub repo_map_artifact_id: Option<ArtifactId>,
    pub memory_excerpt_ids: Vec<String>,
    pub diagnostics_snapshot_artifact_id: Option<ArtifactId>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanId(pub String);
```

This shape is **canonical** for both the Plan-mode emitter and the
verifier consumer; ADR [`0033`](0033-coding-agent-personas.md)'s
sketch of `PlanVerificationRequest::plan` re-exports this type.
The schema URL is `https://ork.dev/schemas/plan/v1`; future shape
changes bump to `/v2` and ship a deprecation window per ADR
[`0003`](0003-a2a-protocol-model.md).

The wire shape is intentionally a *summary* of intent, not a full
diff: `PlanStep::tools` names tools but does not pre-bind their
arguments, and `affected_paths` is advisory. The executor is
responsible for actually producing the change; the plan is the
hypothesis.

### Cross-verification gate

When Plan terminates, the gate runs:

```rust
// crates/ork-core/src/workflow/plan_gate.rs

#[async_trait]
pub trait PlanCrossVerifier: Send + Sync {
    /// Dispatch `plan` to N peers per `policy.verifiers`, await
    /// verdicts subject to `policy.timeout_per_verifier`, aggregate
    /// per `policy.aggregation`, persist findings as artifacts (ADR
    /// 0016), and return a single decision.
    async fn verify(
        &self,
        ctx: &PlanGateContext,
        plan: &Plan,
        policy: &PlanVerificationPolicy,
    ) -> Result<PlanGateDecision, OrkError>;
}

#[derive(Clone, Debug)]
pub struct PlanVerificationPolicy {
    pub verifiers: Vec<PlanVerifierTarget>,
    pub aggregation: AggregationPolicy,
    pub timeout_per_verifier: Duration,    // default 60s
    pub on_timeout: TimeoutPolicy,         // FailClosed | FailOpen
    pub on_invalid_verdict: InvalidVerdictPolicy, // RetryOnce | TreatAsRequestChanges
    pub require_human_after: Option<HumanEscalation>, // ADR 0027 hook
}

#[derive(Clone, Debug)]
pub struct PlanVerifierTarget {
    pub agent_ref: AgentRef,            // ADR 0007: id or inline card
    pub weight: f32,                    // for AggregationPolicy::Weighted; default 1.0
    pub vetoes: bool,                   // if true, this verifier's `reject` is final regardless of others
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregationPolicy {
    Unanimous,        // all verifiers must approve
    Majority,         // > N/2 approve; ties → request_changes
    FirstDeny,        // any reject halts dispatch immediately
    Weighted,         // sum of approve-weights > sum of non-approve-weights; vetoers override
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutPolicy { FailClosed, FailOpen }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidVerdictPolicy { RetryOnce, TreatAsRequestChanges }

#[derive(Clone, Debug)]
pub enum PlanGateDecision {
    Approved {
        verdicts: Vec<PlanVerdict>,
        approved_plan_artifact_id: ArtifactId,
    },
    RequestChanges {
        verdicts: Vec<PlanVerdict>,
        aggregated_findings: Vec<PlanFinding>,
    },
    Rejected {
        verdicts: Vec<PlanVerdict>,
        reason: String,
    },
}
```

Verifier peers are dispatched via the existing peer-delegation path
(ADR [`0006`](0006-peer-delegation.md), ADR
[`0007`](0007-remote-a2a-agent-client.md)), not over a new wire.
The dispatch is a single A2A task per verifier whose initial
message carries the plan `DataPart`; verifiers run in their own
Plan phase (read-only catalog) with access to the same
`get_repo_map` / `get_diagnostics` / `recall` tools so their review
is grounded.

Verifier peers are selected, in order:

1. **Workflow** — `WorkflowStep::plan_verification.verifiers[]`
   names AgentRefs explicitly.
2. **Hook** — ADR [`0039`]'s `require_a2a_plan_verification` hook
   may inject targets at run time.
3. **Profile hint** — when neither of the above is set,
   `ModelProfile::recommended_plan_verifier_model_id` (ADR
   [`0034`](0034-per-model-capability-profiles.md)) plus ADR
   [`0042`]'s capability discovery selects one peer whose model is
   "materially different" from the planner's.
4. **None** — if no targets resolve and the persona/profile does
   not require verification, the gate is **bypassed** and Plan
   transitions straight to Execute (still useful: plan mode alone
   already constrains tool use).

### Verdict schema and constrained decoding

Verifiers submit a single structured verdict via the
`submit_plan_verdict` tool already introduced by ADR
[`0033`](0033-coding-agent-personas.md). This ADR pins the wire
shape and requires the verifier's provider to honour it through
ADR [`0035`](0035-constrained-decoding.md) constrained decoding
when the profile supports it:

```json
{
  "kind": "data",
  "data": {
    "schema": "https://ork.dev/schemas/plan-verdict/v1",
    "plan_id": "01HXYZ...ULID",
    "verifier_agent_id": "ork.agent.plan_verifier.haiku",
    "verifier_model_id": "claude-haiku-4-5",
    "decision": "approve",
    "confidence": 0.82,
    "findings": [
      {
        "severity": "minor",
        "step_id": "s1",
        "summary": "Renaming a public method without a deprecation shim breaks downstream crates.",
        "evidence": "git grep 'resolve_input(' shows 6 hits across ork-api and ork-cli."
      }
    ],
    "suggestions": [
      {
        "step_id": "s1",
        "rewrite": "Add a #[deprecated] alias `resolve_input` that forwards to `resolve_input_path` for one minor version."
      }
    ]
  }
}
```

```rust
// crates/ork-agents/src/plan.rs (continued)

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanVerdict {
    pub plan_id: PlanId,
    pub verifier_agent_id: AgentId,
    pub verifier_model_id: String,
    pub decision: PlanDecision,
    pub confidence: f32,                 // 0.0..=1.0
    pub findings: Vec<PlanFinding>,
    pub suggestions: Vec<PlanSuggestion>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDecision { Approve, RequestChanges, Reject }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanFinding {
    pub severity: PlanFindingSeverity,
    pub step_id: Option<String>,
    pub summary: String,
    pub evidence: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanFindingSeverity { Blocker, Major, Minor, Nit }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanSuggestion {
    pub step_id: Option<String>,
    pub rewrite: String,
}
```

Note the shape is a *deliberate evolution* of the
`PlanVerdict` sketched in ADR
[`0033`](0033-coding-agent-personas.md):
`verdict` → `decision` (alignment with ADR
[`0035`](0035-constrained-decoding.md)'s standard discriminant
naming), `score` → `confidence` (the field carries the verifier's
calibrated certainty, not a quality score), and the mandatory
`plan_id` ties a verdict to its plan for audit. The implementing
session updates ADR [`0033`](0033-coding-agent-personas.md)'s
`plan_verifier.rs` types to this shape; ADR
[`0033`](0033-coding-agent-personas.md) is still Proposed so the
edit is in-scope.

The verifier's provider attaches the JSON Schema of `PlanVerdict`
as ADR [`0035`](0035-constrained-decoding.md)'s
`Constraint::JsonSchema` when the profile's
`supports_grammar_constraint == true`. When unsupported, the
verifier still calls `submit_plan_verdict` but the loop validates
the arguments after the fact and applies
`InvalidVerdictPolicy::RetryOnce` if validation fails.

### Verifier diversity (the no-echo property)

A verifier running on the same model as the planner is mostly an
expensive echo — Google Research, *Towards a Science of Scaling
Agent Systems* (April 2025), reports the same-model verifier baseline
hovers around `+0%` on aggregate task success. The gate guards
against this in two layers:

- **Soft layer (default).** ADR
  [`0034`](0034-per-model-capability-profiles.md)'s
  `recommended_plan_verifier_model_id` is consulted when the
  workflow does not name verifiers explicitly. ADR [`0042`]'s
  discovery index ranks remaining peers by `provider_id` distance
  and same-vendor penalty.
- **Hard layer (opt-in).**
  `PlanVerificationPolicy::require_distinct_verifier_model = true`
  causes the gate to refuse to dispatch to a peer whose
  `verifier_model_id` matches the planner's resolved
  `model_id`; the run fails fast rather than running an echo.

The hard layer is off by default — operators with a single hosted
model in their config still want plan verification on cheaper /
faster instances of *the same* model, and forbidding that would
reduce the surface to "no verification at all."

### Failure modes

The gate's behaviour for each failure is explicit:

- **Verifier unreachable / times out.** Driven by
  `TimeoutPolicy`. `FailClosed` (default for team flows) treats the
  silent verifier as `request_changes` and either re-prompts the
  planner or — if `require_human_after` is set — escalates to ADR
  [`0027`](0027-human-in-the-loop.md). `FailOpen` (default for solo
  flows where verification is opt-in convenience) records the
  miss in observability (ADR [`0022`](0022-observability.md)) and
  proceeds to Execute.
- **Verifier returns invalid verdict** (post-constrained-decode
  validation fails). One ADR
  [`0035`](0035-constrained-decoding.md) repair-loop retry; on
  second failure, treat as `request_changes` per
  `InvalidVerdictPolicy`.
- **Verifier itself violates a hook** (ADR [`0039`]) during its
  read-only run — e.g. attempts a tool not in its catalog. The
  verifier's verdict is dropped; if all verifiers are dropped, the
  gate falls through to `TimeoutPolicy`.
- **Verifier verdict references an unknown step_id.** The finding
  is preserved (operators want to see it) but is not blocking; the
  aggregator treats it as advisory.
- **Aggregation produces `request_changes`.** The gate re-dispatches
  the planner with verdicts injected, capped at
  `PlanVerificationPolicy::max_replan_rounds` (default 2). Beyond
  the cap, the run fails with `OrkError::Validation("plan_rejected")`
  unless `require_human_after` escalates first.
- **Aggregation produces `reject` (or any vetoer rejects).** The
  step terminates as `Failed` with the verdicts attached as
  artifacts. No replan; the workflow may route to a recovery step
  per ADR [`0018`](0018-dag-executor-enhancements.md).

### HITL composition (ADR 0027)

`PlanVerificationPolicy::require_human_after: Option<HumanEscalation>`
plugs ADR [`0027`](0027-human-in-the-loop.md) into the gate. Three
modes:

- `Always` — every plan, regardless of verdict, requires human
  approval before Execute.
- `OnNonApprove` — automatic on `request_changes` / `reject`.
- `OnAggregationFailure` — automatic when verifiers disagree but
  no aggregation policy produces a clear winner (e.g. 1-1 split
  under `Majority`).

The escalation surfaces in the web UI (ADR
[`0017`](0017-webui-chat-client.md)) as an inline prompt next to
the plan; the human's decision becomes a synthetic verdict in the
event log (`verifier_agent_id = "ork.human.<user_id>"`).

### Trust, audit, and observability

- **Tenant scope.** A2A verifier dispatch honours ADR
  [`0020`](0020-tenant-security-and-trust.md): the verifier task
  inherits the originator's tenant scope unless the verifier's card
  declares `cross_tenant: true` and the originator's tenant grants
  the trust. Cross-tenant verifier deployments (an "ork verifier as
  a service") receive a redacted plan: `affected_paths` and
  `context_refs.repo_*` are stripped or replaced with content
  hashes per the tenant's redaction policy.
- **Artifacts.** The plan, every verdict, and the aggregated
  decision are persisted as ADR
  [`0016`](0016-artifact-storage.md) artifacts and surface in the
  web UI. The plan artifact's content-hash is the canonical
  identifier — re-running the same plan against the same head
  commit reuses verdicts when caching is on (open question; see
  below).
- **Events.** New event kinds on the per-task event log (ADR
  [`0022`](0022-observability.md)): `plan.proposed`,
  `plan.verifier.dispatched`, `plan.verifier.verdict`,
  `plan.gate.decision`. Each carries `plan_id` and the verifier's
  `agent_id` and `model_id` so dashboards can slice
  cross-verification cost and latency by phase, model, and
  workflow.

### Solo and team flows

Solo flows (ADR [`0033`](0033-coding-agent-personas.md)'s
`solo_coder`) default to plan mode on for weak profiles, plan mode
off for frontier profiles, **and cross-verification off**. A solo
operator opts in via:

- `ModelProfileRef::with_plan_verifiers(&[...])` (per ADR
  [`0033`](0033-coding-agent-personas.md) §Solo opt-in), or
- `pre_edit_hook = "verify_plan"` on the run config (per ADR
  [`0033`](0033-coding-agent-personas.md)), or
- `verify: true` on the
  [`coding-agent-solo.yaml`](../../workflow-templates/) workflow
  template.

Team flows (ADR [`0045`]) default to **plan mode on, cross-verification
on, aggregation = Majority, distinct-model required, on_timeout =
FailClosed**. The architect persona produces the plan; verifiers
review it; only then does the executor receive it (architect →
verifier → executor handoff per ADR
[`0033`](0033-coding-agent-personas.md) §Architect → executor
handoff).

In team flows the executor starts directly in
`AgentPhase::Execute`. There is no second plan-mode pass: the plan
is already in context, already approved, and re-planning would
duplicate the architect's work and waste verifier budget.

### Web UI affordance (ADR 0017)

The web UI (ADR [`0017`](0017-webui-chat-client.md)) renders the
plan as a structured panel adjacent to the chat stream, with each
verifier's verdict, findings, and confidence inline. Findings are
colour-coded by severity; suggestions are clickable and copy the
`rewrite` text into the chat input. When `require_human_after`
fires, the panel grows an Approve / Request changes / Reject
control row backed by ADR
[`0027`](0027-human-in-the-loop.md)'s approval API.

The panel does **not** auto-collapse on approval — operators want
to see the verdict trail when reading a finished run.

### Out of scope

- **Plan synthesis quality.** What makes a *good* plan is a prompt
  problem owned by ADR
  [`0033`](0033-coding-agent-personas.md)'s persona prompts. This
  ADR pins the *shape* and the *gate*; the prompts that produce
  good plans evolve out of band.
- **Verifier selection beyond profile hints + ADR [`0042`]
  discovery.** A reputation / scoring system for verifiers is a
  follow-up.
- **Caching of verdicts.** Re-using a verdict for an identical
  plan against an identical head commit is desirable but raises
  staleness questions (the diagnostics snapshot ages, the repo map
  ages); deferred. See *Open questions*.
- **Streaming partial verdicts.** Verifiers terminate on
  `submit_plan_verdict`; intermediate findings are not streamed.
  This matches ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  terminal-only verdict policy. May change if the UI demands it.
- **Plan-mode for non-coding tasks.** The phase model is generic
  but the read-only tool set is coding-flavoured. Non-coding tasks
  may opt in trivially by classifying their tools, but this ADR
  does not enumerate that work.
- **Team composition.** ADR [`0045`].

## Acceptance criteria

- [ ] Enum `AgentPhase { Plan, Execute }` defined at
      `crates/ork-agents/src/phase.rs` with serde derives and the
      `from_persona_phase` mapper shown in `Decision`.
- [ ] `AgentConfig::phase: AgentPhase` field added at
      [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
      with default `AgentPhase::Execute`; existing call sites
      compile against `AgentConfig::default()` unchanged.
- [ ] `ToolReadOnlyClass { ReadOnly, Mutating }` added to
      `ToolDescriptor` at
      [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs);
      every existing native tool registration gets a class.
- [ ] `LocalAgent::tool_descriptors()` filters out `Mutating` tools
      when `AgentConfig::phase == AgentPhase::Plan`, keeps
      `ReadOnly` tools plus `propose_plan` — verified by
      `crates/ork-agents/tests/plan_mode_catalog.rs::masks_mutators`.
- [ ] `propose_plan` registered as a native tool with a JSON Schema
      that mirrors `Plan`; calling it in
      `AgentPhase::Execute` returns `OrkError::Validation("propose_plan_in_execute")`
      — verified by
      `crates/ork-agents/tests/propose_plan.rs::rejected_outside_plan_phase`.
- [ ] `Plan`, `PlanStep`, `PlanContextRefs`, `PlanId` types defined
      at `crates/ork-agents/src/plan.rs` with serde derives matching
      the JSON shape in `Decision`.
- [ ] `PlanVerdict`, `PlanDecision`, `PlanFinding`, `PlanSuggestion`,
      `PlanFindingSeverity` defined in the same module; ADR
      [`0033`](0033-coding-agent-personas.md)'s
      `personas/plan_verifier.rs` re-exports these instead of
      defining its own.
- [ ] Round-trip test
      `crates/ork-agents/tests/plan_schema.rs::plan_roundtrip` and
      `verdict_roundtrip` pass against the example payloads in
      `Decision`.
- [ ] `submit_plan_verdict` JSON Schema in
      [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
      updated to the new verdict shape (`decision`, `confidence`).
- [ ] `PlanCrossVerifier` trait + `PlanGateContext`,
      `PlanVerificationPolicy`, `PlanVerifierTarget`,
      `AggregationPolicy`, `TimeoutPolicy`, `InvalidVerdictPolicy`,
      `PlanGateDecision`, `HumanEscalation` defined at
      `crates/ork-core/src/workflow/plan_gate.rs` with the
      signatures shown.
- [ ] `A2aPlanCrossVerifier` impl in `crates/ork-core` (or
      `crates/ork-agents`) that dispatches via the existing
      delegation publisher (ADR
      [`0006`](0006-peer-delegation.md)) and remote-agent client
      (ADR [`0007`](0007-remote-a2a-agent-client.md)); no new wire
      shape — verified by
      `crates/ork-core/tests/plan_gate_smoke.rs::dispatches_to_two_peers`.
- [ ] Aggregation tests at `crates/ork-core/tests/plan_aggregation.rs`
      cover all four `AggregationPolicy` variants (`unanimous`,
      `majority`, `first_deny`, `weighted`) plus the `vetoes: true`
      override on `Weighted`.
- [ ] Failure-mode tests at `crates/ork-core/tests/plan_gate_failure.rs`:
      `verifier_timeout_fail_closed_request_changes`,
      `verifier_timeout_fail_open_proceeds`,
      `invalid_verdict_retry_then_request_changes`,
      `vetoer_reject_overrides_majority_approve`,
      `unknown_step_id_finding_advisory_only`.
- [ ] `LocalAgent` Plan-phase loop calls the gate on
      `propose_plan`; on `Approved`, flips to
      `AgentPhase::Execute`, re-resolves the catalog, and continues
      with the approved plan injected into the system prompt —
      verified by
      `crates/ork-agents/tests/plan_to_execute.rs::approved_plan_continues`.
- [ ] `crates/ork-agents/tests/plan_to_execute.rs::request_changes_replans`
      asserts that on aggregated `request_changes` the loop
      re-prompts the planner with the verdicts injected.
- [ ] `crates/ork-agents/tests/plan_to_execute.rs::reject_terminates`
      asserts that on `reject` the step ends as `Failed` with the
      verdicts attached as artifacts (ADR
      [`0016`](0016-artifact-storage.md)).
- [ ] `ModelProfile::default_phase_for_coding_tasks: AgentPhase`
      added at `crates/ork-llm/src/profile.rs`; built-in defaults
      match the table in `Decision` — verified by
      `crates/ork-llm/tests/profile_default_phase.rs::builtin_defaults_match_table`.
- [ ] `WorkflowStep::phase: Option<AgentPhase>` and
      `WorkflowStep::plan_verification: Option<PlanVerificationPolicy>`
      added to the workflow YAML schema; loaded by the workflow
      compiler — verified by
      `cargo test -p ork-core compiler::loads_plan_verification_policy`.
- [ ] Plan, verdicts, and gate decision persisted as artifacts
      (ADR [`0016`](0016-artifact-storage.md)); event-log rows for
      `plan.proposed`, `plan.verifier.dispatched`,
      `plan.verifier.verdict`, `plan.gate.decision` emitted with
      `plan_id`, `verifier_agent_id`, `verifier_model_id` (ADR
      [`0022`](0022-observability.md)) — verified by
      `crates/ork-core/tests/plan_gate_observability.rs::events_emitted`.
- [ ] Tenant scope test
      `crates/ork-core/tests/plan_gate_tenant.rs::cross_tenant_plan_redacted`
      asserts that a plan dispatched to a `cross_tenant: true`
      verifier has `affected_paths` and `context_refs` redacted
      per ADR [`0020`](0020-tenant-security-and-trust.md).
- [ ] HITL test
      `crates/ork-core/tests/plan_gate_hitl.rs::on_non_approve_escalates_to_human`
      drives the flow through ADR
      [`0027`](0027-human-in-the-loop.md)'s approval API and
      asserts a synthetic human verdict appears in the event log.
- [ ] `cross-verification distinct-model` test
      `crates/ork-core/tests/plan_gate_diversity.rs::same_model_planner_and_verifier_rejected`
      asserts that with
      `require_distinct_verifier_model = true` the gate refuses to
      dispatch to a same-model peer.
- [ ] Web UI panel for plan + verdicts shipped at
      [`client/webui/frontend/`](../../) with severity colour-coding
      and a Suggest-rewrite click-to-copy affordance — manual
      verification documented in the demo script.
- [ ] [`workflow-templates/coding-agent-solo.yaml`](../../workflow-templates/)
      gains a `verify` step wired through this ADR's gate (replaces
      the placeholder ADR
      [`0033`](0033-coding-agent-personas.md) shipped) — verified
      by `cargo test -p ork-core compiler::loads_solo_template_with_verify`.
- [ ] [`demo/scripts/`](../../demo/scripts/) adds a stage that
      drives a small failing-test fix through plan mode +
      one-verifier cross-verification end-to-end against a toy
      crate; expected output captured under
      [`demo/expected/`](../../demo/expected/).
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0038`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- Weak-model thrash on coding tasks drops sharply: a 7B model
  that would otherwise call `apply_patch` after misreading a
  diagnostic literally cannot — the tool is not in its catalog.
- The adversarial-review pattern from
  [`AGENTS.md`](../../AGENTS.md) §7 generalises from design-time
  ADR review to runtime task gating, with zero new wire shapes —
  every existing A2A peer delegation path carries plans and
  verdicts unchanged.
- Plan + verdicts on the event log + as artifacts make every run
  auditable: "why did the agent do X" has a structured answer
  that survives the LLM context being garbage-collected.
- Verifier diversity (different model, different tenant, different
  vendor) is a feature only an A2A platform can offer. Single-user
  CLIs can only echo their own model.
- HITL composes cleanly: the gate is the choke point, so
  `require_human_after` is one optional field rather than a
  separate gating layer.
- Forward compatibility with ADR [`0042`] is automatic: the
  capability discovery service already wants to index `plan_verifier`
  personas, and this ADR's hard requirements (read-only catalog,
  `submit_plan_verdict`, plan/verdict schemas) give the index
  something concrete to filter on.

### Negative / costs

- Every cross-verified run pays N+1 LLM round-trips before any
  edit happens (planner + N verifiers). For a solo run on a
  frontier model this is *worse* than letting the model edit
  directly. Mitigation: cross-verification is opt-in for solo
  flows; team flows already amortise the extra hops over
  multi-step work.
- Plan-mode tool masking has to stay synchronised with every new
  tool added to the catalog. We close this with the
  `ToolReadOnlyClass` requirement at registration time, but
  reviewers must catch missing classifications in code review.
  Mitigation: a unit test asserts every registered tool has a
  classification (no default).
- Two new wire schemas (`plan/v1`, `plan-verdict/v1`) are now
  load-bearing. Breaking changes require a `/v2` and a deprecation
  window per ADR [`0003`](0003-a2a-protocol-model.md). The shapes
  are intentionally minimal to limit churn pressure.
- Aggregation policies are a small but real surface. Operators
  will pick one and it will be *almost* right; the wrong choice
  (e.g. `Unanimous` with three verifiers, two of which time out)
  produces frustrating false negatives. Mitigation: the default
  in solo flows is "no verifiers"; the default in team flows is
  `Majority`.
- A verifier persona that violates a hook (ADR [`0039`]) during
  its own read-only run is a new failure mode; we drop the verdict
  and log, but the *budget* (LLM tokens) is already spent. ADR
  [`0022`](0022-observability.md)'s `BudgetMonitor` should track
  it.
- The shape of `PlanVerdict` evolves from ADR
  [`0033`](0033-coding-agent-personas.md)'s sketch (`verdict` →
  `decision`, `score` → `confidence`, mandatory `plan_id`). ADR
  [`0033`](0033-coding-agent-personas.md) is still Proposed, so
  this is in-scope; reviewers must catch the rename in any
  in-flight 0033 implementation.
- Plans grounded with `repo_map_artifact_id` and
  `diagnostics_snapshot_artifact_id` retain references to
  artifacts that may be GC'd before the verdict trail is consulted.
  Artifact retention policy (ADR
  [`0016`](0016-artifact-storage.md)) needs to keep plan-related
  artifacts at least as long as the parent run; flagged as a
  follow-up.

### Neutral / follow-ups

- ADR [`0034`](0034-per-model-capability-profiles.md) gains
  `default_phase_for_coding_tasks`. The field is additive; profiles
  written before this ADR get `AgentPhase::Execute` (today's
  behaviour).
- ADR [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
  persona consumes the canonical types this ADR defines — the
  persona stops carrying its own `PlanVerificationRequest` /
  `PlanVerdict` types in favour of `crates/ork-agents/src/plan.rs`.
- ADR [`0039`] (planned hooks) gains
  `require_a2a_plan_verification` as a recognised hook outcome.
- ADR [`0042`] (planned discovery) gains a filter on
  `coding-persona.role == "plan_verifier"` and on
  `model-profile.model_id` to power the diversity ranker.
- ADR [`0045`] (planned team orchestrator) consumes this gate as
  the architect → executor handoff.
- A future ADR may add verdict caching: `(plan_content_hash,
  verifier_agent_id, repo_head_commit) → verdict`, with TTL bound
  by the freshness of the diagnostics snapshot.
- A future ADR may add streaming partial verdicts (open question
  below) if UI demand surfaces.

## Alternatives considered

- **Plan mode without cross-verification.** Tool masking alone is
  the cheap win and would be most of the value for weak local
  models. Rejected as the *only* shape for this ADR because the
  cross-verification protocol depends on the plan schema being
  pinned, and shipping plan mode with an unspec'd "we'll spec the
  plan shape later" leaves ADR
  [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
  persona dangling. Combining is cheaper than serialising.
- **Cross-verification without plan mode.** Have the planner emit
  a plan as a free-form text part and verifiers consume it.
  Rejected: without phase masking the planner is free to emit a
  plan *and* mutate the workspace in the same turn, which defeats
  the gate.
- **Use ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  in-engine `Verifier` port for plan verification.** Rejected:
  the in-engine port runs *inside* the workflow engine after a
  producing step, on a typed output. Plan verification runs
  *across A2A peers* before the producer's mutating tools fire;
  coupling them to the in-engine port loses the cross-mesh /
  cross-tenant property and forces verifiers to be in-process.
- **A new dedicated `/a2a/plan-verify` JSON-RPC method.** Rejected:
  the existing peer-delegation path (ADR
  [`0006`](0006-peer-delegation.md)) plus the `plan` `DataPart`
  carry everything needed. A dedicated method would fragment the
  A2A surface ork has been disciplined about keeping minimal.
- **Single-verifier always; aggregation is YAGNI.** Rejected:
  team flows want the diversity property (one security verifier,
  one model-diversity verifier), and weighted-with-vetoer is
  cheaper to design once than to retrofit when the second
  verifier shows up.
- **Mask tools at the executor instead of at the catalog.** Easier
  to implement (one check at tool dispatch) but the model still
  *sees* `apply_patch` in its catalog and may emit it with an
  argument blob — the executor refuses, the model retries, tokens
  burn. Catalog-level masking is strictly better; the model
  cannot reference a tool it doesn't know about.
- **Treat plan mode as a separate persona** (a new `planner_only`
  role). Rejected: phase is a *state*, not a role. The same
  `solo_coder` persona transitions from Plan to Execute within a
  single task; modelling that as two personas would force a swap
  of `LocalAgent` instance mid-flight.
- **Make verification synchronous by piggybacking on the planner's
  Plan-phase tool catalog (`get_peer_review` tool).** Rejected:
  it forces verification on every call site that uses Plan mode,
  even when no verifier is wanted; a gate is a cleaner separation.
- **Hard-require distinct-model verifiers by default.** Rejected:
  small operator deployments often have only one model wired up.
  Off by default; opt-in on team flows where the diversity
  property is the point.
- **No HITL composition; human approval is a separate workflow
  step.** Rejected: HITL on the *plan* (before the executor runs)
  is materially different from HITL on the *result* (after the
  executor runs). Approving a plan is cheap; reviewing a botched
  diff is expensive. Putting the human in the gate where the
  agents already are is the right place.
- **Skip the `confidence` field on the verdict.** Rejected: the
  weighted-aggregation policy multiplies weight by confidence in
  a follow-up; designing it out now means a wire-shape change
  later. Cheap to include; cheaper than removing.

## Affected ork modules

- New: [`crates/ork-agents/src/phase.rs`](../../crates/ork-agents/) —
  `AgentPhase`, `from_persona_phase`.
- New: [`crates/ork-agents/src/plan.rs`](../../crates/ork-agents/) —
  `Plan`, `PlanStep`, `PlanContextRefs`, `PlanId`, `PlanVerdict`,
  `PlanDecision`, `PlanFinding`, `PlanFindingSeverity`,
  `PlanSuggestion`.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
  — `AgentConfig::phase`, catalog filter at `tool_descriptors`,
  Plan→Execute transition on `propose_plan`.
- [`crates/ork-agents/src/personas/plan_verifier.rs`](../../crates/ork-agents/) —
  re-export plan / verdict types from `ork-agents/src/plan.rs`;
  drop the persona-local copies (ADR
  [`0033`](0033-coding-agent-personas.md) integration).
- New: [`crates/ork-core/src/workflow/plan_gate.rs`](../../crates/ork-core/) —
  `PlanCrossVerifier` trait, `PlanGateContext`,
  `PlanVerificationPolicy`, `PlanVerifierTarget`, `AggregationPolicy`,
  `TimeoutPolicy`, `InvalidVerdictPolicy`, `PlanGateDecision`,
  `HumanEscalation`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  — wire the gate between Plan termination and Execute dispatch
  for steps whose `plan_verification` policy is set.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
  — `ToolReadOnlyClass` field on `ToolDescriptor`; classification
  of every existing native tool; `propose_plan` registration; update
  `submit_plan_verdict` schema.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — `propose_plan` implementation (validates, emits `DataPart`,
  hands to the gate).
- [`crates/ork-llm/src/profile.rs`](../../crates/ork-llm/) —
  `ModelProfile::default_phase_for_coding_tasks` field +
  built-in default values per the table in `Decision`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — `WorkflowStep::phase`, `WorkflowStep::plan_verification`.
- [`crates/ork-core/src/workflow/compiler/`](../../crates/ork-core/src/workflow/)
  — YAML parsing for the new fields.
- [`workflow-templates/coding-agent-solo.yaml`](../../workflow-templates/)
  — `verify` step wired through this ADR's gate.
- [`client/webui/frontend/`](../../) — plan-and-verdict panel +
  HITL approve/request/reject controls.
- [`demo/scripts/`](../../demo/scripts/) and
  [`demo/expected/`](../../demo/expected/) — new stage covering
  plan mode + one-verifier cross-verification.
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
| Claude Code | `EnterPlanMode` / `ExitPlanMode` tools, plan-mode prompt scaffolding | `AgentPhase` + `propose_plan` + Plan-mode catalog filter |
| opencode | `--plan` flag, read-only tool subset | `AgentPhase::Plan`, `ToolReadOnlyClass::ReadOnly` filter |
| Cline | "Plan vs Act" mode toggle in the chat UI | `AgentPhase` + ADR [`0017`](0017-webui-chat-client.md) panel |
| Cursor | Agent mode plan step before edits | Plan-mode default for weak profiles (`ModelProfile::default_phase_for_coding_tasks`) |
| Aider | `/architect` mode + separate editor model | Architect persona (ADR [`0033`](0033-coding-agent-personas.md)) + executor persona under Plan/Execute split |
| GitHub Copilot Workspace | Architect/executor split with plan approval gate | `propose_plan` → cross-verification gate → executor with approved plan |
| Claude Code subagent dispatch | `Agent` tool delegation to specialised subagents | `PlanVerifierTarget` over peer delegation (ADR [`0006`](0006-peer-delegation.md), ADR [`0007`](0007-remote-a2a-agent-client.md)) |
| Multi-agent debate / verifier ensembles (Du et al., 2023; Google Research, *Towards a Science of Scaling Agent Systems*, 2025) | "ask N independent judges, aggregate verdicts" | `AggregationPolicy` + verifier diversity (different model / vendor / tenant) |
| ork's own [`AGENTS.md`](../../AGENTS.md) §7 adversarial ADR review | design-time skeptic pass on an ADR | runtime skeptic pass on a plan |

## Open questions

- **Verdict caching.** Identical plan + identical head commit
  could reuse a cached verdict, with TTL bound by diagnostics
  staleness. Worth it on repeat runs; not v1.
- **Streaming partial findings.** Verifiers terminate on
  `submit_plan_verdict`. Streaming intermediate findings via SSE
  would let the UI render verdicts as they form, but adds a new
  event shape and complicates aggregation. Defer until the UI
  demands it.
- **Repair budget per planner round.** `max_replan_rounds` defaults
  to 2; the right number is empirical. Capture under ADR
  [`0022`](0022-observability.md) metrics and tune.
- **Verifier reputation.** Two verifiers with the same role can
  have different historical accuracy; `Weighted` aggregation
  could derive weights from observed accuracy. Out of scope; ADR
  [`0042`] follow-up.
- **Cross-tenant redaction policy.** This ADR specifies that
  `cross_tenant: true` verifiers receive a redacted plan, but the
  redaction rule (hash, drop, abstract) is per-tenant policy and
  is not centrally defined yet — flagged for ADR
  [`0020`](0020-tenant-security-and-trust.md) follow-up.
- **Plan structure beyond v1.** `Plan::steps[]` is a flat list;
  team flows (ADR [`0045`]) may want a per-step DAG with rollback
  boundaries (ADR [`0031`](0031-transactional-code-changes.md)).
  Bumps to `plan/v2` when needed.
- **Verifier persona running on a non-LLM judge.** A grammar /
  static-analysis-driven plan checker (no LLM) fits the trait
  surface but has not been prototyped. Reserved as a future
  `plan_verifier` variant.

## References

- A2A spec: <https://github.com/google/a2a>
- Du et al., *Improving Factuality and Reasoning in Language
  Models through Multiagent Debate* (2023):
  <https://arxiv.org/abs/2305.14325>
- Google Research, *Towards a Science of Scaling Agent Systems —
  When and Why Agent Systems Work* (April 2025):
  <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/>
- Anthropic, Claude Code plan mode docs:
  <https://docs.claude.com/en/docs/claude-code/plan-mode>
- Aider architect/editor split:
  <https://aider.chat/docs/usage/modes.html>
- Cline plan vs. act mode:
  <https://docs.cline.bot/features/plan-and-act>
- Related ADRs: [`0003`](0003-a2a-protocol-model.md),
  [`0006`](0006-peer-delegation.md),
  [`0007`](0007-remote-a2a-agent-client.md),
  [`0011`](0011-native-llm-tool-calling.md),
  [`0016`](0016-artifact-storage.md),
  [`0017`](0017-webui-chat-client.md),
  [`0020`](0020-tenant-security-and-trust.md),
  [`0022`](0022-observability.md),
  [`0025`](0025-typed-output-validation-and-verifier-agent.md),
  [`0027`](0027-human-in-the-loop.md),
  [`0029`](0029-workspace-file-editor.md),
  [`0030`](0030-git-operations.md),
  [`0033`](0033-coding-agent-personas.md),
  [`0034`](0034-per-model-capability-profiles.md),
  [`0035`](0035-constrained-decoding.md),
  [`0037`](0037-lsp-diagnostics.md), 0039 (forthcoming),
  0040 (forthcoming), 0042 (forthcoming), 0045 (forthcoming).
