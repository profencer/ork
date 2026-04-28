# 0045 — Coding team orchestrator (architect agent)

- **Status:** Proposed
- **Date:** 2026-04-29
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0002, 0003, 0006, 0007, 0011, 0017, 0018, 0020, 0022, 0025, 0027, 0028, 0029, 0030, 0031, 0032, 0033, 0034, 0036, 0037, 0038, 0040, 0041, 0042, 0043, 0044
- **Supersedes:** —

## Context

ADR [`0033`](0033-coding-agent-personas.md) ships *personas* — typed
descriptors that turn a generic
[`LocalAgent`](../../crates/ork-agents/src/local.rs) into a named
coding agent (architect, executor, reviewer, tester, plan_verifier,
solo_coder). It also ships a *solo* reference: one persona, one
workflow template, one demo. Section 0033's `Decision` is explicit
that team composition — *which personas run in what order, how their
outputs feed each other, how diffs from multiple executors are
reconciled* — is **out of scope** there and owned by this ADR.

The platform-level prerequisites for that composition have all
landed in adjacent ADRs:

- ADR [`0038`](0038-plan-mode-and-cross-verification.md) gives us a
  typed `Plan` shape and an A2A plan cross-verification gate
  (`PlanCrossVerifier` aggregating `PlanVerdict`s under
  `Unanimous`/`Majority`/`FirstDeny`/`Weighted`).
- ADR [`0040`](0040-repo-map.md) provides
  workspace-keyed `get_repo_map` so every newly-dispatched sub-agent
  starts with structural ground truth in 1–4 K tokens instead of 8–20
  exploratory tool round-trips.
- ADR [`0041`](0041-nested-workspaces.md) provides parent/child
  worktrees and the `SubDiffArtifact` capture-on-Completed contract.
- ADR [`0042`](0042-capability-discovery.md) provides
  capability-tagged agent lookup with a `RankingPolicy::DiversityFromSet`
  primitive that picks "an agent whose role is `executor`, language
  includes Rust, edit_format includes `unified_diff`."
- ADR [`0043`](0043-team-shared-memory.md) provides the canonical
  `TeamId`, the `TeamMemory` port, and the append-only decision log
  with `supersedes` semantics that lets sibling sub-agents see "what
  we already tried."
- ADR [`0044`](0044-multi-agent-diff-aggregation.md) provides
  `TeamDiffAggregator` and the `TeamTransactionalCodeChange` step that
  composes N `SubDiffArtifact`s into one merged tree under a
  conflict policy with retry / HITL / fail-team escalation.
- ADR [`0018`](0018-dag-executor-enhancements.md) provides parallel
  step semantics, ADR [`0006`](0006-peer-delegation.md) +
  [`0007`](0007-remote-a2a-agent-client.md) provide the dispatch
  primitives, ADR [`0027`](0027-human-in-the-loop.md) provides the
  HITL escalation gate, ADR [`0017`](0017-webui-chat-client.md)
  surfaces all of the above to a human, and ADR
  [`0022`](0022-observability.md) gives the audit/tracing surface
  the headline demo will visualise.

What is missing — and what this ADR fixes — is the *policy* that
composes those parts: a reference **architect agent** that decomposes
a high-level coding goal into a graph of subtasks, drives them through
the prerequisite stack in the right order, and produces a single
green commit on a feature branch from a clean checkout. This is the
"ork as A2A coding-agent platform" headline demo: the feature that
single-process coding harnesses (Aider, opencode, Claude Code,
Cursor, Copilot Workspace) cannot offer because they lack the mesh,
peer delegation, capability discovery, team memory, and aggregation
primitives that ADRs 0006/0007/0042/0043/0044 supply.

The closest existing surfaces are deliberately *not* this:

- ADR [`0033`](0033-coding-agent-personas.md)'s solo workflow
  ([`workflow-templates/coding-agent-solo.yaml`](../../workflow-templates/))
  is one persona end-to-end. This ADR composes ≥ 3 personas in a
  graph.
- ADR [`0026`](0026-workflow-topology-selection-from-task-features.md)
  *picks a topology*; this ADR *implements one* (the team-shaped
  coding topology). 0026 may select us as one of its outputs.
- ADR [`0044`](0044-multi-agent-diff-aggregation.md) is the
  *integration step*. This ADR is the *orchestrator* that drives
  sub-task dispatch into and out of that step, and that owns the
  pre-integration plan-verification gate, the discovery loop, and
  the post-integration review.
