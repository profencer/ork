# 0044 — Multi-agent transactional diff aggregation

- **Status:** Proposed
- **Date:** 2026-04-29
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0016, 0022, 0025, 0027, 0028, 0029, 0030, 0031, 0037, 0038, 0041, 0043, 0045
- **Supersedes:** —

## Context

ADR [`0031`](0031-transactional-code-changes.md) defines the
single-agent transactional code change: open a worktree, drive an
apply phase, drive a validate phase, and either fast-forward the
base branch onto the task branch or drop the worktree. The contract
is "all-or-nothing observation on the base branch" for **one**
producer.

ADR [`0041`](0041-nested-workspaces.md) generalises that worktree
into a hierarchy: a trunk worktree per run, plus N sub-worktrees,
one per sub-agent. Each sub-agent edits in isolation, and on A2A
`Completed` the coordinator captures the sub-worktree's diff
against the parent's `head_commit` as a
[`SubDiffArtifact`](../../crates/ork-core/src/ports/sub_workspace.rs)
ADR-0016 artifact. ADR 0041 explicitly stops there:

> The patch is the contract handed to ADR 0044; this ADR does not
> aggregate.

That deferral is what this ADR closes. The *team*-level operation —
combining N `SubDiffArtifact`s produced by N parallel sub-agents
under one orchestrator (ADR [`0045`]) — has none of the guarantees
that ADR 0031 gives a single agent. Concretely, three failure
modes are observable today even on a small team workflow:

1. **Textual conflicts.** Two sub-agents edited overlapping hunks
   of [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs);
   per-sub-worktree both diffs apply cleanly against the parent,
   but `git apply` of the second onto the merged tree rejects.
   ADR 0041's "no cross-worktree locking" posture surfaces this at
   merge time, by design — but no component owns "merge time"
   yet.
2. **Semantic conflicts.** Sub-A renamed a public function in
   `ork-core`; sub-B added a new caller of the old name in
   `ork-api`. Both diffs apply cleanly textually; the merged tree
   has an undefined symbol that *neither* `cargo check` against
   sub-A nor against sub-B caught. ADR
   [`0037`](0037-lsp-diagnostics.md) gives us the diagnostic
   primitive but no consumer runs it on the merged tree.
3. **Test conflicts.** Sub-A introduces a test against an internal
   trait method; sub-B refactors the trait so the method signature
   changes. Per-sub-worktree both ran `cargo test` green; on the
   merged tree the test fails. ADR
   [`0028`](0028-shell-executor-and-test-runners.md) provides
   `run_tests`, but again no aggregation step invokes it.

The naive answer — "have the orchestrator concatenate diffs and
fast-forward into trunk" — fails all three. The correct answer is
already structurally present in the codebase: ADR 0031's apply →
validate → finalize lifecycle, lifted from "one agent" to "N
agents." The integration step *is* the team's apply phase; the
merged-tree validation *is* the team's validate phase; the FF
merge into trunk *is* the team's finalize. The missing pieces are
(a) a coordinator that drives that lifecycle over a set of
sub-worktree diffs rather than one agent's edits, and (b) the
conflict-detection layers and resolution policies that single-agent
transactions never need.

ADR [`0045`] (team orchestrator, planned) will dispatch
sub-tasks; ADR [`0043`](0043-team-shared-memory.md) will carry
`team_remember` entries between iterations; ADR
[`0038`](0038-plan-mode-and-cross-verification.md) supplies the
`PlanVerdict` shape and `PlanCrossVerifier` aggregation that this
ADR reuses for optional pre-commit cross-verification of the
merged diff. None of those ADRs own the merge gate. This ADR
does.

## Decision

ork **introduces** a `TeamDiffAggregator` port in `ork-core` with
a `LocalTeamDiffAggregator` implementation in `ork-integrations`,
a new workflow step kind `TeamTransactionalCodeChange` that wraps
N child sub-tasks in a team-level apply/validate/finalize triple,
a configurable `IntegrationConflictPolicy` describing what to do
when textual / semantic / test conflicts surface on the merged
tree, and a Postgres `team_transactions` table that persists the
team-transaction lifecycle phase. The port reuses ADR 0031's FF
merge primitive on success and ADR 0027's `HumanInputGate` for
escalation, exactly as ADR 0031 reuses them for the single-agent
case.

A team transaction's contract is the team-level analogue of ADR
0031's:

> Either every sub-agent's diff has been applied to a fresh
> integration worktree, every team-level validation step
> succeeded, and the **trunk branch** has been fast-forwarded
> onto the integration tip — or the integration worktree has
> been dropped, every sub-worktree has been rolled back per ADR
> 0031, and trunk is untouched. There is no third state visible
> outside the orchestrator.

### Boundary with ADR 0031

| Concern | ADR 0031 | ADR 0044 (this) |
| ------- | -------- | --------------- |
| Producer | One agent | One orchestrator + N sub-agents |
| Worktree under apply | One trunk worktree | N sub-worktrees + one fresh integration worktree |
| Diff captured | One unified diff against `base_commit` | N `SubDiffArtifact`s + one merged unified diff against `trunk_commit` |
| Validate phase | The agent's own validate steps | The merged tree's validation: textual apply + LSP + tests + optional cross-verifier |
| Finalize on success | FF-merge task branch onto base branch | FF-merge integration branch onto trunk branch |
| Finalize on failure | Drop one worktree | Drop integration worktree + roll back all N sub-worktree transactions |

A team transaction is composed of (N + 1) ADR-0031 transactions:
one ADR-0031 transaction per sub-agent (rooted at its
sub-worktree, base = sub-branch, finalize = "promote to integration
worktree on team-level success"), plus one ADR-0031 transaction at
the team level (rooted at the integration worktree, base = trunk,
finalize = standard FF merge to trunk). This ADR does **not**
re-implement ADR 0031's lifecycle; it composes ADR 0031.

### `TeamDiffAggregator` port

```rust
// crates/ork-core/src/ports/team_diff_aggregator.rs

use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{RunId, StepId, TenantId};

use crate::ports::sub_workspace::SubDiffArtifact;          // ADR 0041
use crate::ports::transaction::{TransactionId, TransactionResolution}; // ADR 0031
use crate::ports::workspace::WorkspaceHandle;              // ADR 0029
use crate::ports::team::TeamId;                            // ADR 0043
use crate::ports::human_input::HumanInputRequestId;        // ADR 0027
use crate::ports::plan_gate::PlanVerdict;                  // ADR 0038

/// Drives the lifecycle of one `TeamTransactionalCodeChange` step.
///
/// Owns:
/// - integration-worktree provisioning (delegated to
///   `GitOperations` via a freshly branched task branch off trunk);
/// - deterministic application of N `SubDiffArtifact`s in
///   `ApplicationOrder` order;
/// - conflict detection (textual, semantic via ADR 0037, test via
///   ADR 0028) on the merged tree;
/// - dispatch of `IntegrationConflictPolicy` resolutions;
/// - optional cross-verification of the merged diff via ADR 0038;
/// - finalize: FF-merge integration onto trunk on success; drop
///   integration + cascade rollback through ADR 0031 sub-tx on
///   failure.
///
/// It does **not** drive the per-sub-agent apply (that is the
/// orchestrator's job — ADR 0045 — composing ADR 0006 delegation
/// with ADR 0041 sub-worktrees) and it does **not** mutate sub-
/// worktrees beyond requesting the rollback through their own
/// ADR-0031 coordinators.
#[async_trait]
pub trait TeamDiffAggregator: Send + Sync {
    /// Provision the integration worktree branched off
    /// `req.trunk_branch` at the recorded `req.trunk_commit`.
    /// Idempotent per `(tenant_id, run_id, step_id)`.
    async fn open(
        &self,
        req: OpenTeamTransactionRequest,
    ) -> Result<TeamTransactionHandle, OrkError>;

    /// Apply the N sub-diffs onto the integration worktree in the
    /// order the strategy resolves to, capturing every conflict.
    async fn apply(
        &self,
        tx: &TeamTransactionHandle,
        diffs: Vec<SubDiffArtifact>,
        order: ApplicationOrder,
    ) -> Result<IntegrationApplyOutcome, OrkError>;

    /// Run the full validation suite against the merged tree:
    /// LSP diagnostics (ADR 0037) and the configured test runner
    /// (ADR 0028). Returns the structured outcome; the engine
    /// hands it to `finalize` together with the conflict policy.
    async fn validate(
        &self,
        tx: &TeamTransactionHandle,
    ) -> Result<IntegrationValidationOutcome, OrkError>;

    /// Optional pre-finalize cross-verification of the merged diff.
    /// When the team policy enables it, dispatches the merged diff
    /// to N verifier peers via ADR 0038's `PlanCrossVerifier`-shaped
    /// API and aggregates verdicts under the same `AggregationPolicy`.
    /// Returns `None` when verification is disabled for this team.
    async fn cross_verify_merged(
        &self,
        tx: &TeamTransactionHandle,
        merged_diff_artifact_id: &str,
    ) -> Result<Option<MergedDiffVerdict>, OrkError>;

    /// Decide the team transaction's outcome given the apply +
    /// validate + verdict bundle and the conflict policy.
    ///
    /// Returns:
    /// - `Committed { merge_commit }` on full success and a
    ///   trunk FF;
    /// - `ReDispatching { offending_subtask_ids, retry_count }`
    ///   when the policy resolves to `re_dispatch` and the bound
    ///   has not been exhausted;
    /// - `AwaitingHuman { request_id }` when the policy resolves
    ///   to `escalate_to_human`;
    /// - `RolledBack { reason, sub_resolutions }` when the policy
    ///   resolves to `fail_team` or any retry budget is exhausted.
    async fn finalize(
        &self,
        tx: &TeamTransactionHandle,
        outcome: TeamValidationOutcome,
        policy: &IntegrationConflictPolicy,
    ) -> Result<TeamTransactionResolution, OrkError>;

    /// Resume from a HITL decision. Maps the human's
    /// `ApprovalDecision` (ADR 0027) onto either a forced commit,
    /// a forced rollback, or a re-dispatch with a human-supplied
    /// resolution note recorded as a team_remember entry (ADR
    /// 0043).
    async fn resume_from_approval(
        &self,
        tx: &TeamTransactionHandle,
        decision: HumanIntegrationDecision,
    ) -> Result<TeamTransactionResolution, OrkError>;

    /// Force-abort. Used by engine-restart sweepers and by
    /// cooperative cancellation. Cascades through the per-sub-
    /// worktree ADR-0031 coordinators with `AbortReason::Cancelled`.
    async fn abort(
        &self,
        team_transaction_id: TeamTransactionId,
        reason: TeamAbortReason,
    ) -> Result<(), OrkError>;

    /// List team-transactions that are not in a terminal phase.
    /// Engine-restart calls this per tenant to drive recovery.
    async fn list_in_flight(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<InFlightTeamTransaction>, OrkError>;
}

#[derive(Debug, Clone)]
pub struct OpenTeamTransactionRequest {
    pub tenant_id: TenantId,
    pub team_id: TeamId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub repo: String,
    pub trunk_branch: String,
    /// Tip of `trunk_branch` recorded at open. Used by `finalize`
    /// for the same compare-and-swap discipline ADR 0031 enforces
    /// on its task → base merge.
    pub trunk_commit: String,
    /// Template for the integration branch. Defaults to
    /// `ork/run-{run_id}/team-{team_id}/integration-{step_id}`.
    pub integration_branch_template: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct TeamTransactionHandle {
    pub team_transaction_id: TeamTransactionId,
    pub team_id: TeamId,
    /// The fresh integration worktree branched off trunk. Distinct
    /// from any sub-worktree allocated by ADR 0041.
    pub integration_workspace: WorkspaceHandle,
    pub trunk_branch: String,
    pub trunk_commit: String,
    pub integration_branch: String,
    pub phase: TeamTransactionPhase,
    /// References to the per-sub-agent ADR-0031 transactions whose
    /// rollback this team transaction owns.
    pub sub_transaction_ids: Vec<TransactionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTransactionId(pub uuid::Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamTransactionPhase {
    /// Integration worktree allocated; no diffs applied yet.
    Opened,
    /// Diffs being applied in the resolved order.
    Applying { order: ApplicationOrder },
    /// All diffs applied; merged diff captured as artifact.
    Applied {
        merged_diff_artifact_id: String,
        applied_subtask_ids: Vec<String>,
    },
    /// Validation in flight against the merged tree.
    Validating { merged_diff_artifact_id: String },
    /// Validation complete (pass or fail).
    Validated {
        merged_diff_artifact_id: String,
        outcome: TeamValidationOutcome,
    },
    /// Optional cross-verification in flight.
    CrossVerifying {
        merged_diff_artifact_id: String,
        verifier_count: u32,
    },
    /// Team awaiting a human decision via ADR 0027.
    AwaitingHuman {
        merged_diff_artifact_id: String,
        validation_artifact_id: String,
        verdict_artifact_ids: Vec<String>,
        request_id: HumanInputRequestId,
    },
    /// Re-dispatch in flight: one or more sub-agents are being
    /// asked to redo with peer diffs in context.
    ReDispatching {
        retry_count: u32,
        offending_subtask_ids: Vec<String>,
    },
    /// Terminal: FF merge succeeded, integration worktree dropped.
    Committed { merge_commit: String },
    /// Terminal: integration dropped without affecting trunk; sub-
    /// worktree transactions were rolled back per ADR 0031.
    Aborted {
        reason: TeamAbortReason,
        merged_diff_artifact_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplicationOrder {
    /// Apply in the order produced by the persona priority list.
    /// Default: architects (Plan / Verify) first, executors
    /// (Edit / Test / Commit) next, docs last. Persona phase comes
    /// from ADR 0033's `PersonaPhase`.
    ByRolePriority,
    /// Pre-rank diffs by simulated rejection count: try every
    /// permutation pairwise, choose the order with the fewest
    /// `git apply --check` rejections. Bounded at N ≤ 8.
    ByFewestConflicts,
    /// Caller-supplied. The orchestrator (ADR 0045) chose the
    /// order explicitly.
    Explicit { subtask_ids: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationApplyOutcome {
    pub merged_diff_artifact_id: String,
    pub applied: Vec<String>, // subtask_id
    pub textual_conflicts: Vec<TextualConflict>,
    pub files_changed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextualConflict {
    pub subtask_id: String,
    pub path: String,
    /// `git apply --reject` output, or the 3-way merge marker
    /// region. Persisted full to the artifact; truncated here.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationValidationOutcome {
    /// LSP diagnostics that exist on the merged tree but are not
    /// attributable to any single sub-diff (i.e. cross-merge
    /// emergent diagnostics — broken imports, undefined symbols,
    /// type errors).
    pub semantic_conflicts: Vec<SemanticConflict>,
    /// Tests that pass per-sub-worktree but fail on the merged
    /// tree, identified by re-running per-sub-worktree fixture
    /// + the merged-tree run and diffing the failing-test set.
    pub test_conflicts: Vec<TestConflict>,
    /// All test failures on the merged tree, regardless of
    /// whether per-sub-worktree they passed. Useful when the
    /// re-run discipline is not enabled.
    pub merged_tree_test_summary: Option<TestSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticConflict {
    pub path: String,
    pub line: u32,
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    /// Sub-tasks whose diffs touched this path. Empty if the
    /// emergent diagnostic lives in a file no sub-diff touched
    /// (e.g. an import in a downstream crate).
    pub touched_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConflict {
    pub test_id: String,
    /// Sub-tasks whose per-sub-worktree run had this test green.
    pub green_in: Vec<String>,
    /// Failure message from the merged-tree run (truncated; full
    /// log lives in the validation artifact).
    pub merged_failure: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamValidationOutcome {
    Pass {
        merged_diff_artifact_id: String,
    },
    Fail {
        merged_diff_artifact_id: String,
        textual: Vec<TextualConflict>,
        semantic: Vec<SemanticConflict>,
        test: Vec<TestConflict>,
        validation_artifact_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedDiffVerdict {
    pub verdicts: Vec<PlanVerdict>,
    pub aggregated: PlanGateDecisionLite,
    pub artifact_ids: Vec<String>, // per-verifier verdict artifacts
}

/// Mirror of ADR 0038's `PlanGateDecision` minus the inline
/// verdicts (already carried separately to keep this struct flat).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanGateDecisionLite { Approved, RequestChanges, Rejected }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamTransactionResolution {
    Committed {
        merge_commit: String,
        sub_resolutions: Vec<(String /* subtask_id */, TransactionResolution)>,
    },
    ReDispatching {
        offending_subtask_ids: Vec<String>,
        retry_count: u32,
    },
    AwaitingHuman {
        request_id: HumanInputRequestId,
    },
    RolledBack {
        reason: TeamAbortReason,
        sub_resolutions: Vec<(String, TransactionResolution)>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamAbortReason {
    TextualConflict,
    SemanticConflict,
    TestConflict,
    CrossVerifierRejected,
    HumanRejected,
    RetriesExhausted,
    EngineRestart,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HumanIntegrationDecision {
    /// Force the FF merge despite open conflicts; recorded in
    /// `audit.team_transaction.committed_after_human_override`.
    ForceCommit,
    /// Drop integration and roll back all sub-worktree
    /// transactions.
    ForceRollback,
    /// Record the human's note as a `team_remember` entry (ADR
    /// 0043) tagged `team_remember.kind = "integration_resolution"`,
    /// then re-dispatch the named sub-tasks.
    ReDispatch {
        offending_subtask_ids: Vec<String>,
        note: String,
    },
}

#[derive(Debug, Clone)]
pub struct InFlightTeamTransaction {
    pub team_transaction_id: TeamTransactionId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub team_id: TeamId,
    pub integration_workspace: WorkspaceHandle,
    pub phase: TeamTransactionPhase,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}
```

### `TeamTransactionalCodeChange` workflow step

[`WorkflowStep`](../../crates/ork-core/src/models/workflow.rs)
gains a new variant alongside ADR 0031's
`TransactionalCodeChange`:

```rust
WorkflowStep::TeamTransactionalCodeChange {
    id: String,
    /// The team this step is bound to. Sub-task dispatch (ADR
    /// 0045) reads the same `team_id`; sub-worktree branch names
    /// (ADR 0041) inherit it.
    team_id: TeamId,
    repo: String,
    trunk_branch: String,
    integration_branch_template: Option<String>,
    /// Sub-tasks. Each entry compiles to a child `Agent` step
    /// scoped to a sub-worktree allocated by ADR 0041, executed
    /// inside an ADR-0031 transaction whose `on_failure` is
    /// pinned to `Rollback` (the team transaction owns escalation,
    /// not the sub).
    subtasks: Vec<TeamSubtaskSpec>,
    /// Order strategy for applying the captured `SubDiffArtifact`s
    /// onto the integration worktree. Defaults to
    /// `ApplicationOrder::ByRolePriority`.
    application_order: ApplicationOrder,
    conflict_policy: IntegrationConflictPolicy,
    cross_verification: Option<MergedDiffVerificationPolicy>,
    timeout: Option<Duration>,
    depends_on: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamSubtaskSpec {
    pub subtask_id: String,
    pub agent: AgentRef,           // ADR 0007
    pub persona_phase: PersonaPhase, // ADR 0033 — used by ByRolePriority
    pub prompt_template: String,
    pub tools: Vec<String>,
    /// Visibility into peer sub-task diffs on re-dispatch. When
    /// true, ADR 0043 `team_remember` entries written by this
    /// aggregator on conflict (kind = `peer_diff`) are surfaced
    /// to the agent's `team_recall` on the retry.
    pub include_peer_diffs_on_retry: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntegrationConflictPolicy {
    /// Re-dispatch the offending sub-agents up to `max_retries`
    /// times. Peer diffs are surfaced as `team_remember` entries
    /// (kind = `peer_diff`) before the retry. Once `max_retries`
    /// is exhausted, transitions to the policy's
    /// `escalation`.
    ReDispatch {
        max_retries: u32,
        offender_selection: OffenderSelection,
        escalation: Box<IntegrationConflictPolicy>,
    },
    /// Pause the run; surface the diffs + findings + suggested
    /// re-dispatch list via ADR 0027. Approval forces a commit,
    /// rejection rolls back, re-dispatch routes back through
    /// `ReDispatch`.
    EscalateToHuman {
        required_scopes: Vec<String>,
        prompt: String,
        approval_timeout: Option<Duration>,
        on_approval_timeout: ApprovalTimeoutPolicy, // reuse ADR 0031
    },
    /// Drop integration; cascade rollback to every sub-worktree
    /// transaction; mark the team step `Failed`.
    FailTeam,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OffenderSelection {
    /// Re-dispatch every sub-agent whose diff touched a conflict
    /// (textual, semantic, or test). Default.
    AllTouched,
    /// Re-dispatch only the sub-agent whose diff was applied last
    /// (and therefore whose hunk caused the rejection).
    LastWriter,
    /// For semantic conflicts on a path that no sub-diff touched
    /// (e.g. broken imports in a downstream crate), re-dispatch
    /// the architect/planner instead of an executor.
    ArchitectIfEmergent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergedDiffVerificationPolicy {
    /// Reuses ADR 0038's verifier targets verbatim. The merged
    /// diff is presented to verifiers as a `DataPart` with the
    /// merged `PlanFinding`-style summary plus a `FilePart`
    /// pointing at the full unified diff artifact.
    pub verifiers: Vec<PlanVerifierTarget>,
    pub aggregation: AggregationPolicy,        // ADR 0038
    pub timeout_per_verifier: Duration,
    pub on_timeout: TimeoutPolicy,             // ADR 0038
    pub on_invalid_verdict: InvalidVerdictPolicy, // ADR 0038
}
```

YAML shape:

```yaml
- id: integrate-feature-x
  kind: team_transactional_code_change
  team_id: feature-x-team
  repo: ork
  trunk_branch: main
  application_order:
    kind: by_role_priority
  subtasks:
    - subtask_id: arch
      agent: { id: ork.agent.persona.architect.opus }
      persona_phase: plan
      prompt_template: "Decompose the change. Edit ADR docs only."
      tools: [read_file, write_file, get_repo_map]
      include_peer_diffs_on_retry: true
    - subtask_id: core-impl
      agent: { id: ork.agent.persona.executor.sonnet }
      persona_phase: edit
      prompt_template: "Implement Decision §3 of the architect's plan."
      tools: [read_file, write_file, run_tests, get_diagnostics]
      include_peer_diffs_on_retry: true
    - subtask_id: api-impl
      agent: { id: ork.agent.persona.executor.sonnet }
      persona_phase: edit
      prompt_template: "Implement Decision §4 of the architect's plan."
      tools: [read_file, write_file, run_tests, get_diagnostics]
      include_peer_diffs_on_retry: true
  conflict_policy:
    kind: re_dispatch
    max_retries: 2
    offender_selection: all_touched
    escalation:
      kind: escalate_to_human
      required_scopes: ["workflow:approve:team_integration"]
      prompt: |
        Two team retries failed to produce a clean merge. Inspect
        diffs, semantic conflicts, and test conflicts; approve to
        force-commit, reject to roll back, or specify subtasks to
        retry with a guidance note.
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
  timeout: 90m
```

### Aggregation pipeline

The engine arm `WorkflowEngine::run_team_transaction` walks:

1. **Open.** `TeamDiffAggregator::open` allocates the integration
   worktree at `trunk_commit` recorded *before* sub-tasks
   dispatch. The sub-tasks' base-commit invariant (ADR 0031's
   compare-and-swap on FF) is now relative to this trunk_commit,
   not to `main` directly.