- ADR [`0038`](0038-plan-mode-and-cross-verification.md)'s gate is
  a *primitive*; this ADR uses it twice — once on the architect's
  decomposition, once optionally on the merged diff (the latter
  delegated to ADR 0044's `cross_verification` policy).

## Decision

ork **introduces** a reference `TeamOrchestrator` agent
(`ork.persona.team_architect`) and a workflow step kind
`CodingTeamRun` that compose ADRs 0033/0038/0040/0041/0042/0043/0044
into one named end-to-end pipeline. The orchestrator owns three loops
in sequence: a *decomposition* loop that emits a `TeamPlan`
(structurally compatible with ADR 0038's `Plan`); a *dispatch* loop
that, for each ready subtask, runs ADR 0042 discovery → ADR 0041
sub-worktree allocation → ADR 0006/0007 delegation, in parallel where
the dependency DAG allows; and a *finalize* loop that hands the
captured `SubDiffArtifact`s to ADR 0044, runs the merged-diff
verifier (ADR 0025 + optional final plan_verifier pass), and either
ships a green commit on a feature branch or rolls back, retaining
the decision log and verifier findings as artifacts for the user.

A `CodingTeamRun` step is the team-level analogue of the solo
workflow shipped by ADR 0033: one team_id, one `TeamPlan`, one
merged-diff artifact, one observable outcome.

### Boundary with ADR 0033

| Concern | ADR 0033 | ADR 0045 (this) |
| ------- | -------- | --------------- |
| Layer | Personas (the building blocks) | Orchestration policy (how they're composed) |
| Surface | `CodingPersona` descriptor + four full + four stub personas | `TeamOrchestrator` agent + `CodingTeamRun` step + workflow template |
| Persona names introduced | `solo_coder`, `architect`, `executor`, `plan_verifier`, plus `reviewer.stub`, `tester.stub`, `security.stub`, `docs.stub` | `team_architect` only (a specialisation of `architect`) — every other persona consumed here is owned by 0033 |
| Stub-to-real promotions | n/a | Promotes `reviewer.stub`, `tester.stub` to fully-implemented personas |
| Dispatch | n/a — solo workflow is single-agent | Owns sub-task fan-out via ADR 0006/0007 + ADR 0042 discovery |
| Diff lifecycle | One `LocalAgent` editing one `WorkspaceHandle` | N sub-worktrees → ADR 0041 capture → ADR 0044 aggregation |
| Verification | Optional ADR 0038 gate via `verify_plan_step` toggle | Mandatory ADR 0038 gate on the team plan; optional ADR 0038 gate on the merged diff via ADR 0044's `cross_verification` |
| Demo target | One persona fixes a defect on a toy crate (stage 10) | 3–4 agent team ships a small feature on a toy crate (stage 11) |

ADR 0033 reserved persona ids; ADR 0042 indexes them; this ADR is the
first consumer that *composes* them. None of 0033's persona shapes
change. This ADR adds **one** new persona id (`ork.persona.team_architect`)
and **promotes** two stubs.

### `team_architect` persona (extends ADR 0033)

`team_architect` is a specialisation of ADR 0033's `architect`
persona. It inherits architect's `EditFormat::ReadOnly` and
`PersonaPhase::Plan` defaults but replaces the system prompt with one
tuned for *decomposition* rather than single-track planning:

```rust
// crates/ork-agents/src/personas/team_architect.rs

pub fn descriptor() -> CodingPersona {
    CodingPersona {
        id: PersonaId("ork.persona.team_architect"),
        role: PersonaRole::Architect,
        display_name: "Team Architect".into(),
        languages: vec![Language::Rust, Language::Python, Language::TypeScript],
        tool_catalog: ToolCatalog {
            // Read-only catalog plus the new structured-output tool.
            required: vec![
                "read_file".into(),
                "code_search".into(),
                "get_repo_map".into(),       // ADR 0040
                "get_diagnostics".into(),    // ADR 0037
                "team_recall".into(),        // ADR 0043
                "submit_team_plan".into(),   // this ADR
            ],
            optional: vec!["recall".into()],
        },
        system_prompt: SystemPrompt { template: TEAM_ARCHITECT_PROMPT.into() },
        edit_format: EditFormat::ReadOnly,
        default_phase: PersonaPhase::Plan,
        model_profile_ref: ModelProfileRef { profile: "ork.profiles.architect_default".into() },
        default_compaction_trigger_ratio: 0.7,
        default_memory_autoload_top_k: Some(8),
    }
}
```

The persona ships **one** new structured-output tool —
`submit_team_plan` — that terminates Plan and emits a typed `DataPart`
carrying a `TeamPlan`. This mirrors ADR 0033's `submit_plan_verdict`
(verdict shape) and ADR 0038's `propose_plan` (single-track plan
shape); the team plan is its team-level analogue.

### `TeamPlan` shape

A `TeamPlan` is a decomposition graph where every node is a subtask
spec and every edge is an explicit `depends_on`. Each node carries
*everything ADR 0042 discovery needs* (`required_role`, `language`,
`edit_format_in`) plus *everything ADR 0044 aggregation needs*
(`persona_phase` for `ApplicationOrder::ByRolePriority`,
`include_peer_diffs_on_retry` defaulting to true) plus the goal, the
file scope hint, and the success criteria the executor will validate
against.

```rust
// crates/ork-core/src/models/team_plan.rs

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TeamPlan {
    pub schema: String, // pinned to "https://ork.dev/schemas/team-plan/v1"
    pub objective: String,
    pub repo: String,
    pub trunk_branch: String,
    pub feature_branch: String,
    /// Optional; when present, the orchestrator's pre-dispatch plan
    /// cross-verification (ADR 0038) consumes this list verbatim.
    pub plan_verifiers: Vec<DiscoveryFilter>, // ADR 0042
    pub subtasks: Vec<TeamPlanSubtask>,
    /// Acceptance criteria the merged tree must satisfy. Surfaced to
    /// the final verifier step (ADR 0025) and to the optional
    /// final-plan_verifier pass.
    pub acceptance_criteria: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TeamPlanSubtask {
    pub subtask_id: String,        // stable within a TeamPlan
    pub goal: String,
    pub required_role: PersonaRole,         // ADR 0033
    pub language: Language,                 // ADR 0033
    /// Soft hint; the agent is not constrained to it but team
    /// memory tags peer diffs by overlap with this set.
    pub file_scope_hint: Vec<String>,
    pub dependencies: Vec<String>,           // subtask_id list
    pub success_criteria: Vec<String>,       // executor self-check
    pub est_complexity: ComplexityBand,
    /// Optional: caller-supplied filter that overrides the
    /// orchestrator's default `DiscoveryFilter` for this slot.
    pub agent_selection: Option<AgentSelectionPolicy>,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplexityBand {
    Trivial,    // single-file edit, < 50 LoC
    Small,      // 1–3 files, < 200 LoC
    Medium,     // 3–10 files, < 1000 LoC
    Large,      // > 10 files or > 1000 LoC — orchestrator may sub-decompose
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentSelectionPolicy {
    /// Use this `DiscoveryFilter` against ADR 0042; pick the top-1.
    Discovery { filter: DiscoveryFilter, ranking: RankingPolicyKind },
    /// Caller pinned a specific agent (escape hatch; bypasses
    /// discovery — observability records this as `pinned`).
    Pinned { agent_id: AgentId },
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingPolicyKind {
    ClosestMatch,
    Cheapest,
    LowestLatency,
    /// Default for executors when there is no peer to be diverse from.
    /// Falls back to ClosestMatch internally.
    DiversityFromPlanner,
}
```

`TeamPlan::schema` is the same versioning-by-URL discipline ADR 0033
uses for `PlanVerificationRequest`. The architect's prompt instructs
the model to call `submit_team_plan` exactly once; the agent loop
terminates on first call. JSON-Schema constrained decoding (ADR
[`0035`](0035-constrained-decoding.md)) is reused.

The DAG must be acyclic; the orchestrator validates this at receipt
and rejects the plan with a `request_changes` verdict (recorded as a
plan_verifier-shaped finding) if it is not. Cycles are the only
fast-fail at the orchestrator layer; everything else flows through
ADR 0038.

### Decomposition is itself a plan: the *team* plan-verification gate

The `TeamPlan` emitted by `team_architect` goes through ADR 0038's
`PlanCrossVerifier` *before* any sub-agent is dispatched. The gate
shape is **identical** to the per-step plan-verification gate ADR
0038 already specifies — the only difference is the `Plan` payload
embedded in the `DataPart` is a `TeamPlan` rather than a single-track
plan. Verifier peers receive:

- the full `TeamPlan` (subtasks + dependencies + acceptance criteria),
- a `repo_context` block with the repo map (ADR 0040), the
  team's recent decision-log entries (ADR 0043, top 8 by `last_seen`),
  and the trunk commit recorded at team formation,
- a `review_brief` autogenerated from the orchestrator's prompt
  template (e.g. *"Look for missing dependencies, over-decomposition,
  or subtasks that touch overlapping files without explicit ordering"*).

Every verifier returns the existing `PlanVerdict` shape (ADR 0033's
schema URL); the `PlanCrossVerifier` aggregates them under whichever
`AggregationPolicy` the workflow declared. The decomposition gate
enforces:

- `verdict = approve` → proceed to dispatch.
- `verdict = request_changes` → re-prompt `team_architect` with the
  aggregated findings (ADR 0038's `repair` loop, bounded by
  `max_replans`).
- `verdict = reject` → fail the run with `TeamRunResolution::PlanRejected`.

`max_replans` defaults to 2. After exhaustion the run escalates to
HITL via ADR 0027 with the rejected plan, the verdict bundle, and
the option to (a) approve the plan as-is, (b) supply a corrected
plan inline, or (c) cancel.

The team plan-verification gate reuses **all** of ADR 0038's
infrastructure — verifier discovery via ADR 0042 (when
`plan_verifiers` is omitted, default `DiscoveryFilter { role:
PlanVerifier, .. }`), invalid-verdict policy, timeout policy, repair
loop semantics, audit events. This ADR adds zero new verifier wire
shapes; it ships one new *schema URL* for the team-shaped plan.

### Dispatch loop: discovery → sub-worktree → delegation, parallel

Once the `TeamPlan` is approved, the orchestrator topologically sorts
the subtask DAG and dispatches every subtask whose `dependencies` are
all satisfied. The dispatch step for a single subtask is:

```
for ready subtask S in TeamPlan:
    1. Discovery (ADR 0042):
       filter = S.agent_selection.filter || default_for(S.required_role, S.language)
       hits   = CapabilityDiscovery::query(tenant, { filter, ranking, limit: 1 })
       if hits.is_empty(): record DispatchFailure { reason: NoCandidate }
                          → trigger no-candidate policy (default: HITL)
       agent_ref = hits[0].agent_id
    2. Sub-worktree (ADR 0041):
       sub = SubWorkspaceCoordinator::open_sub(parent = trunk, subtask_id = S.subtask_id)
    3. Context priming (this ADR):
       message = build_subtask_message(
           goal: S.goal,
           success_criteria: S.success_criteria,
           file_scope_hint: S.file_scope_hint,
           workspace_ref: sub.workspace_ref,
           repo_map: get_repo_map(sub),                           # ADR 0040
           team_decisions: TeamMemory::recent_decisions(team, 8), # ADR 0043
           team_notes:     TeamMemory::recall(team, S.goal, 5),   # ADR 0043
       )
    4. Delegation (ADR 0006/0007):
       task = A2aRemoteAgent::send_task(agent_ref, message)
    5. On Completed:
       diff_artifact = SubWorkspaceCoordinator::capture_diff(sub) # ADR 0041
       record SubtaskOutcome { subtask_id, agent_id, diff_artifact }
```

Independent subtasks dispatch **in parallel**: the orchestrator emits
them as parallel children under the same workflow scope using ADR
[`0018`](0018-dag-executor-enhancements.md)'s parallel-branch
semantics. Concurrency is bounded by
`engine.team_run.default_parallelism` (default 4); the bound is per
team, not per tenant.

The orchestrator does *not* compete with ADR 0044 for the
integration step — when all subtasks reach `Completed` (or any
terminal state), control passes to a `TeamTransactionalCodeChange`
step whose `subtasks` field is filled in by the orchestrator from
the resolved `TeamPlan`. ADR 0044 owns apply / validate /
cross_verify / finalize; this ADR is the producer of the
`SubDiffArtifact` set.

### Sub-agent context priming

Every dispatched sub-agent receives the same three pieces in the
initial A2A message:

1. **The repo map (ADR 0040)** — pinned to the sub-worktree's
   `head_commit`, body-free, structured. Surfaced as a `DataPart`
   with `kind = "repo_map.v1"`.
2. **A relevant slice of the team decision log (ADR 0043)** — the
   top-8 most-recent decision-log entries (`recent_decisions`) plus
   the top-K notes from `recall(team, subtask.goal, K=5)`. Surfaced
   as a `DataPart` with `kind = "team_recall.v1"`. Auto-priming honours
   the agent's per-`ModelProfile` `team_memory_autoload_top_k`.
3. **The subtask spec** — `goal`, `success_criteria`,
   `file_scope_hint`, the orchestrator's prompt template, and the
   `team_id` so the sub-agent's tool calls resolve to the right
   `TeamMemory` bucket. Surfaced as a `TextPart` plus the structured
   `subtask_spec.v1` `DataPart`.

The sub-agent runs whatever loop its persona prescribes (executor,
reviewer, tester, …) — the orchestrator does not mandate phase
internals. On `Completed`, the sub-agent's last `DataPart` payload
(typically a small summary plus the success-criteria self-check) is
recorded alongside the captured `SubDiffArtifact` for the
aggregator's `IntegrationApplyOutcome` audit.

### Integration: hand off to ADR 0044

When all sub-tasks are terminal, the orchestrator constructs the
`WorkflowStep::TeamTransactionalCodeChange` payload from the resolved
plan. The mapping is mechanical:

| `TeamPlan` field / state | `TeamTransactionalCodeChange` field |
| ------------------------ | ----------------------------------- |
| `team_id` (formed at run start, ADR 0043) | `team_id` |
| `repo`, `trunk_branch` | `repo`, `trunk_branch` |
| Resolved `(subtask_id, agent_id, persona_phase, include_peer_diffs_on_retry)` quads | `subtasks: Vec<TeamSubtaskSpec>` |
| `engine.team_run.default_application_order` (default `ByRolePriority`) | `application_order` |
| `team_run.default_conflict_policy` (default: `ReDispatch{max_retries=2, escalation=EscalateToHuman}`) | `conflict_policy` |
| `TeamPlan.plan_verifiers` mapped through ADR 0042 + `team_run.merged_diff_verification_aggregation` | `cross_verification` (optional) |
| `TeamPlan` `est_complexity`-based timeout, capped at workflow timeout | `timeout` |

ADR 0044 takes it from there: open integration worktree, apply N
diffs, validate (LSP + tests + emergent-conflict detection),
optionally cross-verify the merged diff, and finalize (FF-merge to
trunk on success, escalation on conflict per the policy).

`IntegrationConflictPolicy::ReDispatch` triggers ADR 0044's
peer-diff-as-`team_remember` retry path; the orchestrator's
`on_re_dispatch` callback (registered with the engine) re-opens the
named subtasks via the same dispatch loop above (with new sub-worktree
ids per ADR 0041's `-r<retry_count>` suffix). This is **structurally
the same** as the first dispatch round; the only delta is the
sub-agent's `team_recall` automatically surfaces the peer diffs ADR
0044 wrote to the bucket.

### Final review

After ADR 0044 finalizes with `Committed { merge_commit }`, the
orchestrator runs **one last** review pass:

1. **ADR 0025 verifier** — the merged diff's structured outputs (the
   per-subtask success-criteria self-check `DataPart`s collected
   above, plus the merged-diff artifact) are handed to ADR 0025's
   `Verifier` port. The verifier defaults to a rubric-based check
   ("does each acceptance criterion appear satisfied by the merged
   diff?") and emits a `RunVerdict`.
2. **Optional final plan_verifier pass** — if the workflow set
   `final_plan_verification: Some(_)`, the orchestrator dispatches
   the merged diff *plus* the resolved `TeamPlan` *plus* the
   acceptance criteria to N independent `plan_verifier` peers via
   ADR 0038, asking *"does the merged outcome match the original
   plan's intent?"*. This is distinct from ADR 0044's
   `cross_verification`: 0044's gate runs *before* the FF merge and
   reasons about textual/semantic/test conflicts; this gate runs
   *after* and reasons about *plan ↔ outcome alignment*. Both are
   optional; both reuse ADR 0038's `submit_plan_verdict` shape.

If both pass (or neither is configured), the run terminates as
`TeamRunResolution::Shipped { merge_commit, branch: feature_branch }`.
If either rejects, the orchestrator transitions to `Rolling back the
merge`: ADR 0044's resolution is already committed (FF is the only
trunk write), so rollback at this layer means *opening a follow-up
revert task* on the same feature branch via the same dispatch loop.
The trunk commit is **not** reverted by the orchestrator unilaterally;
that escalates to HITL.

The final-review delineation is the contract: ADR 0044's
`cross_verification` reviews *the diff*; this ADR's final review
checks *plan ↔ outcome alignment*.

### Failure modes and recovery

| Failure | Detected by | Recovery | Final state |
| ------- | ----------- | -------- | ----------- |
| Architect produces a cyclic / malformed `TeamPlan` | `submit_team_plan` schema check + DAG validator at receipt | Re-prompt `team_architect` with a synthetic `PlanVerdict` finding (`severity: blocker`); retry up to `max_decomposition_retries` (default 2) | `PlanRejected` after exhaustion |
| Decomposition gate `request_changes` | ADR 0038's plan cross-verification | ADR 0038 `repair` loop; bounded by `max_replans` (default 2); HITL on exhaustion | Either `PlanApproved` or `HumanRequested` |
| Decomposition gate `reject` | ADR 0038 | No retry; HITL with the rejected plan and verdicts | `PlanRejected` after HITL `Cancel` |
| No discovery candidate for a subtask role/language | ADR 0042 returns empty hits | (a) Re-prompt architect to widen the slot's `agent_selection` (default), (b) HITL escalation if widening also fails | `NoCandidate` after exhaustion |
| Single subtask fails (sub-agent emits `Failed`/timeout) | A2A task lifecycle (ADR 0008) | Re-dispatch with a `team_remember` decision-log addendum (kind `subtask_retry`, body = failure reason + re-prompt); bounded by `max_subtask_retries` (default 1) | `SubtaskFailed` after exhaustion |
| Repeated subtask failure (same subtask exhausts retries) | This ADR's retry budget | Escalate to HITL (ADR 0027) with the failed subtask's transcript, the `TeamPlan`, and the option to (a) skip the subtask, (b) supply a manual diff, (c) cancel the run | Either `SubtaskSkipped`, `SubtaskHumanFix`, or `Cancelled` |
| Integration conflict (textual / semantic / test) | ADR 0044's `validate` | ADR 0044's `IntegrationConflictPolicy` owns this; the orchestrator's role is to receive the `ReDispatching` callback and re-dispatch the named subtasks | Either eventual `Committed` or ADR 0044's escalation path |
| Cross-verification reject (pre-merge) | ADR 0044's `cross_verify_merged` | ADR 0044's policy; orchestrator participates only if the policy resolves to re-dispatch | `Aborted { CrossVerifierRejected }` on terminal reject |
| Final plan_verifier reject | This ADR's final review | Open a follow-up revert subtask on the feature branch; record the verdicts as audit findings | `Shipped { needs_revert: true }` recorded |
| Final ADR 0025 verifier `RunVerdict::Reject` | This ADR's final review | Same as final plan_verifier reject | `Shipped { needs_revert: true }` |
| Team-wide rollback (orchestrator-level abort) | Orchestrator (cancellation, timeout, fatal error) | Drop **all** open sub-worktrees via ADR 0041; ADR 0044's `abort(Cancelled)` cascade; trunk untouched; the decision log and per-subtask transcripts are retained as ADR 0016 artifacts under `team-{team_id}/` | `Cancelled { sub_resolutions, retained_artifacts }` |

The "retain decision log and findings as artifacts for the user"
discipline is load-bearing: even on a cancelled run, the architect's
`TeamPlan`, every verdict bundle, every subtask's transcript, and the
final decision-log snapshot are written as ADR 0016 artifacts under a
deterministic prefix
(`tenant-{tenant_id}/run-{run_id}/team-{team_id}/`). The user can
re-open the run in the web UI (ADR 0017) and inspect *what was
decided and why* even if no code shipped. ADR 0043 already namespaces
the decision log; this ADR pins the export format.

### `CodingTeamRun` workflow step

```rust
// crates/ork-core/src/models/workflow.rs — new variant alongside
// ADR 0044's TeamTransactionalCodeChange.

WorkflowStep::CodingTeamRun {
    id: String,
    team_architect: AgentRef,                    // ADR 0007
    repo: String,
    trunk_branch: String,
    feature_branch_template: Option<String>,     // default: "ork/run-{run_id}/team-{team_id}"
    /// Brief / objective handed to the architect.
    objective_template: String,
    /// Plan verification on the team plan (ADR 0038).
    plan_verification: PlanVerificationPolicy,
    /// Subtask-side controls.
    subtask_dispatch: SubtaskDispatchPolicy,
    /// Aggregation of the merged diff (ADR 0044).
    integration: IntegrationPolicy,
    /// Final review pass on the merged outcome.
    final_review: FinalReviewPolicy,
    timeout: Option<Duration>,
    depends_on: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanVerificationPolicy {
    /// `None` → discovery-driven default (top-3 plan_verifiers
    /// diverse from the architect by ADR 0042's `DiversityFromSet`).
    pub verifiers: Option<Vec<PlanVerifierTarget>>,    // ADR 0038
    pub aggregation: AggregationPolicy,                // ADR 0038
    pub max_replans: u32,                              // default 2
    pub on_replan_exhausted: ReplanExhaustionPolicy,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplanExhaustionPolicy {
    EscalateToHuman { required_scopes: Vec<String>, prompt: String, approval_timeout: Option<Duration> },
    FailRun,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SubtaskDispatchPolicy {
    pub max_parallelism: u32,                          // default 4
    pub max_subtask_retries: u32,                      // default 1
    pub on_no_candidate: NoCandidatePolicy,
    pub on_subtask_exhausted: SubtaskExhaustionPolicy,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoCandidatePolicy {
    /// Re-prompt the architect, asking it to widen the slot's
    /// `agent_selection.filter`. Default.
    WidenAndReplan { max_widenings: u32 },
    EscalateToHuman { required_scopes: Vec<String>, prompt: String },
    FailSubtask,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubtaskExhaustionPolicy {
    EscalateToHuman { required_scopes: Vec<String>, prompt: String, approval_timeout: Option<Duration> },
    SkipSubtask,
    FailRun,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IntegrationPolicy {
    pub application_order: ApplicationOrder,           // ADR 0044
    pub conflict_policy: IntegrationConflictPolicy,    // ADR 0044
    pub cross_verification: Option<MergedDiffVerificationPolicy>, // ADR 0044
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FinalReviewPolicy {
    /// Run ADR 0025's RunVerifier on the merged diff. Default true.
    pub run_verifier_enabled: bool,
    /// Optional N-peer plan_verifier pass on (TeamPlan, merged_diff).
    /// Distinct from ADR 0044's pre-merge cross-verification.
    pub final_plan_verification: Option<PlanVerificationPolicy>,
    pub on_reject: FinalRejectPolicy,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalRejectPolicy {
    /// Open a follow-up revert subtask on the feature branch.
    /// Default; trunk commit is **not** unilaterally reverted.
    OpenRevertSubtask,
    EscalateToHuman,
}
```

### Workflow template

A new template at
[`workflow-templates/coding-team-orchestrator.yaml`](../../workflow-templates/)
wires the above:

```yaml
name: coding-team-orchestrator
version: 1
description: Architect + parallel executors + plan_verifier + reviewer
             ship a small feature on a feature branch from clean checkout
             to green commit.
inputs:
  - { name: repo,             type: string, required: true }
  - { name: trunk_branch,     type: string, default: main }
  - { name: feature_branch,   type: string, required: true }
  - { name: objective,        type: string, required: true }
  - { name: max_parallelism,  type: int,    default: 4 }
steps:
  - id: team-run
    kind: coding_team_run
    team_architect:
      id: ork.agent.persona.team_architect.opus
    repo: "{{ inputs.repo }}"
    trunk_branch: "{{ inputs.trunk_branch }}"
    feature_branch_template: "{{ inputs.feature_branch }}"
    objective_template: "{{ inputs.objective }}"
    plan_verification:
      verifiers: null   # discovery-driven default
      aggregation: majority
      max_replans: 2
      on_replan_exhausted:
        kind: escalate_to_human
        required_scopes: ["workflow:approve:team_plan"]
        prompt: "Architect's decomposition was rejected twice. Approve as-is, supply a corrected plan, or cancel."
    subtask_dispatch:
      max_parallelism: "{{ inputs.max_parallelism }}"
      max_subtask_retries: 1
      on_no_candidate:
        kind: widen_and_replan
        max_widenings: 1
      on_subtask_exhausted:
        kind: escalate_to_human
        required_scopes: ["workflow:approve:subtask_skip"]
        prompt: "Subtask exhausted retries. Skip, supply a manual fix, or cancel."
    integration:
      application_order: { kind: by_role_priority }
      conflict_policy:
        kind: re_dispatch
        max_retries: 2
        offender_selection: all_touched
        escalation:
          kind: escalate_to_human
          required_scopes: ["workflow:approve:team_integration"]
          prompt: "Integration retries exhausted. Inspect diffs and decide."
          approval_timeout: 1h
          on_approval_timeout: rollback
      cross_verification:
        verifiers:
          - { agent_ref: { id: ork.agent.plan_verifier.haiku }, weight: 1.0, vetoes: false }
          - { agent_ref: { id: ork.agent.plan_verifier.opus  }, weight: 2.0, vetoes: true  }
        aggregation: weighted
        timeout_per_verifier: 60s
        on_timeout: fail_closed
        on_invalid_verdict: retry_once
    final_review:
      run_verifier_enabled: true
      final_plan_verification:
        verifiers: null    # discovery-driven; default top-3 diverse
        aggregation: majority
        max_replans: 0
        on_replan_exhausted: { kind: fail_run }
      on_reject: open_revert_subtask
    timeout: 90m
```

The template uses persona-shaped agent ids (`ork.agent.persona.*`) so
operators wire concrete deployments without changing the template;
this matches ADR 0033's solo template discipline.

### Demo: stage-11

A new script
[`demo/scripts/stage-11-coding-team.sh`](../../demo/scripts/) runs the
full flow against the same toy crate ADR 0033 introduced for stage-10
([`demo/toy-crate/`](../../demo/), created if absent):

1. Bootstrap a clean checkout via the existing
   [`demo/scripts/lib.sh`](../../demo/scripts/lib.sh).
2. Boot ork-api with the persona registry pre-installing
   `team_architect`, two `executor` instances on different model
   profiles (`opus` and `haiku`), one `plan_verifier`, one
   `reviewer`, and one `tester`.
3. Submit a "ship a small feature" task through the web UI gateway
   from ADR [`0017`](0017-webui-chat-client.md) — the headline
   reference task is *"Add a `crates/ork-toy/src/greet.rs` module
   exporting `pub fn greet(name: &str) -> String` with a unit test
   and a doctest."*
4. Stream the run in the browser; the UI surfaces:
    - the architect's `TeamPlan` (rendered from the `submit_team_plan`
      `DataPart`),
    - the plan-verification verdict bundle,
    - the per-subtask sub-task panels (one per executor + reviewer +
      tester) running in parallel under the same parent run,
    - the team decision log (ADR 0043) updating live,
    - the merged-diff artifact (ADR 0044) with conflict findings (in
      this scripted demo: zero textual, zero semantic, zero test),
    - the final review verdict.
5. Assert via `curl` that the final task state is `Completed`,
   `git log <feature_branch> -1` shows the merge commit, and
   `git diff trunk..<feature_branch> --stat` matches the expected
   one-file-changed line.

Expected deltas land in
[`demo/expected/stage-11.txt`](../../demo/expected/) and
[`demo/expected/stage-11-tree.txt`](../../demo/expected/), matching
the convention used by stages 0–10.

The *visible payoff* — and the reason this is the headline demo — is
that the user sees the full team mechanics in one screen: discovery
hits, the plan + verdict bundle, parallel sub-task panels, the
decision log, the aggregated diff, the final review. Every part is a
named ADR; the orchestrator is the glue.

### Observability: per-team summary in the event log

ADR [`0022`](0022-observability.md) gains a per-team summary event
emitted at every terminal transition of a `CodingTeamRun`:

```
audit.team_run.summary
  team_id: TeamId
  run_id: RunId
  resolution: TeamRunResolution
  subtasks_dispatched: u32
  subtasks_succeeded: u32
  subtasks_failed: u32
  agents_used: Vec<AgentId>          # de-duplicated
  models_used: Vec<String>           # resolved profile.model_id, de-duplicated
  total_tokens: { prompt: u64, completion: u64 }
  total_latency_ms: u64
  conflicts: { textual: u32, semantic: u32, test: u32 }
  conflict_resolutions: { re_dispatched: u32, hitl: u32, failed: u32 }
  cross_verification_decisions: { approved: u32, request_changes: u32, rejected: u32 }
  retained_artifact_count: u32       # ADR 0016 artifacts under team-{team_id}/
```

In addition, the orchestrator emits per-phase events:

| Event | Emitted when | Carries |
| ----- | ------------ | ------- |
| `audit.team_run.opened` | `team_id` minted, trunk commit recorded | team_id, repo, trunk_branch, trunk_commit |
| `audit.team_run.plan_emitted` | `submit_team_plan` returns | team_id, subtask_count, subtask_role_breakdown |
| `audit.team_run.plan_verified` | Plan-verification gate completes | team_id, aggregated decision, verdict_artifact_ids |
| `audit.team_run.subtask_dispatched` | Each subtask delegation `send_task` returns | team_id, subtask_id, agent_id, sub_workspace_id |
| `audit.team_run.subtask_completed` | Each subtask reaches terminal A2A state | team_id, subtask_id, terminal_state, sub_diff_artifact_id (if any) |
| `audit.team_run.integration_handed_off` | Control transferred to ADR 0044 | team_id, team_transaction_id, subtask_count |
| `audit.team_run.final_reviewed` | Final review pass terminates | team_id, decision, verdict_artifact_ids |
| `audit.team_run.shipped` | `Shipped { merge_commit }` recorded | team_id, merge_commit, feature_branch, needs_revert |
| `audit.team_run.cancelled` | `Cancelled` recorded | team_id, reason, retained_artifact_count |

OTLP spans nest: `team_run:<team_id>` is the parent of every
`plan:*`, per-subtask `subtask:<subtask_id>` (rooted at the existing
A2A delegation span), `team_transaction:<id>` (rooted at ADR 0044's
existing schema), `final_review:*`, and the per-phase verifier spans.
The merged-diff capture and FF-merge sub-operations remain rooted
under ADR 0044's spans; this ADR re-parents them under the team-run
span without re-implementing them.

### Engine integration

```rust
// crates/ork-core/src/workflow/engine.rs — new arm.

impl WorkflowEngine {
    async fn run_team_run(&self, ctx: &mut RunContext, step: &CodingTeamRunStep)
        -> Result<StepResult, OrkError>
    {
        // 1. Form team.
        let team_id = TeamId::new();                          // ADR 0043
        ctx.team_memory.open_team(team_id).await?;
        self.audit.team_run_opened(&ctx, team_id, ...);

        // 2. Dispatch architect → submit_team_plan.
        let plan = self.dispatch_team_architect(ctx, step, team_id).await?;
        ctx.team_memory.append_decision(team_id, "team_plan", &plan).await?;

        // 3. Plan-verification gate (ADR 0038).
        let verdict = self.plan_cross_verifier
            .verify_team_plan(ctx, team_id, &plan, &step.plan_verification)
            .await?;
        match verdict.aggregated {
            PlanGateDecision::Approved => {}
            PlanGateDecision::RequestChanges { findings } => {
                return self.replan_or_escalate(ctx, step, team_id, plan, findings).await;
            }
            PlanGateDecision::Rejected => {
                return Ok(StepResult::failed(TeamRunResolution::PlanRejected));
            }
        }

        // 4. Topologically-sorted parallel dispatch.
        let outcomes = self.dispatch_subtasks(ctx, step, team_id, &plan).await?;

        // 5. Hand off to ADR 0044.
        let team_tx = self.team_diff_aggregator
            .run_team_transaction(ctx, team_id, &plan, outcomes, &step.integration)
            .await?;

        // 6. Final review.
        let final_resolution = self.final_review(ctx, team_id, &plan, &team_tx, &step.final_review).await?;

        // 7. Summarise + emit audit event.
        self.audit.team_run_summary(&ctx, team_id, &final_resolution, &outcomes, &team_tx);

        Ok(StepResult::from_team_run(final_resolution))
    }
}
```

The arm is glue; every load-bearing primitive comes from a numbered
ADR. The new code is the orchestration policy itself —
`replan_or_escalate`, `dispatch_subtasks`, `final_review`, and the
audit summary — none of which has a natural home in any other ADR.

### Persistence

The team-run pipeline persists per-step state alongside ADR 0044's
`team_transactions` table:

```sql
-- migrations/012_team_runs.sql

CREATE TABLE team_runs (
    team_id                 UUID PRIMARY KEY,            -- ADR 0043
    tenant_id               TEXT NOT NULL,
    run_id                  UUID NOT NULL,
    step_id                 TEXT NOT NULL,
    repo                    TEXT NOT NULL,
    trunk_branch            TEXT NOT NULL,
    trunk_commit            TEXT NOT NULL,
    feature_branch          TEXT NOT NULL,
    team_plan_artifact_id   TEXT,
    plan_verdict_artifact_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    final_review_artifact_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    team_transaction_id     UUID REFERENCES team_transactions(team_transaction_id),
    resolution              JSONB,
    opened_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at               TIMESTAMPTZ,
    UNIQUE (tenant_id, run_id, step_id)
);

CREATE TABLE team_run_subtasks (
    team_id                 UUID NOT NULL
        REFERENCES team_runs(team_id) ON DELETE CASCADE,
    subtask_id              TEXT NOT NULL,
    retry_index             INT NOT NULL DEFAULT 0,
    agent_id                TEXT NOT NULL,
    sub_workspace_id        UUID,
    sub_diff_artifact_id    TEXT,
    terminal_state          TEXT,
    PRIMARY KEY (team_id, subtask_id, retry_index)
);
```

Crash-recovery semantics mirror ADR 0044's: a `team_runs` row whose
`team_transaction_id` is non-null defers to ADR 0044's recovery; a
row with no `team_transaction_id` and a non-null `team_plan_artifact_id`
re-enters at the dispatch loop (the architect's plan is durable);
rows with no `team_plan_artifact_id` are aborted and the architect is
re-dispatched.

### Configuration

```toml
# config/default.toml
[engine.team_run]
default_parallelism                       = 4
default_max_decomposition_retries         = 2
default_max_replans                       = 2
default_max_subtask_retries               = 1
default_application_order                 = "by_role_priority"
default_conflict_policy_max_retries       = 2
default_final_review_run_verifier_enabled = true
retained_artifact_prefix                  = "team-{team_id}"
plan_verifier_default_top_k               = 3
```

## Acceptance criteria

- [ ] Type `TeamPlan` defined at
      `crates/ork-core/src/models/team_plan.rs` with the fields shown
      in `Decision`, alongside `TeamPlanSubtask`, `ComplexityBand`,
      `AgentSelectionPolicy`, `RankingPolicyKind`. Schema URL
      constant `TEAM_PLAN_SCHEMA_V1 = "https://ork.dev/schemas/team-plan/v1"`.
- [ ] `TeamPlan` serde round-trip stable, verified by
      `crates/ork-core/tests/team_plan_roundtrip.rs::roundtrip_full`
      and `::roundtrip_minimal`.
- [ ] DAG validator at `crates/ork-core/src/models/team_plan.rs::validate_dag`
      rejects cyclic plans with `OrkError::Validation("team_plan cycle: ...")`,
      verified by `crates/ork-core/tests/team_plan_validate.rs::rejects_cycle`
      and `::accepts_diamond`.
- [ ] Persona descriptor `ork.persona.team_architect` defined at
      `crates/ork-agents/src/personas/team_architect.rs` with
      `EditFormat::ReadOnly`, `PersonaPhase::Plan`, and the
      tool catalog shown in `Decision`.
- [ ] `PersonaRegistry::with_defaults` registers
      `ork.persona.team_architect` in addition to ADR 0033's set,
      verified by
      `crates/ork-agents/tests/persona_registry.rs::team_architect_registered`.
- [ ] Native tool `submit_team_plan` registered in
      [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
      with a JSON Schema mirroring `TeamPlan` and the agent loop
      terminating on first call, verified by
      `crates/ork-agents/tests/team_architect_smoke.rs::terminates_on_submit_team_plan`.
- [ ] Stub personas `reviewer.stub` and `tester.stub` from ADR 0033
      promoted to full implementations at
      `crates/ork-agents/src/personas/reviewer.rs` and
      `crates/ork-agents/src/personas/tester.rs`, with
      `EditFormat::UnifiedDiff` for `tester` and
      `EditFormat::ReadOnly` for `reviewer`; `unimplemented!("ADR 0045")`
      panics removed; verified by
      `crates/ork-agents/tests/persona_registry.rs::stubs_promoted`.
- [ ] Step-kind enum variant `WorkflowStep::CodingTeamRun` defined in
      [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
      with the fields shown in `Decision`, alongside
      `PlanVerificationPolicy`, `ReplanExhaustionPolicy`,
      `SubtaskDispatchPolicy`, `NoCandidatePolicy`,
      `SubtaskExhaustionPolicy`, `IntegrationPolicy`,
      `FinalReviewPolicy`, `FinalRejectPolicy`.
- [ ] YAML round-trip parses the example under `Decision` into the
      enum variant, verified by
      `crates/ork-core/tests/workflow_kinds/coding_team_run_yaml.rs::round_trip_full`
      and `::round_trip_minimal`.
- [ ] Engine arm `WorkflowEngine::run_team_run` defined at
      [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
      walks the pipeline `open → architect → plan_verify → dispatch →
      integrate → final_review → summary`.
- [ ] Migration `migrations/012_team_runs.sql` creates the
      `team_runs` and `team_run_subtasks` tables with the schema
      shown in `Persistence`, including the
      `UNIQUE (tenant_id, run_id, step_id)` constraint and the
      foreign-key reference to `team_transactions`.
- [ ] Integration test
      `crates/ork-core/tests/team_run_smoke.rs::happy_path_ships_feature`
      asserts: a stub-LLM-driven team_architect emits a 3-subtask
      plan, plan-verification approves under `Majority`, three
      stub-LLM executors complete in parallel, ADR 0044 commits the
      merged diff, final review approves, and the run terminates as
      `Shipped { merge_commit, .. }`.
- [ ] Integration test
      `crates/ork-core/tests/team_run_smoke.rs::cyclic_plan_rejected`
      asserts: a team_architect that emits a cyclic plan re-prompts
      with a synthetic blocker finding, retries up to
      `max_decomposition_retries`, and eventually returns
      `PlanRejected`.
- [ ] Integration test
      `crates/ork-core/tests/team_run_smoke.rs::no_candidate_widens_then_escalates`
      asserts: a subtask whose `required_role` has no
      capability-discovery hits triggers `WidenAndReplan`; the
      architect emits a wider filter; if widening also yields no
      hits, the run escalates to HITL.
- [ ] Integration test
      `crates/ork-core/tests/team_run_smoke.rs::repeated_subtask_failure_escalates`
      asserts: a subtask that exhausts `max_subtask_retries` opens an
      ADR-0027 `HumanInputGate` request whose payload contains the
      failed subtask's transcript and the `TeamPlan`.
- [ ] Integration test
      `crates/ork-core/tests/team_run_smoke.rs::team_rollback_drops_subworktrees`
      asserts: an orchestrator-level cancellation drops every open
      sub-worktree via ADR 0041, cascades `abort(Cancelled)` to ADR
      0044, leaves trunk untouched, and writes the decision log
      snapshot as a `team-{team_id}/decision-log.json` ADR 0016
      artifact.
- [ ] Integration test
      `crates/ork-core/tests/team_run_smoke.rs::final_plan_verifier_reject_opens_revert`
      asserts: a final-plan-verifier `reject` after a successful FF
      merge transitions the run to `Shipped { needs_revert: true }`
      and opens a follow-up revert subtask on the feature branch
      (the trunk commit is not unilaterally reverted).
- [ ] Per-phase audit events
      `audit.team_run.{opened, plan_emitted, plan_verified,
      subtask_dispatched, subtask_completed, integration_handed_off,
      final_reviewed, shipped, cancelled}` emitted at the transitions
      documented in `Observability`, verified by
      `crates/ork-core/tests/team_run_observability.rs::events_emitted_at_transitions`.
- [ ] Terminal `audit.team_run.summary` emitted with the fields
      documented in `Observability`, verified by
      `crates/ork-core/tests/team_run_observability.rs::summary_fields_complete`.
- [ ] OTLP spans nest correctly: `subtask:*` and
      `team_transaction:*` spans are parented under
      `team_run:<team_id>`, verified by
      `crates/ork-core/tests/team_run_observability.rs::span_hierarchy`.
- [ ] [`workflow-templates/coding-team-orchestrator.yaml`](../../workflow-templates/)
      created with the steps shown in `Decision`; loaded by the
      workflow compiler without errors —
      `cargo test -p ork-core compiler::loads_team_orchestrator_template`.
- [ ] [`demo/scripts/stage-11-coding-team.sh`](../../demo/scripts/)
      runs end-to-end against a toy crate, reaches `TaskState::Completed`,
      and produces a single merge commit on the feature branch with
      a 3-subtask team (`team_architect` + 2 `executor` + `reviewer` +
      `tester`); the script's output matches
      [`demo/expected/stage-11.txt`](../../demo/expected/) and the
      worktree shape matches
      [`demo/expected/stage-11-tree.txt`](../../demo/expected/).
- [ ] Demo script asserts that the web UI (ADR 0017) surfaces the
      `TeamPlan`, the verdict bundle, the per-subtask panels, the
      decision log, the merged diff, and the final review — verified
      by a `curl` probe of the SSE stream that asserts the presence
      of the per-phase audit events listed above in chronological
      order.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row added for
      `0045` and decision-graph edges drawn from `0033`, `0038`,
      `0040`, `0041`, `0042`, `0043`, `0044` to `0045` and from
      `0045` to `0022`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- The headline "ork as A2A coding-agent platform" demo finally lands
  end-to-end: one workflow template, one demo script, one terminal
  state, one merge commit. Every numbered ADR from 0033 onward has a
  visible role.
- The orchestrator is **glue**, not new infrastructure. Discovery,
  sub-worktree allocation, plan verification, peer delegation,
  diff aggregation, conflict resolution, HITL, observability — every
  load-bearing primitive comes from a separately-reviewed ADR. The
  new code is the composition policy, which is the only thing this
  ADR is well-positioned to commit.
- Decomposition itself going through plan cross-verification (ADR
  0038, same protocol, larger artifact) is the structural answer to
  *"who reviews the architect?"*. It generalises ADR 0033's
  `verify_plan_step` knob from a per-iteration toggle to a top-level
  gate that runs once per team formation.
- Sub-agent context priming is uniform: repo map (0040), team
  decisions (0043), subtask spec. Every executor walks into the same
  starting state regardless of the persona, the model profile, or
  whether it is local or remote. This is the single biggest predictor
  of whether multi-agent coordination converges or thrashes.
- Failure-mode coverage is comprehensive and *boring*: every failure
  has a documented detector, a bounded retry path, an escalation
  gate, and a terminal state. There is no orchestrator-level
  free-form "the run failed somewhere" outcome.
- Retained artifacts on rollback ("decision log + verdicts +
  transcripts under `team-{team_id}/`") make even cancelled runs
  inspectable in the web UI. Operators see *what was decided and
  why*, even when no code shipped.
- The platform-distinct value proposition is structural: a
  single-process CLI cannot run an architect on Claude Opus on tenant
  A while two executors run on local quantised models on tenant B
  while a verifier runs on hosted GPT on tenant C, with the entire
  thing audited under one team_id. ork can.
- Promoting the `reviewer.stub` and `tester.stub` to full
  implementations closes the gap ADR 0033 deliberately left open;
  the persona registry no longer carries `unimplemented!` bodies.

### Negative / costs

- The orchestrator is the most expensive surface area to test. A
  full integration test requires stub LLMs for the architect, every
  executor, every plan_verifier, and the final reviewer; a stub
  ADR-0044 aggregator wired to a stub git remote; a stub HITL gate;
  and a stub ADR-0042 discovery returning canned hits. The
  acceptance criteria above are written with that in mind, but the
  test surface is large.
- N+1 worktrees per team (one trunk + N sub-worktrees) and
  many artifacts (TeamPlan + N `SubDiffArtifact`s + merged-diff +
  per-verifier verdicts + per-subtask transcripts) scale linearly
  with team size. Disk and storage pressure is the same shape ADR
  0044 already documents; this ADR adds the architect-side artifacts
  on top.
- Cost of plan cross-verification on the team plan: N verifier LLM
  calls per decomposition round, plus up to `max_replans` retries.
  For frontier-only deployments this is the load-bearing reasoning
  step's cost; for cost-sensitive deployments the `verifiers: null`
  default routes to ADR 0042's `DiversityFromSet` over `cost_tier:
  cheap`-tagged verifiers. The wire shape supports both.
- The orchestrator is opinionated about ordering: decompose →
  plan_verify → dispatch → integrate → final_review. Workflows that
  want a different shape (e.g. dispatch *some* subtasks before
  plan_verify, to surface diagnostics for the verifier) cannot use
  this template — they have to author a custom workflow that
  composes the same ADRs differently. We accept this; one canonical
  shape is more valuable than a flag forest.
- "Final plan_verifier reject after FF merge" is structurally
  awkward: the trunk has already advanced. Opening a revert subtask
  is the conservative answer (no unilateral force-push) but it does
  mean the user sees a "shipped, needs revert" state that is *not*
  fully terminal until the revert merges. We document this loudly;
  operators who want strict pre-merge gating should configure ADR
  0044's `cross_verification` instead of the post-merge final
  plan_verifier.
- The `team_architect` persona is one more thing for ADR 0033's
  `PersonaRegistry` to carry. We reserve the *one* new persona id
  here rather than introducing a forest of team-shaped personas;
  every other persona consumed by this orchestrator is owned by ADR
  0033.
- The decomposition gate is mandatory in this ADR's default config.
  An operator who wants to skip it (e.g. for a one-off prototyping
  run) must configure `plan_verification.verifiers: []` *and*
  `plan_verification.max_replans: 0`, which auto-approves on an
  empty verdict set. We considered making the gate opt-out; we keep
  it opt-in-by-default because the headline demo's value depends on
  showing it, and because solo flows already have a separate opt-in
  via ADR 0033.

### Neutral / follow-ups

- ADR [`0026`](0026-workflow-topology-selection-from-task-features.md)
  may register `coding_team_run` as one of its outputs once this
  ADR ships — the topology classifier picks "team" vs "solo" based
  on task features, then dispatches the corresponding template. We
  do not pre-commit that mapping.
- A future ADR may add a "team retrospective" step that runs after
  the run terminates, distilling the decision log + verdict bundles
  into a single ADR-0043 `team_remember` entry tagged
  `kind: "retrospective"`, so future teams on the same repo benefit
  from the lesson. Out of scope here; deferred to a follow-up.
- A future ADR may surface team templates in the web UI (ADR
  [`0017`](0017-webui-chat-client.md)) as "preset team shapes"
  (e.g. *"Bug-fix team"*, *"Feature-implementation team"*,
  *"Refactor team"*) so an end user picks the shape rather than
  authoring YAML. Out of scope here.
- The `final_plan_verification` policy reuses ADR 0038's verifier
  shape; if a future ADR refines plan-verification semantics to
  consume *outcomes* (diff + tests + diagnostics) rather than
  *plans*, this surface is the natural migration target.
- ADR [`0036`](0036-per-step-model-assignment.md) (when it lands)
  may propagate per-subtask model assignment via `TeamPlanSubtask`,
  bypassing ADR 0042 discovery for the executor slots. The
  `AgentSelectionPolicy::Pinned` variant is shaped to absorb that
  swap.
- A future ADR may add team-of-teams composition (one orchestrator
  whose subtasks are themselves `CodingTeamRun` steps). The
  workflow-step variant supports nesting today; the test surface
  does not. Deferred until the use case appears.
- The "two executors on same task" pattern (deliberately running
  redundant attempts and keeping the better diff) is *not* this
  ADR — it is a future ADR on competitive multi-agent execution.
  The team here is *cooperative*: each subtask is dispatched once,
  diffs combine.

## Alternatives considered

- **Keep ADR 0033's solo workflow as the only first-class flow;
  treat team composition as workflow-template wiring.** Rejected:
  the orchestration policy is non-trivial (decomposition gate,
  parallel dispatch, integration handoff, final review) and lives in
  Rust, not YAML. Forcing it into a workflow template would push the
  decomposition loop, the verifier-aggregation loop, and the retry
  budgets into Tera template strings, which is exactly the failure
  mode ADRs 0028–0033 worked to remove.
- **Bundle this ADR with ADR 0044.** Rejected: ADR 0044 is the
  *integration step* and is independently useful (a hand-authored
  workflow with two delegate steps and a `team_transactional_code_change`
  finalizer ships without 0045). Coupling them locks a `TeamPlan`-shaped
  precondition into 0044, which it does not need.
- **Bundle this ADR with ADR 0033.** Rejected: 0033 is the *building
  blocks*, this ADR is the *assembly*. Coupling them locks the
  persona descriptor's design to one specific orchestrator shape,
  precludes other orchestrator shapes (competitive, hierarchical) in
  future ADRs, and forces 0033's reviewers to litigate `TeamPlan`
  semantics they do not consume.
- **Use ADR 0027 HITL as the only escalation surface.** Considered
  for failure-mode simplicity; rejected because the *bounded retry*
  paths (replan, widen, subtask-retry) recover the vast majority of
  realistic transient failures without paging a human. HITL is the
  terminal escalation, not the first-line response.
- **Make the decomposition gate optional by default.** Rejected:
  the headline demo depends on showing it, and the platform-distinct
  value proposition is "every plan is reviewable across processes
  and profiles". An operator who wants to skip can configure an
  empty verifier list with `max_replans: 0`; an opt-out flag
  encourages the pathology.
- **Final plan_verifier rejects unilaterally revert the merge.**
  Rejected: a force-push to trunk is exactly the kind of "hard to
  reverse, affects shared state" action that warrants escalation,
  not automation. Opening a revert subtask is the conservative
  answer; ADR 0044's `cross_verification` is the correct surface for
  pre-merge gating with strict semantics.
- **Replace ADR 0042 discovery with hard-coded persona ids in the
  workflow template.** Rejected: the available agent set is
  per-tenant and time-varying (ADR 0005), and capability tagging is
  the unit operators care about. Hard-coded ids work for the demo
  but defeat the platform's value proposition.
- **One generic "coordinator" persona used for both this orchestrator
  and any future team shape.** Rejected: the prompt is
  decomposition-specific (graph emission, dependency reasoning),
  the tool catalog is decomposition-specific
  (`submit_team_plan` is the structured-output exit), and the
  edit_format is `ReadOnly`. A future competitive-team coordinator
  would have a different prompt, a different exit tool, and a
  different acceptance criterion. Two personas are clearer than one
  polymorphic one.
- **Consume ADR 0038's plan shape verbatim for `TeamPlan`.**
  Considered for shape-economy; rejected because a team plan carries
  per-subtask role/language/edit_format requirements that ADR 0038's
  single-track plan does not. We pin a separate schema URL
  (`team-plan/v1`) and document that ADR 0038's verifier consumes it
  via the same `DataPart` envelope.
- **Have the orchestrator run as a dedicated process (a "team
  coordinator" service) rather than as a `LocalAgent` driven by an
  engine arm.** Rejected for v1: every other workflow step runs in
  the engine; making this one a service-out-of-band would invent a
  third-class citizen between "engine step" and "remote A2A peer".
  A future ADR may externalise it once the load justifies it.

## Affected ork modules

- New: [`crates/ork-core/src/models/team_plan.rs`](../../crates/ork-core/) —
  `TeamPlan`, `TeamPlanSubtask`, `ComplexityBand`,
  `AgentSelectionPolicy`, `RankingPolicyKind`, `validate_dag`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — new `WorkflowStep::CodingTeamRun` variant + supporting policy
  structs.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  — new `run_team_run` arm composing the dispatch / integration /
  final-review phases.
- New: [`crates/ork-agents/src/personas/team_architect.rs`](../../crates/ork-agents/) —
  the new persona descriptor and prompt template.
- [`crates/ork-agents/src/personas/reviewer.rs`](../../crates/ork-agents/) —
  promoted from `reviewer.stub`; full implementation.
- [`crates/ork-agents/src/personas/tester.rs`](../../crates/ork-agents/) —
  promoted from `tester.stub`; full implementation.
- [`crates/ork-agents/src/persona.rs`](../../crates/ork-agents/) —
  `PersonaRegistry::with_defaults` registers `team_architect`;
  stub-removal cleanup.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs)
  — register the `submit_team_plan` native tool with constrained
  decoding shape.
- New: `migrations/012_team_runs.sql` — `team_runs` and
  `team_run_subtasks` tables.
- [`workflow-templates/coding-team-orchestrator.yaml`](../../workflow-templates/)
  — new template.
- [`demo/scripts/stage-11-coding-team.sh`](../../demo/scripts/)
  — new script + matching `demo/expected/stage-11*.txt`.
- [`docs/adrs/README.md`](README.md) — ADR index row + decision-graph
  edges.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| GitHub Copilot Workspace | architect-then-execute split with a reviewable plan | `team_architect` + plan-verification gate + parallel executors |
| Devin / Cognition | autonomous task decomposition, sub-task fan-out, integration | `TeamPlan` decomposition + ADR 0042 discovery + ADR 0044 aggregation |
| OpenHands multi-agent | architect / executor / verifier split | `team_architect` + executors + plan_verifier (ADR 0033 personas) |
| Claude Code subagents | `.claude/agents/*.md` per role; project-local skills | `PersonaRegistry` + `CodingTeamRun` step composing them |
| Aider `/architect` mode | plan-then-edit toggle | ADR 0038's plan mode + this ADR's team plan as its team-level analogue |
| LangGraph multi-agent supervisor | central coordinator dispatching to worker agents | `TeamOrchestrator` engine arm dispatching via ADR 0006/0007 |
| Multi-agent debate / verifier ensembles | "ask N independent judges, aggregate verdicts" | Plan cross-verification (ADR 0038) on the `TeamPlan`, again on the merged diff (ADR 0044) |
| Solace Agent Mesh | workflow-driven agent fan-out | n/a — SAM has no team / sub-worktree / aggregation primitives; this ADR is net-new |

## Open questions

- **Plan-verifier echo when no diversity is available.** ADR 0042's
  `DiversityFromSet` ranks against the architect's profile; on a
  single-tenant deployment with one model profile available, every
  candidate verifier ties at distance zero. Stance for v1: the
  workflow may set `plan_verification.verifiers: []` to skip the
  gate (auto-approve); the orchestrator emits an
  `audit.team_run.plan_verified.skipped_no_diversity` event so the
  operator sees it. Revisit when ADR 0042's diversity scoring
  matures.
- **Decision-log slice size for sub-agent priming.** The default
  top-8 recent + top-5 recall is a guess; we will tune by dogfooding.
  The `default_memory_autoload_top_k` knob from ADR 0034 already
  carries the per-profile override.
- **Orchestrator persona vs. orchestrator engine arm.** This ADR
  ships the orchestrator as an *engine arm* (Rust code in
  `ork-core::workflow::engine`) driven by a *persona* for the
  decomposition step only. An alternative — making the orchestrator
  itself an A2A agent that delegates to executors — was considered
  and rejected for v1 as too heavy; revisit if a future ADR wants
  cross-mesh team formation.
- **Streaming the `TeamPlan`.** The decomposition gate is
  terminal-only today (the architect's loop ends on
  `submit_team_plan`). A future ADR may add an SSE stream that
  surfaces partial plans during decomposition, so the web UI shows
  "thinking" in real time. Out of scope here; the wire shape
  supports it via ADR 0008's existing SSE.
- **Team-of-teams.** A `CodingTeamRun` whose subtasks are themselves
  `CodingTeamRun` steps is mechanically supported (the
  orchestrator's dispatch loop calls `send_task` regardless of the
  agent's internal shape). Whether this composes well is an open
  empirical question; deferred until the use case appears.
- **Cancellation deadlock.** Cooperative cancellation of an
  in-flight team run requires every sub-agent to honour an A2A
  `cancel`; non-cooperative agents stall the orchestrator's
  shutdown. Stance for v1: the orchestrator's cancellation timeout
  (`engine.team_run.cancel_timeout`, default 30s) hard-aborts the
  sub-task workflow nodes after the deadline; the underlying A2A
  task is orphaned and reaped by ADR 0008's task GC. We document
  this loudly.
- **Architect's tool catalog and `code_search` body bytes.** The
  architect is `EditFormat::ReadOnly` but `code_search` returns body
  bytes. For very large repos this can blow the architect's context
  before `submit_team_plan` is called. Mitigation: ADR 0040's repo
  map is the primary navigation surface; `code_search` is a
  fallback. A future ADR may add a "structural-only" `code_search`
  variant that returns symbol-level hits without bodies.

## References

- A2A spec: <https://github.com/google/a2a>
- GitHub Copilot Workspace plan/execute split:
  <https://githubnext.com/projects/copilot-workspace/>
- OpenHands multi-agent docs:
  <https://docs.all-hands.dev/modules/usage/agents>
- LangGraph supervisor pattern:
  <https://langchain-ai.github.io/langgraph/tutorials/multi_agent/agent_supervisor/>
- Devin task decomposition (Cognition):
  <https://www.cognition.ai/blog/introducing-devin>
- Multi-agent verification (Google Research, *Towards a Science of
  Scaling Agent Systems*, April 2025):
  <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/>
- Related ADRs:
  [`0002`](0002-agent-port.md),
  [`0003`](0003-a2a-protocol-model.md),
  [`0006`](0006-peer-delegation.md),
  [`0007`](0007-remote-a2a-agent-client.md),
  [`0011`](0011-native-llm-tool-calling.md),
  [`0017`](0017-webui-chat-client.md),
  [`0018`](0018-dag-executor-enhancements.md),
  [`0020`](0020-tenant-security-and-trust.md),
  [`0022`](0022-observability.md),
  [`0025`](0025-typed-output-validation-and-verifier-agent.md),
  [`0027`](0027-human-in-the-loop.md),
  [`0028`](0028-shell-executor-and-test-runners.md),
  [`0029`](0029-workspace-file-editor.md),
  [`0030`](0030-git-operations.md),
  [`0031`](0031-transactional-code-changes.md),
  [`0032`](0032-agent-memory-and-context-compaction.md),
  [`0033`](0033-coding-agent-personas.md),
  [`0034`](0034-per-model-capability-profiles.md),
  [`0036`](0036-per-step-model-assignment.md),
  [`0037`](0037-lsp-diagnostics.md),
  [`0038`](0038-plan-mode-and-cross-verification.md),
  [`0040`](0040-repo-map.md),
  [`0041`](0041-nested-workspaces.md),
  [`0042`](0042-capability-discovery.md),
  [`0043`](0043-team-shared-memory.md),
  [`0044`](0044-multi-agent-diff-aggregation.md).