2. **Dispatch sub-tasks.** The orchestrator (ADR 0045) opens N
   ADR-0031 sub-transactions, each rooted at a sub-worktree
   allocated by ADR 0041. Their `on_failure` is pinned to
   `Rollback`: a single sub-failure does not escalate to HITL — the
   team-level policy owns escalation.
3. **Collect `SubDiffArtifact`s.** When all sub-transactions
   finalize successfully (each rolling back its own sub-worktree
   per the ADR-0041 capture-then-drop discipline) the engine
   passes the resulting `SubDiffArtifact`s to
   `TeamDiffAggregator::apply`. Sub-transactions whose validate
   phase failed do not reach this step; the team transaction
   short-circuits with `RolledBack { reason: <propagated>}`.
4. **Apply.** Diffs are applied to the integration worktree in
   the resolved order. The aggregator first runs
   `git apply --check` per diff; on a rejection, it falls back to
   3-way merge via `git apply --3way`. Any unresolved hunk is
   recorded as a `TextualConflict` and flagged for the policy.
5. **Capture merged diff.** After the apply phase the aggregator
   computes `git diff <trunk_commit>..<integration_HEAD>` and
   persists it as an ADR-0016 artifact named
   `team-tx-{team_transaction_id}.merged.diff` under scope
   `(tenant_id, run_id, "team_transactions")`. This is the
   artifact that downstream review surfaces (ADR 0017 Web UI, ADR
   0027 HITL pane) consume.
6. **Validate.** `TeamDiffAggregator::validate` runs:
    - **LSP diagnostics** (ADR 0037) over every file touched plus
      every file `import`-reachable from a touched file (the
      reachability set comes from ADR 0040's repo map). Any
      `Error`-severity diagnostic that did *not* appear on the
      pre-apply tree is a `SemanticConflict`. (Pre-apply
      diagnostics are read from the trunk repo map.)
    - **Test runner** (ADR 0028) with the workflow-supplied
      command (defaulting to the project's
      `default_test_command` from `config/default.toml`). The
      aggregator additionally re-runs the per-sub-worktree
      `TestSummary`s from each sub-transaction's audit log; tests
      that were green per-sub but red on the merged tree are
      `TestConflict`s.
7. **Cross-verify (optional).** If
   `cross_verification: Some(_)`, the merged-diff artifact is
   handed to `cross_verify_merged`, which dispatches verifier
   peers reusing ADR 0038's `PlanCrossVerifier` aggregation
   semantics (`Unanimous` / `Majority` / `FirstDeny` / `Weighted`).
   The verdict shape and `submit_plan_verdict` tool are taken
   verbatim from ADR 0038 — the ADR 0044 dispatch carries the
   merged diff plus the per-sub `PlanFinding` history as
   context, so verifiers reason about the *team's* decision the
   same way they reason about a single planner's plan.
8. **Finalize.** `TeamDiffAggregator::finalize` consults the
   bundle `(IntegrationApplyOutcome, IntegrationValidationOutcome,
   Option<MergedDiffVerdict>)`:
    - **All clean** → reuse ADR 0031's FF-merge primitive
      (compare-and-swap `git update-ref refs/heads/<trunk_branch>
      <integration_tip> <trunk_commit>`); on success record
      `Committed { merge_commit }` and emit
      `audit.team_transaction.committed`. The integration
      worktree is dropped; sub-worktrees were already dropped by
      their own ADR-0031 finalize.
    - **Conflict surfaced** → consult `IntegrationConflictPolicy`:
       - `ReDispatch` (retries < max_retries): write a
         `team_remember` entry (ADR 0043) per offending
         subtask_id with `kind = "peer_diff"` carrying the
         sibling diffs and the conflict findings; transition to
         `ReDispatching`, return the offender list to the
         orchestrator (ADR 0045). The orchestrator re-opens N′
         sub-transactions, the team transaction re-enters from
         step 3 with the new diffs.
       - `EscalateToHuman`: open an ADR-0027 `HumanInputGate`
         request with the merged diff, validation artifact, and
         verdicts; transition to `AwaitingHuman`. Resume on
         `HumanIntegrationDecision`.
       - `FailTeam`: cascade `abort(Cancelled)` through every
         sub-transaction's ADR-0031 coordinator; drop the
         integration worktree; transition to `Aborted`.
9. **Drop the integration worktree** unconditionally on terminal
   transition (Committed or Aborted). The trunk branch advance
   is the only persistent effect of a successful team
   transaction.

### Conflict detection layers — formal definition

For a merged tree `M`, trunk tree `T`, and per-sub trees
`S_1, …, S_N`:

| Layer | Definition | Source |
| ----- | ---------- | ------ |
| Textual conflict | `git apply --3way diff_i` produces a reject hunk against the in-progress integration tree. | `git apply` exit + reject log |
| Semantic conflict | LSP diagnostic of severity `Error` on `M` whose `(path, code, message)` is **not** present in the diagnostic set of any of `T, S_1, …, S_N`. | ADR 0037 `get_diagnostics` over the file's import-closure |
| Test conflict | Test id `t` such that some `S_i` had `t ∈ green(S_i)` and `t ∈ failed(M)`. | ADR 0028 `TestSummary` re-run per sub + merged |

The "neither S_i nor T introduced this" property is what makes
the conflict *emergent* — semantically, exactly the case ADR
0031's single-agent transaction cannot detect, because there is
no merge step in the single-agent path.

### Default `ApplicationOrder` — `ByRolePriority`

The default order draws from ADR
[`0033`](0033-coding-agent-personas.md)'s `PersonaPhase`:

```
PersonaPhase::Plan, ::Verify          ─►  bucket 1 (architects)
PersonaPhase::Edit, ::Test, ::Commit  ─►  bucket 2 (executors)
PersonaPhase::Review                   ─►  bucket 3 (reviewers)
PersonaPhase::Explore, ::AnyPhase     ─►  bucket 4 (catch-all)
```

Within a bucket, ties are broken by `subtask_id` (lex order) for
determinism. The intuition is the same as the
write-an-ADR-then-implement-it loop the rest of the codebase uses:
plan-shaped diffs (which are mostly docs and skeletons) land
first; executor-shaped diffs (which carry the actual edits) land
on top of the architect's skeleton; review-shaped diffs (which
mostly add tests or annotations) land last.

`ByFewestConflicts` is opt-in for teams that empirically suffer
under ByRolePriority — it is `O(N!)` in worst case but bounded at
`N ≤ 8` by the engine; teams with more sub-tasks are nudged to
`Explicit` (orchestrator-supplied).

### Re-dispatch carries peer diffs as `team_remember`

When `IntegrationConflictPolicy::ReDispatch` fires, the aggregator
writes one `team_remember` entry per offending subtask_id, scoped
to `(tenant_id, team_id)` per ADR 0043. The entry's body carries:

- the offending subtask's previous unified diff (as a
  `DataPart`-shaped JSON object, full bytes via ADR 0016 spillover),
- every sibling subtask's unified diff,
- the conflict findings (textual / semantic / test) attributed to
  the offending subtask,
- a system-authored note: *"Your previous attempt conflicted with
  peer diffs at [paths]. Please redo your task taking peer diffs
  into account."*

The kind is `peer_diff`; the topic is
`team_remember.topic = "integration_conflict"`. ADR 0043 already
namespaces this; this ADR pins the content shape.

On retry the orchestrator (ADR 0045) re-opens the sub-transaction
with `include_peer_diffs_on_retry = true`, which surfaces those
entries via the agent's automatic `team_recall` on first turn.
`team_remember` entries written by this aggregator are tagged
`source = "team_diff_aggregator"` so that ADR 0043's retention
policy can age them out independently of human-authored entries.

### Cross-verification reuses ADR 0038, deliberately

The merged-diff cross-verification gate is a *re-pointing* of ADR
0038's verifier infrastructure, not a fork:

- The verifier targets, the timeouts, the aggregation policies,
  and the constrained-decoded `PlanVerdict` JSON Schema are the
  same. We do not introduce a `MergedDiffVerdict` schema; the
  aggregator wraps `Vec<PlanVerdict>` plus the aggregation result.
- The dispatch path is the same A2A peer delegation (ADR 0006 +
  ADR 0007). What differs is the `DataPart` payload: instead of a
  proposed plan, the verifier receives the merged diff plus the
  per-sub `PlanFinding` history (when ADR 0038 plan-mode was used
  upstream) plus the `IntegrationApplyOutcome` /
  `IntegrationValidationOutcome` summary.
- ADR 0038's `submit_plan_verdict` tool accepts the same
  arguments. The `decision` enum (`approve` / `request_changes` /
  `reject`) maps to the team finalize policy:
  `approve` → proceed to FF; `request_changes` → escalate per
  `IntegrationConflictPolicy`; `reject` →
  `TeamAbortReason::CrossVerifierRejected`, drop and roll back.

The deliberate reuse keeps the wire shape stable across ADR 0038
(plan-time) and ADR 0044 (merge-time) verifiers; the same agent
can serve both roles.

### Atomicity invariant

Trunk advances iff every gate passed. Concretely:

- `git update-ref refs/heads/<trunk_branch> <integration_tip>
  <trunk_commit>` is a compare-and-swap; if trunk moved under
  the team transaction (a concurrent commit landed), the FF
  refuses and the team transaction transitions to
  `Aborted { TimedOut }` — the same posture ADR 0031 takes for
  single-agent CAS failure. Neither merge commits nor `--force`
  is permitted.
- The integration worktree is dropped on every terminal path.
  Sub-worktrees were dropped by their own ADR-0031 transactions
  before the diffs reached the aggregator; on `FailTeam` /
  `RetriesExhausted` / `HumanRejected` the cascade is a no-op
  (sub-transactions are already terminal).
- `ReDispatching` is **not** terminal. Sub-worktrees for the
  re-dispatch are freshly allocated by ADR 0041 with new
  subtask_ids (suffixed `-r<retry_count>`); the previous
  attempt's ADR-0016 artifacts are retained for audit.

### Persistence: `team_transactions`

```sql
-- migrations/011_team_transactions.sql

CREATE TABLE team_transactions (
    team_transaction_id     UUID PRIMARY KEY,
    tenant_id               TEXT NOT NULL,
    team_id                 TEXT NOT NULL,
    run_id                  UUID NOT NULL,
    step_id                 TEXT NOT NULL,
    repo                    TEXT NOT NULL,
    trunk_branch            TEXT NOT NULL,
    trunk_commit            TEXT NOT NULL,
    integration_branch      TEXT NOT NULL,
    integration_workspace_root TEXT NOT NULL,
    phase                   JSONB NOT NULL,
    conflict_policy         JSONB NOT NULL,
    cross_verification      JSONB,
    merged_diff_artifact_id TEXT,
    validation_artifact_id  TEXT,
    verdict_artifact_ids    JSONB NOT NULL DEFAULT '[]'::jsonb,
    human_input_request_id  UUID,
    merge_commit            TEXT,
    abort_reason            TEXT,
    retry_count             INT NOT NULL DEFAULT 0,
    opened_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_phase_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at               TIMESTAMPTZ,
    UNIQUE (tenant_id, run_id, step_id)
);

CREATE INDEX team_transactions_in_flight_idx
    ON team_transactions (tenant_id, last_phase_at)
    WHERE closed_at IS NULL;

CREATE TABLE team_transaction_subtasks (
    team_transaction_id     UUID NOT NULL
        REFERENCES team_transactions (team_transaction_id) ON DELETE CASCADE,
    subtask_id              TEXT NOT NULL,
    transaction_id          UUID NOT NULL,
    sub_diff_artifact_id    TEXT,
    retry_index             INT NOT NULL DEFAULT 0,
    status                  TEXT NOT NULL,
    PRIMARY KEY (team_transaction_id, subtask_id, retry_index)
);
```

`step_results` (ADR 0018) gains an optional
`team_transaction_id UUID` column joining to `team_transactions`,
mirroring the `transaction_id` column ADR 0031 added.

### Resume / crash-recovery semantics

On engine startup, `TeamDiffAggregator::list_in_flight` is called
per tenant. The action depends on `phase`:

| Persisted phase | Action on restart |
| --------------- | ----------------- |
| `Opened` / `Applying` / `Applied` / `Validating` / `CrossVerifying` | `abort(EngineRestart)`. The merge has not been committed; drop the integration worktree; cascade `abort(Cancelled)` through every sub-transaction whose own row says `Validated{Pass}` but not yet `Committed`. The parent step is re-tried per the existing run-recovery logic. |
| `Validated { Pass }` (no cross-verification configured) | Re-attempt `finalize`. The FF merge is idempotent: if trunk already advanced to `integration_tip`, observe `merge_commit` matches. |
| `Validated { Fail }` / `CrossVerifying` returning a non-Approved bundle | Re-dispatch `IntegrationConflictPolicy`. For `EscalateToHuman` re-open the HITL request only if no `human_input_request_id` is recorded. |
| `AwaitingHuman` | No action; existing HITL resume path. |
| `ReDispatching` | Re-emit the orchestrator instruction to spawn the recorded `offending_subtask_ids` as a new retry round; the team transaction stays alive. The engine treats this exactly like a `Loop` mid-iteration restart. |
| `Committed` / `Aborted` | Terminal; nothing to do. |

The "abort on restart" cases mirror ADR 0031: re-running an
apply pass that may have invoked sub-agents via A2A is not safe
without an idempotency contract on the sub-agent side, which is
out of scope here.

### Observability

ADR [`0022`](0022-observability.md) gains team-aware events on
the existing audit/tracing surfaces:

| Event | Emitted when | Carries |
| ----- | ------------ | ------- |
| `audit.team_transaction.opened` | `open` succeeds | `team_transaction_id`, `team_id`, `repo`, `trunk_branch`, `trunk_commit`, `integration_branch`, sub_count |
| `audit.team_transaction.diff_collected` | `SubDiffArtifact` received from each sub | `team_transaction_id`, `subtask_id`, `sub_diff_artifact_id` |
| `audit.team_transaction.applied` | `Applied` phase recorded | `team_transaction_id`, `merged_diff_artifact_id`, files-changed count, textual_conflict_count |
| `audit.team_transaction.validated` | `Validated` recorded | `team_transaction_id`, semantic_conflict_count, test_conflict_count, outcome |
| `audit.team_transaction.cross_verified` | `cross_verify_merged` returns a verdict bundle | `team_transaction_id`, verdicts, aggregated decision |
| `audit.team_transaction.re_dispatched` | `ReDispatching` recorded | `team_transaction_id`, `retry_count`, `offending_subtask_ids` |
| `audit.team_transaction.escalated` | `AwaitingHuman` recorded | `team_transaction_id`, `request_id`, `required_scopes` |
| `audit.team_transaction.committed` | FF merge succeeded | `team_transaction_id`, `merge_commit`, whether human-overridden |
| `audit.team_transaction.aborted` | `Aborted` recorded | `team_transaction_id`, `reason`, sub_resolutions |

OTLP spans nest: `team_transaction:<id>` is the parent of every
`apply:*`, `validate:*`, `cross_verify:*`, and per-sub
`transaction:<sub_id>` span (the sub-tx span is itself rooted at
ADR 0031's existing span schema, now reparented under the team
span). The merged-diff capture and FF-merge sub-operations get
their own spans for latency visibility.

### Configuration

```toml
# config/default.toml
[engine.team_transaction]
default_timeout_seconds       = 5400          # 90 min
default_max_retries           = 2
diff_max_bytes                = 1048576       # 1 MiB inline; rest spills
retain_rejected_branches      = false
retention_days                = 30
default_application_order     = "by_role_priority"
default_test_command          = "cargo test --workspace"
by_fewest_conflicts_max_subtasks = 8
```

## Acceptance criteria

- [ ] Step-kind enum variant `WorkflowStep::TeamTransactionalCodeChange`
      defined in
      [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
      with the fields shown in `Decision`, alongside
      `TeamSubtaskSpec`, `IntegrationConflictPolicy`,
      `OffenderSelection`, `MergedDiffVerificationPolicy`,
      `ApplicationOrder`.
- [ ] YAML round-trip parses the example under `Decision` into the
      enum variant, verified by
      `crates/ork-core/tests/workflow_kinds/team_transaction_yaml.rs::round_trip_re_dispatch`
      and `::round_trip_cross_verification`.
- [ ] Trait `TeamDiffAggregator` defined at
      `crates/ork-core/src/ports/team_diff_aggregator.rs` with the
      signature shown in `Decision`, re-exported from
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs).
- [ ] Supporting types `OpenTeamTransactionRequest`,
      `TeamTransactionHandle`, `TeamTransactionId`,
      `TeamTransactionPhase`, `IntegrationApplyOutcome`,
      `TextualConflict`, `IntegrationValidationOutcome`,
      `SemanticConflict`, `TestConflict`,
      `TeamValidationOutcome`, `MergedDiffVerdict`,
      `PlanGateDecisionLite`, `TeamTransactionResolution`,
      `TeamAbortReason`, `HumanIntegrationDecision`,
      `InFlightTeamTransaction` defined in the same module.
- [ ] `LocalTeamDiffAggregator` defined at
      `crates/ork-integrations/src/team_diff_aggregator.rs`,
      constructed from an `Arc<dyn GitOperations>`,
      `Arc<dyn ShellExecutor>`, `Arc<dyn ArtifactStore>`,
      `Arc<dyn TransactionCoordinator>` (per-sub ADR-0031
      coordinator), `Arc<dyn HumanInputGate>`,
      `Arc<dyn TeamMemory>` (ADR 0043),
      `Arc<dyn LspDiagnostics>` (ADR 0037),
      `Arc<dyn PlanCrossVerifier>` (ADR 0038, optional — `None`
      => panic at step-time when `cross_verification` is set),
      `Arc<dyn TeamTransactionRepo>` (Postgres-backed).
- [ ] Migration `migrations/011_team_transactions.sql` creates the
      `team_transactions` and `team_transaction_subtasks` tables
      with the schema shown in `Persistence`, including the
      `UNIQUE (tenant_id, run_id, step_id)` constraint and the
      partial index.
- [ ] `LocalTeamDiffAggregator::open` is idempotent for a repeated
      `(tenant_id, run_id, step_id)` tuple, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::open_is_idempotent`.
- [ ] `LocalTeamDiffAggregator::apply` applies N diffs in
      `ApplicationOrder::ByRolePriority` order (architects then
      executors then reviewers) and records
      `IntegrationApplyOutcome.applied` matching that order,
      verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::apply_orders_by_role_priority`.
- [ ] `LocalTeamDiffAggregator::apply` records a `TextualConflict`
      when two sub-diffs touch overlapping hunks of the same
      file, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::apply_records_textual_conflict`.
- [ ] `LocalTeamDiffAggregator::apply` writes the merged unified
      diff to `ArtifactStore` under name
      `team-tx-{team_transaction_id}.merged.diff`, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::apply_persists_merged_diff_artifact`.
- [ ] `LocalTeamDiffAggregator::validate` flags an
      `Error`-severity LSP diagnostic on the merged tree that is
      absent from every sub-tree as a `SemanticConflict`,
      verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::validate_flags_emergent_semantic_conflict`.
- [ ] `LocalTeamDiffAggregator::validate` flags a test that was
      green per-sub but red on the merged tree as a
      `TestConflict`, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::validate_flags_emergent_test_conflict`.
- [ ] `LocalTeamDiffAggregator::finalize` with policy
      `IntegrationConflictPolicy::ReDispatch` and a textual
      conflict writes one `team_remember` entry per offending
      subtask_id with `kind = "peer_diff"` and topic
      `"integration_conflict"`, then returns
      `TeamTransactionResolution::ReDispatching`, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::re_dispatch_writes_peer_diff_team_memory`.
- [ ] `LocalTeamDiffAggregator::finalize` with retries exhausted
      and escalation `EscalateToHuman` opens an ADR-0027
      `HumanInputGate` request whose payload contains the merged
      diff, validation artifact, and verdict artifact ids, and
      returns `AwaitingHuman`, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::escalation_opens_hitl_with_diffs`.
- [ ] `LocalTeamDiffAggregator::finalize` with policy
      `FailTeam` cascades `abort(Cancelled)` through every sub-
      transaction's `TransactionCoordinator` and drops the
      integration worktree, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::fail_team_cascades_rollback`.
- [ ] `LocalTeamDiffAggregator::finalize` on full success runs
      the FF merge via `git update-ref` with the recorded
      `trunk_commit` as expected old value and records
      `Committed`, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::finalize_ff_merges_on_clean_outcome`.
- [ ] `LocalTeamDiffAggregator::finalize` rolls back when trunk
      advanced under the team transaction (CAS failure on
      `update-ref`), verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::finalize_refuses_when_trunk_moved`.
- [ ] `LocalTeamDiffAggregator::cross_verify_merged` dispatches
      to the configured `PlanCrossVerifier` and returns
      `MergedDiffVerdict` whose `aggregated` matches the
      configured `AggregationPolicy` (Unanimous / Majority /
      FirstDeny / Weighted), verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::cross_verify_merged_aggregates_per_policy`.
- [ ] `LocalTeamDiffAggregator::resume_from_approval` with
      `HumanIntegrationDecision::ForceCommit` runs the FF merge
      and records `Committed` with audit event
      `audit.team_transaction.committed_after_human_override`,
      verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::resume_force_commit`.
- [ ] `LocalTeamDiffAggregator::resume_from_approval` with
      `HumanIntegrationDecision::ReDispatch` writes a synthetic
      `team_remember` entry carrying the human's note (kind
      `integration_resolution`) and returns `ReDispatching`,
      verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::resume_re_dispatch_writes_team_memory`.
- [ ] `LocalTeamDiffAggregator::list_in_flight` returns rows
      where `closed_at IS NULL`, verified by
      `crates/ork-integrations/tests/team_diff_aggregator.rs::list_in_flight_filters_closed`.
- [ ] Engine arm `WorkflowEngine::run_team_transaction` walks the
      pipeline (open → dispatch → apply → validate → cross_verify
      → finalize) and writes the resolution into the parent
      `StepResult`, verified by integration test
      `crates/ork-core/tests/team_transaction_smoke.rs::happy_path_commits`.
- [ ] Integration test
      `crates/ork-core/tests/team_transaction_smoke.rs::semantic_conflict_redispatches`
      asserts: two sub-agents produce diffs that pass per-sub LSP
      checks but produce an emergent `Error` diagnostic on the
      merged tree; team transaction enters `ReDispatching` with
      both subtask_ids; second pass resolves; team transaction
      commits.
- [ ] Integration test
      `crates/ork-core/tests/team_transaction_smoke.rs::escalation_pause_resume_commits`
      asserts: retries exhaust, run enters `InputRequired`,
      `HumanInputGate::resolve(ForceCommit, ...)` resumes the
      run, trunk advances.
- [ ] Integration test
      `crates/ork-core/tests/team_transaction_smoke.rs::fail_team_rolls_back_subs`
      asserts: a textual conflict under
      `IntegrationConflictPolicy::FailTeam` cascades through
      every sub-transaction's coordinator and trunk is unchanged.
- [ ] Crash-recovery test
      `crates/ork-core/tests/team_transaction_resume.rs::aborts_validating_on_restart`
      seeds a `Validating`-phase row, restarts the engine,
      asserts the row transitions to `Aborted { EngineRestart }`
      and the integration worktree is cleaned up.
- [ ] Crash-recovery test
      `crates/ork-core/tests/team_transaction_resume.rs::reattempts_validated_pass_finalize`
      seeds a `Validated { Pass }`-phase row, restarts the
      engine, asserts the FF merge runs and the row transitions
      to `Committed`.
- [ ] Audit events `audit.team_transaction.{opened,
      diff_collected, applied, validated, cross_verified,
      re_dispatched, escalated, committed, aborted}` are emitted
      at the transitions documented in `Observability`, verified
      by `crates/ork-core/tests/team_transaction_observability.rs`.
- [ ] OTLP spans nest correctly: per-sub
      `transaction:<sub_id>` spans are parented under the
      `team_transaction:<id>` span, verified by
      `crates/ork-core/tests/team_transaction_observability.rs::span_hierarchy`.
- [ ] `cargo test -p ork-core workflow::team_transaction::` is green.
- [ ] `cargo test -p ork-integrations team_diff_aggregator::` is green.
- [ ] Public API documented in
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
      and re-exported from
      [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs).
- [ ] [`README.md`](README.md) ADR index row added for `0044` and
      decision-graph edges drawn from `0031`, `0037`, `0038`,
      `0041`, `0043` to `0044` and from `0044` to `0022`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Team-shaped agent topologies (ADR 0045) finally get the same
  *correct-by-construction* atomicity that ADR 0031 gives a
  single agent. The trunk branch is either at `trunk_commit` or
  at the merge commit, never in between, regardless of how many
  sub-agents contributed.
- Three categories of conflict that are *invisible* to the
  single-agent path (textual on the merged tree, emergent
  semantic, emergent test) become first-class, structurally
  detected, and structurally resolvable. Workflow authors no
  longer write prompt-resident merge advice.
- Re-dispatch with peer diffs as `team_remember` context is the
  cheapest available form of multi-agent coordination feedback —
  it does not require the orchestrator to re-prompt sub-agents
  or rebuild context, just to surface the missing data via ADR
  0043's existing recall path.
- Composition over re-implementation: ADR 0031's FF-merge
  primitive, ADR 0027's HITL gate, ADR 0028's test runner, ADR
  0037's diagnostics, ADR 0038's verifier infrastructure, ADR
  0041's sub-worktrees, ADR 0043's team memory, and ADR 0016's
  artifact spill all flow through this ADR. The new code is the
  glue, not new infrastructure.
- Crash-recovery is durable and conservative: a half-applied
  integration tree is unobservable to downstream consumers, the
  same way a half-applied single-agent transaction is.
- Cross-verification of merged diffs reuses ADR 0038's
  `submit_plan_verdict` shape verbatim, so the same
  `plan_verifier` peers can serve plan-time and merge-time
  reviews without learning a second wire shape.

### Negative / costs

- A team transaction is fundamentally `O(N)` in worktrees and
  artifacts: N sub-worktrees + 1 integration worktree + N
  sub-diff artifacts + 1 merged-diff artifact + (≤ V) verdict
  artifacts per cross-verification round. Disk and storage
  pressure scale linearly with team size; ADR 0019's scheduler
  must own GC, the same way it does for ADR 0031 transactions.
- `ApplicationOrder::ByFewestConflicts` is `O(N!)` in worst case
  even with the `N ≤ 8` cap. We commit to nudging larger teams
  to `Explicit`. This is a real ergonomic cost: a team of 12
  with two persona buckets has to either accept ByRolePriority
  ties or pre-rank.
- Re-dispatch retry is bounded but not free: a worst-case team
  with two retries and 4 sub-tasks runs 12 sub-transactions, each
  with its own apply/validate cycle. For a workflow that
  realistically converges, the cost is acceptable; for a
  workflow that thrashes, the operator-visible cost surfaces in
  the audit log as `audit.team_transaction.re_dispatched` events
  with steadily climbing `retry_count` — explicit, observable,
  and bounded by `max_retries` plus the team's wall-clock
  `timeout`.
- "Emergent semantic conflict" detection is only as good as ADR
  0037's diagnostic coverage. A change in a tree the LSP server
  does not index (Markdown, YAML, custom configs) cannot
  surface a semantic conflict via this layer; for those, the
  test phase is the only safety net. ADR 0040's repo-map can
  shrink the gap by widening the diagnostic-reachability set,
  but does not close it.
- Test-conflict detection requires re-running the merged tree's
  test suite, which dominates wall-clock for any non-trivial
  Rust project. We mitigate by accepting the cost: the alternative
  (skipping the merged-tree test phase) is exactly the failure
  mode this ADR exists to prevent.
- The wire shape of `TeamTransactionPhase` is now a long-lived
  contract, identically constrained to ADR 0031's
  `TransactionPhase`. Adding new phases requires backward-
  compatible serde defaults.
- Cross-verification at merge-time risks **echo loops**: a team
  whose architect persona is a verifier in ADR 0038 plan-time
  and the same agent serves as merge-time verifier provides no
  independent signal. We document the failure mode and reuse ADR
  0038's verifier-diversity guidance verbatim — the merge-time
  verifier should be a "materially different" model from any of
  the producers.
- Adversarial review surfaces concern about contention: at 10×
  scale, multiple team transactions targeting the same trunk
  branch funnel through one CAS-FF point, identical to ADR
  0031's single-agent contention. The aggregator does not
  implement an admission queue; concurrent team finalize calls
  on the same `(repo, trunk_branch)` will mostly see CAS
  failures and either retry within wall-clock or roll back.
  This is correct (no silent overwrite) but throughput-limited;
  a queue is a follow-up shared with ADR 0031's open question.
- A team transaction's blast radius on rollback is larger than
  ADR 0031's: a `FailTeam` cascade may discard the work of N
  sub-agents that individually validated. The artifacts are
  retained for post-hoc review (ADR 0016 retention) so the work
  is not lost; but the operator-visible cost of a single
  emergent conflict is N agent-runs of compute. Mitigation:
  prefer `ReDispatch` over `FailTeam` for any team where peer
  diffs are likely to inform the retry; reserve `FailTeam` for
  hard-budget workflows.

### Neutral / follow-ups

- A separate ADR can introduce **partial commit** semantics: land
  the architect's diff and re-dispatch only the executors. The
  current ADR rejects this for simplicity (the integration tree
  is the unit of commit) but the wire shape supports it: an
  `ApplicationOrder::Explicit` plus an `IntegrationConflictPolicy`
  variant `CommitConfirmed { committed: Vec<String>,
  redispatch: Vec<String> }` would suffice.
- A separate ADR may add `merge_strategy: Squash` or
  `MergeCommit` to the team finalize once a use case demands it.
  v1 fast-forwards exactly like ADR 0031, preserving every
  sub-agent's per-task commits intact for git-blame.
- A separate ADR can move FF-merge into `GitOperations` (ADR
  0030); both this ADR and ADR 0031 currently shell out to
  `update-ref` directly. The two share a primitive; once a third
  consumer arrives, the migration is mechanical.
- Multi-repo team transactions (a team change spanning `ork`
  and `ork-webui` atomically) is the natural sequel — same
  shape as ADR 0031's multi-repo open question, lifted to the
  team level. Out of scope here.

## Alternatives considered

- **Apply diffs sequentially through ADR 0031's single-agent
  pipeline (no integration worktree).** Reject: each sub-diff
  would land on trunk in turn, leaving trunk in a partially
  applied state during the cycle and exposing it to other
  consumers. The whole point of this ADR is that the team is
  one atomic unit.
- **Use `git merge -X theirs` / `octopus merge` instead of
  patch-by-patch apply.** Reject: octopus merges fail on any
  textual overlap, and `-X theirs` silently loses changes from
  one side. The 3-way patch apply with explicit reject capture
  is the only option that surfaces conflicts as structured data.
- **Single coordinator that owns both ADR 0031 sub-transactions
  and the team-level integration.** Reject: ADR 0031 is already
  the single-agent atomicity primitive. Re-implementing it
  inside this aggregator would either fork the code or weaken
  the boundary. Composition is the right answer; ADR 0031 is
  the single agent's transaction, this ADR is the team's.
- **Run validation per-sub only, skip merged-tree validation.**
  Reject: that is the failure mode. Per-sub validation cannot
  detect emergent semantic or test conflicts, by construction.
- **Run only the merged-tree validation, skip per-sub.** Reject:
  that defers detection of bugs to merge time, so a single
  failing sub-agent fails the whole team rather than failing
  itself. ADR 0031 sub-transactions catch their own bugs early;
  ADR 0044 catches what only the merge can see.
- **Have each sub-agent push to a shared "integration" branch
  directly.** Reject: this re-introduces the contention and
  partial-state problem ADR 0031 exists to prevent. The
  coordinator's job is to *not* let partial state escape.
- **Use a CRDT-like merge of the diffs.** Reject: source code is
  not a CRDT. The places it appears to be (well-formatted
  blocks of disjoint hunks) are exactly the cases `git apply`
  handles cleanly already; the places it isn't (overlapping
  hunks, semantic dependencies) are exactly what this ADR exists
  to surface, not paper over.
- **Defer the merged-diff cross-verification to a separate ADR.**
  Considered. Decided against: the verifier infrastructure is
  the same as ADR 0038's, the wire shape is the same, and the
  team workflow's value proposition (multiple models, multiple
  perspectives, structurally aggregated) is exactly the moment
  cross-verification of a *merged* diff matters most. Separating
  it would force the implementing session to redesign verifier
  dispatch in v1.5.
- **Have HITL be the only failure mode (no `ReDispatch`,
  no `FailTeam`).** Reject: agentic team flows that pause for
  human review on every textual conflict are non-autonomous and
  defeat the team-orchestrator value proposition. HITL is the
  right escalation when retries exhaust; it is not the
  first-line resolution.
- **Adversarial review surfaced**: "what stops two team
  transactions on the same team_id, same repo, same trunk_branch
  from racing for the FF lock?" Considered. v1 inherits ADR
  0031's CAS posture: both team transactions race; the loser
  rolls back. This is correct but wasteful; a per-(repo,
  trunk_branch) admission queue is a shared follow-up with ADR
  0031.
- **Adversarial review surfaced**: "ApplicationOrder::ByRolePriority
  is hand-wavy; what if the architect is wrong?" The order is a
  *default*, not a correctness invariant. The validation pass
  catches errors regardless of order; the order only affects the
  shape of textual conflicts (which subtask gets blamed). Teams
  that suffer under the default move to `ByFewestConflicts` or
  `Explicit`. The point of having a default is to avoid
  forcing every workflow author to choose.

## Affected ork modules

- New: `crates/ork-core/src/ports/team_diff_aggregator.rs` —
  `TeamDiffAggregator` trait and supporting types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `team_diff_aggregator`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — `WorkflowStep::TeamTransactionalCodeChange` variant,
  `TeamSubtaskSpec`, `IntegrationConflictPolicy`,
  `OffenderSelection`, `MergedDiffVerificationPolicy`,
  `ApplicationOrder`, `StepResult.team_transaction` optional
  field.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
  — lower into `CompiledNode::TeamTransaction`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  — `run_team_transaction` arm, in-flight sweep on startup,
  HITL resume integration, sub-tx cascade-rollback helper,
  re-dispatch loop driver.
- New: `crates/ork-integrations/src/team_diff_aggregator.rs` —
  `LocalTeamDiffAggregator`, `git apply --3way` invocation
  through `ShellExecutor`, FF-merge invocation through
  `ShellExecutor`, merged-diff capture, peer-diff
  `team_remember` writes, HITL composition,
  `PlanCrossVerifier` composition.
- New: `crates/ork-persistence/src/postgres/team_transaction_repo.rs`
  — `PostgresTeamTransactionRepo` implementing the persistence
  facet; `team_transactions` and `team_transaction_subtasks`
  tables.
- New: [`migrations/011_team_transactions.sql`](../../migrations/)
  — table + index + additive `step_results.team_transaction_id`
  column.
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs)
  — boot a `LocalTeamDiffAggregator` from existing
  `Arc<dyn GitOperations>`, `Arc<dyn ShellExecutor>`,
  `Arc<dyn ArtifactStore>`, `Arc<dyn TransactionCoordinator>`,
  `Arc<dyn HumanInputGate>`, `Arc<dyn TeamMemory>`,
  `Arc<dyn LspDiagnostics>`, `Arc<dyn PlanCrossVerifier>`,
  `Arc<dyn TeamTransactionRepo>` and inject it into the
  `WorkflowEngine`.
- [`config/default.toml`](../../config/default.toml) —
  `[engine.team_transaction]` block.
- [`crates/ork-webui/`](../../crates/ork-webui/) — render
  `team-tx-*.merged.diff` and `team-tx-*.validation` artifacts
  inline on the `InputRequired` form when the active HITL
  request was opened by a team transaction. Render the per-sub
  `subtask-*.patch` artifacts as a tabbed view alongside the
  merged diff. Visual treatment is a Web UI detail; the
  schema-driven form already works without it.
- [`workflow-templates/`](../../workflow-templates/) —
  reference YAML for `implement-this-ADR-as-team` rebuilt on
  top of `team_transactional_code_change` once the
  implementation lands (out of scope for this ADR; tracked
  under the implementation issue).

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on
the implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3,
step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Git octopus / 3-way merge | `git merge`, `git apply --3way` | Textual-conflict layer of `IntegrationApplyOutcome` |
| Database two-phase commit | XA / 2PC across resource managers | (N+1)-phase: per-sub ADR-0031 tx + team-level integration tx, with the team owning the global commit decision |
| GitHub merge queue / Bors | Serialised merge of N PRs onto a shared trunk | Team-level FF-merge with CAS on `trunk_commit`; ADR 0044 plays the merge-queue role for *agent-produced* PRs |
| Microsoft / OpenAI MetaGPT | Multi-agent code synthesis with ad-hoc merge | Structurally typed apply/validate/finalize over `SubDiffArtifact`s |
| Aider `--watch-files` collaborative mode | Single human + single LLM share one worktree | Replaced by N isolated sub-worktrees (ADR 0041) and explicit aggregation (this ADR) |
| LangGraph multi-agent supervisor patterns | Supervisor LLM hand-merges branches in prompt | Engine-enforced merge with conflict types and ADR 0027 escalation |
| Solace Agent Mesh | No first-class team merge primitive | `WorkflowStep::TeamTransactionalCodeChange` |

## Open questions

- **Partial commit on partial success.** Should the aggregator
  commit the architect's diff and only re-dispatch the
  executors, or always treat the team as one atomic unit?
  v1 chooses atomicity for simplicity; the wire shape is
  forward-compatible with a future `CommitConfirmed { committed,
  redispatch }` variant of `IntegrationConflictPolicy`.
- **`ByFewestConflicts` enumeration policy beyond `N = 8`.**
  Brute-force permutation is `O(N!)`. For larger teams a greedy
  heuristic (apply diffs ordered by reject-count contribution)
  may be acceptable but loses optimality. Decided to defer to
  implementation; teams hitting the cap are nudged to
  `Explicit`.
- **Sub-agent idempotency on re-dispatch.** ADR 0045 must
  decide whether re-dispatched sub-agents see a fresh
  conversation or the prior turn-history. v1 assumes fresh
  conversation plus the `team_remember` peer-diff entries; ADR
  0045 may relax this.
- **Cross-verification of every retry.** Currently
  cross-verification runs once on the merged diff after
  validation passes. Should it re-run after a re-dispatch?
  Default proposal: yes, because a re-dispatched team's merged
  diff is structurally a different artifact. The per-verifier
  cost may push us to "only the final merge"; revisit when
  empirical workflows surface.
- **Peer-diff size cap.** Surfaced peer diffs as
  `team_remember` entries can grow large for refactor-shaped
  teams. ADR 0043's spillover policy applies, but the agent's
  context window may still overflow. v1 caps the peer-diff
  body at the team's `team_recall.body_max_bytes`; an explicit
  per-entry summarisation pass is a follow-up.
- **Integration-worktree lifetime on `ReDispatching`.** v1
  drops and re-allocates the integration worktree on each
  retry. An optimisation reuses the integration worktree but
  resets it to `trunk_commit` between retries; the simplicity
  cost of the v1 path is worth the implementation clarity.
- **Multi-repo team transactions.** A team change spanning
  `ork` and `ork-webui` requires 2PC across two integration
  worktrees. Out of scope for v1; same posture as ADR 0031's
  multi-repo open question.
- **Trunk push integration.** This ADR commits to a *local*
  trunk branch. Publishing the result (open a PR, push to a
  remote) is out of scope; the same `push`-gating ADR that ADR
  0030 / 0031 anticipate will compose with this one.

## References

- ADR [`0016`](0016-artifact-storage.md) — diff, validation, and
  verdict artifact spill.
- ADR [`0022`](0022-observability.md) — audit events and OTLP
  spans.
- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)
  — typed-output verifier port; an alternative source of merge-
  time sign-off.
- ADR [`0027`](0027-human-in-the-loop.md) — `HumanInputGate` for
  the `EscalateToHuman` policy.
- ADR [`0028`](0028-shell-executor-and-test-runners.md) —
  `ShellExecutor`, `run_tests`, `TestSummary` for merged-tree
  validation and per-sub re-run.
- ADR [`0029`](0029-workspace-file-editor.md) —
  `WorkspaceHandle` field re-used for the integration worktree.
- ADR [`0030`](0030-git-operations.md) — `GitOperations`,
  worktree provisioning, FF-merge primitive shared with ADR
  0031.
- ADR [`0031`](0031-transactional-code-changes.md) — extends:
  team transactions are composed of ADR-0031 sub-transactions
  plus an integration ADR-0031 transaction.
- ADR [`0037`](0037-lsp-diagnostics.md) — diagnostic source for
  the semantic-conflict layer.
- ADR [`0038`](0038-plan-mode-and-cross-verification.md) —
  `PlanCrossVerifier`, `PlanVerdict`, `submit_plan_verdict`,
  aggregation policies reused for merged-diff cross-verification.
- ADR [`0041`](0041-nested-workspaces.md) — consumer of the
  `SubDiffArtifact` shape; provides the per-sub-worktree
  isolation this ADR aggregates over.
- ADR [`0043`](0043-team-shared-memory.md) — `TeamMemory` port
  for re-dispatch peer-diff entries and human-resolution notes.
- ADR `0045` (planned) — team orchestrator; the producer of the
  per-sub ADR-0031 transactions this ADR aggregates.
- `git apply --3way`: <https://git-scm.com/docs/git-apply>.
- `git update-ref` (compare-and-swap):
  <https://git-scm.com/docs/git-update-ref>.
- Du et al., *Improving Factuality and Reasoning in Language
  Models through Multiagent Debate* (2023):
  <https://arxiv.org/abs/2305.14325>.
- Google Research, *Towards a Science of Scaling Agent
  Systems — When and Why Agent Systems Work* (April 2025):
  <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/>.
