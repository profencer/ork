# 0031 — Transactional Code Changes & Rollback

- **Status:** Proposed
- **Date:** 2026-04-28
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0016, 0018, 0022, 0027, 0028, 0029, 0030
- **Supersedes:** —

## Context

After ADRs [0028](0028-shell-executor-and-test-runners.md) (shell +
test runners), [0029](0029-workspace-file-editor.md) (file editor),
and [0030](0030-git-operations.md) (git + worktrees) land, ork has
the *primitives* an autonomous coding agent needs: it can edit a
file, run the tests, inspect the diff, and commit. What it does not
have is a **workflow primitive that ties those primitives together
atomically**.

Three concrete pressures converge on this gap:

1. **Autonomous coding loops without a validation gate are
   dangerous.** Today a workflow that does "edit → commit" can land
   broken code: nothing runs the tests, nothing inspects the diff,
   nothing rolls back if the validation fails. The loop is the
   correctness boundary, and the loop is currently inside the LLM's
   prompt rather than the engine.
2. **ADR [0027](0027-human-in-the-loop.md) is approval-shaped, not
   rollback-shaped.** `HumanInputGate` answers "should we proceed?"
   It does not answer "the agent's edit broke the test suite — what
   happens to the worktree?" Auto-rollback on validation failure is
   a separate concern that *composes* with HITL but is not
   subsumed by it.
3. **ADR [0018](0018-dag-executor-enhancements.md)'s composition
   primitives (`Parallel`, `Switch`, `Map`, `Loop`) are
   shape-only.** They do not couple their child steps to a
   validation gate, do not commit on success, and do not unwind on
   failure. Authoring a "fix-this-bug" workflow on top of `Loop` +
   `Agent` today means the workflow author re-implements rollback
   policy in YAML or in the agent prompt — both are unobservable
   and unenforced.

The pieces are present:

- ADR [0030](0030-git-operations.md) gives us **per-run worktrees**
  via `GitOperations::open_workspace`. A worktree is the natural
  unit of isolation: dropping the worktree without merging is a
  zero-side-effects rollback.
- ADR [0028](0028-shell-executor-and-test-runners.md) gives us
  **`run_tests`** with structured `TestSummary` output suitable for
  driving a pass/fail gate.
- ADR [0029](0029-workspace-file-editor.md) gives us **structured
  edits** that scope writes to the worktree.
- ADR [0027](0027-human-in-the-loop.md) gives us **`HumanInputGate`**
  for surfacing a paused diff to a reviewer.
- ADR [0016](0016-artifact-storage.md) gives us **artifact spill**
  for capturing the diff and validation output at audit-grade
  durability.

What is missing is the **glue**: a step kind that opens a worktree,
drives an apply phase, drives a validate phase, and either
fast-forwards the base branch onto the task branch (commit) or
drops the worktree (rollback) — atomically, observably, and with
crash-safe resume semantics.

The
[`workflow-templates/`](../../workflow-templates/) drafts for
"fix-this-bug" and "implement-this-ADR" cited in ADR
[0030](0030-git-operations.md) all assume this primitive. Without
it, every coding workflow re-derives rollback policy from scratch.

## Decision

ork **introduces** a new workflow step kind
`TransactionalCodeChange`, a `TransactionCoordinator` port in
`ork-core` with a `LocalTransactionCoordinator` implementation in
`ork-integrations`, a small extension to `GitOperations` that adds
**fast-forward-only** branch advancement, and a Postgres
`workflow_transactions` table that persists the lifecycle phase so
the engine can recover from a crash.

A transaction's contract is:

> Either every step in `apply` ran, every step in `validate`
> succeeded, and the **base branch** has been fast-forwarded onto
> the **task branch**'s tip — or the worktree has been dropped and
> the base branch is untouched. There is no third state visible to
> downstream steps.

`on_failure` decides what "validate failed" means, but the
all-or-nothing observation invariant holds across all three
policies.

### `TransactionalCodeChange` step kind

[`WorkflowStep`](../../crates/ork-core/src/models/workflow.rs) (the
enum from ADR [0018](0018-dag-executor-enhancements.md)) gains a
new variant:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowStep {
    Agent { /* ... existing ... */ },
    Parallel { /* ... existing ... */ },
    Switch { /* ... existing ... */ },
    Map { /* ... existing ... */ },
    Loop { /* ... existing ... */ },

    /// NEW: an apply/validate pair that commits the worktree on
    /// success, drops it on failure, and never leaves the base
    /// branch in a half-applied state.
    TransactionalCodeChange {
        id: String,
        /// Logical repo name from the tenant's `RepositorySpec`
        /// set (same vocabulary as ADR 0030's
        /// `OpenWorkspaceRequest.repo`).
        repo: String,
        /// Branch to base the worktree on. Required; the engine
        /// does not infer "main".
        base_branch: String,
        /// Template for the task branch. `None` defaults to
        /// `"ork/run-{run_id}/tx-{step_id}"`. Resolved through the
        /// existing template engine, so callers can substitute
        /// `{tenant_id}`, `{run_id}`, `{step_id}`.
        task_branch_template: Option<String>,
        /// Steps that mutate the worktree. Composes recursively
        /// with every other `WorkflowStep` kind: an `apply` block
        /// can itself contain `Parallel`, `Map`, `Loop`.
        apply: Vec<WorkflowStep>,
        /// Steps whose pass/fail decides the transaction's
        /// outcome. The engine walks `validate` exactly like a
        /// regular sub-graph; `validate` succeeds iff every leaf
        /// step succeeds.
        validate: Vec<WorkflowStep>,
        on_failure: OnTransactionFailure,
        /// Hard cap on wall-clock for the whole transaction
        /// (apply + validate + finalize). Defaults to
        /// `engine.transaction_default_timeout` from
        /// [`config/default.toml`](../../config/default.toml).
        timeout: Option<Duration>,
        depends_on: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OnTransactionFailure {
    /// Drop the worktree, mark the step `Failed`, propagate
    /// failure to the parent container per the existing
    /// `JoinPolicy` / `ItemFailurePolicy` rules.
    Rollback,

    /// Pause the run, surface the diff + validation output to a
    /// human via ADR 0027's `HumanInputGate`. Approval forces a
    /// commit (overriding the validation failure); rejection
    /// rolls back; timeout follows `on_approval_timeout`.
    RequestApproval {
        /// RBAC scopes (ADR 0021) the resolver must hold.
        required_scopes: Vec<String>,
        /// Markdown prompt presented to the reviewer alongside
        /// the diff and the failed validation output.
        prompt: String,
        /// Approval window. `None` = inherit run timeout.
        approval_timeout: Option<Duration>,
        on_approval_timeout: ApprovalTimeoutPolicy,
    },

    /// Drop the worktree and abort the entire run with
    /// `WorkflowRunStatus::Failed`. Skips parent join policies —
    /// this is the "stop the world" lever for transactions
    /// inside a `Map` where partial success is unacceptable.
    FailRun,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTimeoutPolicy {
    Rollback,
    FailRun,
}
```

YAML shape:

```yaml
- id: fix-bug-1234
  kind: transactional_code_change
  repo: ork
  base_branch: main
  apply:
    - id: edit
      kind: agent
      agent: coder
      tools: [read_file, write_file, git_status]
      prompt_template: "Fix issue #1234. Edit files; do not commit."
  validate:
    - id: tests
      kind: agent
      agent: runner
      tools: [run_tests]
      prompt_template: "Run `cargo test --workspace` and report."
  on_failure:
    kind: request_approval
    required_scopes: ["workflow:approve:code"]
    prompt: |
      Tests failed after the agent's edit. Inspect the diff and
      validation output, then approve to land it anyway, edit to
      replace the diff with your version, or reject to roll back.
    approval_timeout: 1h
    on_approval_timeout: rollback
  timeout: 30m
```

### `TransactionCoordinator` port

```rust
// crates/ork-core/src/ports/transaction.rs

use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{RunId, StepId, TenantId};

use crate::ports::human_input::HumanInputRequestId;     // ADR 0027
use crate::ports::workspace::WorkspaceHandle;           // ADR 0029

/// Drives the lifecycle of one `TransactionalCodeChange` step.
///
/// The coordinator owns:
/// - worktree provisioning (delegated to `GitOperations`);
/// - persistence of the lifecycle phase (durable via
///   `workflow_transactions`);
/// - finalization: fast-forward-merge on success, drop on failure;
/// - composition with `HumanInputGate` for `RequestApproval`.
///
/// It does **not** drive the apply or validate sub-steps —
/// `WorkflowEngine` walks those, then calls `finalize` with the
/// outcome.
#[async_trait]
pub trait TransactionCoordinator: Send + Sync {
    /// Provision a worktree for this transaction.
    ///
    /// Idempotent per `(tenant_id, run_id, step_id)`: a second
    /// call with the same key returns the existing handle and
    /// the existing `transaction_id`. This is what makes engine
    /// crash-recovery safe (§ Resume semantics).
    async fn open(
        &self,
        req: OpenTransactionRequest,
    ) -> Result<TransactionHandle, OrkError>;

    /// Mark the transaction as having entered or completed a
    /// lifecycle phase. The engine calls this on every transition
    /// so persisted state matches in-memory state.
    async fn record_phase(
        &self,
        transaction_id: TransactionId,
        phase: TransactionPhase,
    ) -> Result<(), OrkError>;

    /// Decide the transaction's outcome given the validation
    /// result and the on_failure policy.
    ///
    /// Returns:
    /// - `Committed { merge_commit }` if validation passed (or
    ///   approval was granted), the fast-forward merge succeeded,
    ///   and the worktree was removed.
    /// - `RolledBack { reason }` if validation failed and the
    ///   policy resolved to rollback, or the FF merge could not
    ///   be performed.
    /// - `AwaitingApproval { request_id }` if the policy is
    ///   `RequestApproval`. The engine yields; resume happens
    ///   when the HITL gate fires.
    /// - `RunFailed { reason }` if the policy is `FailRun`.
    async fn finalize(
        &self,
        tx: &TransactionHandle,
        outcome: ValidationOutcome,
        on_failure: &OnTransactionFailure,
    ) -> Result<TransactionResolution, OrkError>;

    /// Resume from a HITL approval decision. Called by the engine
    /// after `HumanInputGate::resolve` fires for a transaction
    /// that was `AwaitingApproval`.
    async fn resume_from_approval(
        &self,
        tx: &TransactionHandle,
        decision: ApprovalDecision,
    ) -> Result<TransactionResolution, OrkError>;

    /// Force-abort. Used by engine-restart sweepers and by
    /// cooperative cancellation (ADR 0018's `CancellationToken`).
    async fn abort(
        &self,
        transaction_id: TransactionId,
        reason: AbortReason,
    ) -> Result<(), OrkError>;

    /// List transactions that are not in a terminal phase. Called
    /// by the engine on startup to drive crash recovery (§ Resume
    /// semantics).
    async fn list_in_flight(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<InFlightTransaction>, OrkError>;
}

#[derive(Debug, Clone)]
pub struct OpenTransactionRequest {
    pub tenant_id: TenantId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub repo: String,
    pub base_branch: String,
    pub task_branch: String,
    /// Honoured iff `task_branch` matches the protected pattern
    /// (ADR 0030). Default `false`.
    pub allow_protected_branch: bool,
    /// Wall-clock cap for the whole transaction. The coordinator
    /// records it; enforcement is the engine's responsibility
    /// using a `tokio::time::timeout` around the recursive walk.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct TransactionHandle {
    pub transaction_id: TransactionId,
    pub workspace: WorkspaceHandle,
    /// Tip of `base_branch` at the moment `open` ran. Used by
    /// `finalize` to detect "the base branch advanced under us"
    /// and refuse the FF merge if so.
    pub base_commit: String,
    pub task_branch: String,
    pub phase: TransactionPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionId(pub uuid::Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransactionPhase {
    /// Worktree allocated, apply not yet started.
    Opened,
    /// Apply substeps in flight.
    Applying,
    /// Apply substeps complete; diff captured but not yet
    /// validated.
    Applied { diff_artifact_id: String },
    /// Validate substeps in flight.
    Validating { diff_artifact_id: String },
    /// Validation complete (pass or fail), waiting for finalize.
    Validated {
        diff_artifact_id: String,
        outcome: ValidationOutcome,
    },
    /// HITL pause for `RequestApproval`. The diff and validation
    /// output are stored as artifacts; the request_id links to
    /// `human_input_requests`.
    AwaitingApproval {
        diff_artifact_id: String,
        validation_artifact_id: String,
        request_id: HumanInputRequestId,
    },
    /// Terminal: FF merge succeeded, worktree removed.
    Committed { merge_commit: String },
    /// Terminal: worktree dropped without affecting base branch.
    Aborted {
        reason: AbortReason,
        diff_artifact_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidationOutcome {
    Pass,
    Fail {
        /// Step id of the failing validate sub-step (deepest
        /// leaf that returned `Failed`).
        step_id: StepId,
        /// Short message for the reviewer / log.
        reason: String,
        /// Captured stdout/stderr / TestSummary as ADR 0016
        /// artifact. `None` if the step produced no output.
        validation_artifact_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransactionResolution {
    Committed {
        merge_commit: String,
    },
    RolledBack {
        reason: AbortReason,
    },
    AwaitingApproval {
        request_id: HumanInputRequestId,
    },
    RunFailed {
        reason: AbortReason,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Reject,
    /// Reviewer-supplied edit replaced the agent's diff. The
    /// coordinator does not implement this in v1 (open question);
    /// it is reserved in the wire enum so ADR 0027's `Edit`
    /// decision has an explicit mapping when v2 lands.
    Edit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    ApplyFailed,
    ValidationFailed,
    ApprovalRejected,
    ApprovalTimedOut,
    Cancelled,
    EngineRestart,
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct InFlightTransaction {
    pub transaction_id: TransactionId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub workspace: WorkspaceHandle,
    pub phase: TransactionPhase,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}
```

### Lowering and engine integration

The compiler in
[`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
adds a new `CompiledNode::Transaction` variant whose fields mirror
the step kind, with `apply` and `validate` lowered to child node
ids (matching the ADR 0018 pattern):

```rust
pub enum CompiledNode {
    Agent { ... },
    Parallel { ... },
    Switch { ... },
    Map { ... },
    Loop { ... },
    Transaction {
        repo: String,
        base_branch: String,
        task_branch_template: String,
        apply: NodeId,                    // a synthetic Sequence node
        validate: NodeId,                 // a synthetic Sequence node
        on_failure: OnTransactionFailure,
        timeout: Option<Duration>,
    },
}
```

The engine's recursive walker grows one new arm:

```rust
async fn run_transaction(
    &self,
    ctx: &RunContext,
    node: &CompiledNode, // Transaction variant
) -> Result<NodeOutput, OrkError> {
    let task_branch = ctx.render(&node.task_branch_template);
    let tx = self.tx_coordinator.open(OpenTransactionRequest {
        tenant_id: ctx.tenant_id,
        run_id: ctx.run_id,
        step_id: ctx.current_step_id(),
        repo: node.repo.clone(),
        base_branch: node.base_branch.clone(),
        task_branch,
        allow_protected_branch: false,
        timeout: node.timeout,
    }).await?;

    // Bind the workspace into a derived RunContext so apply +
    // validate sub-steps see it via AgentContext.workspace
    // (ADR 0028 / 0029 / 0030 all read this field).
    let tx_ctx = ctx.with_workspace(tx.workspace.clone());

    self.tx_coordinator.record_phase(tx.transaction_id, TransactionPhase::Applying).await?;
    let apply_result = self.run_node(&tx_ctx, &node.apply).await;
    if let Err(e) = apply_result {
        self.tx_coordinator.abort(tx.transaction_id, AbortReason::ApplyFailed).await?;
        return Err(e);
    }
    let diff_artifact_id = self.capture_diff(&tx).await?;
    self.tx_coordinator.record_phase(
        tx.transaction_id,
        TransactionPhase::Applied { diff_artifact_id: diff_artifact_id.clone() },
    ).await?;

    self.tx_coordinator.record_phase(
        tx.transaction_id,
        TransactionPhase::Validating { diff_artifact_id: diff_artifact_id.clone() },
    ).await?;
    let validate_outcome = self.run_validate(&tx_ctx, &node.validate).await;

    self.tx_coordinator.record_phase(
        tx.transaction_id,
        TransactionPhase::Validated {
            diff_artifact_id,
            outcome: validate_outcome.clone(),
        },
    ).await?;

    let resolution = self.tx_coordinator
        .finalize(&tx, validate_outcome, &node.on_failure)
        .await?;
    self.handle_resolution(ctx, &tx, resolution).await
}
```

`capture_diff` calls `GitOperations::diff` against `base_commit`,
truncates per the existing ADR 0030 spill rules, and writes the
full unified diff to ADR 0016 storage with scope
`(tenant_id, run_id, "transactions")` and name
`tx-{transaction_id}.diff`.

`handle_resolution` is the policy switch:

- `Committed { merge_commit }` → write the commit SHA into the
  step's output, mark `StepStatus::Completed`.
- `RolledBack { reason }` → mark `StepStatus::Failed`, return up
  through the parent's `JoinPolicy` / `ItemFailurePolicy`.
- `AwaitingApproval { request_id }` → set the run to
  `WorkflowRunStatus::InputRequired`, write the
  `pending_human_input` marker on the step (ADR 0027), and yield.
  The engine's existing resume path
  ([`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  on Redis pubsub `ork.hitl.<request_id>`) wakes back into
  `resume_transaction_from_approval`.
- `RunFailed { reason }` → mark the *run* `Failed` directly,
  bypassing parent join policies.

### Fast-forward merge on success

`LocalTransactionCoordinator::finalize` performs the merge by
shelling out to `git update-ref` via `ShellExecutor`, **inside the
read-cache `.git` dir**, not inside the worktree:

```text
git -C <read-cache-root> update-ref \
    refs/heads/<base_branch> <task_branch_tip> <base_commit>
```

The `<expected_old_value>` argument (`<base_commit>` recorded at
`open`) makes the operation a compare-and-swap: if the base branch
moved between `open` and `finalize`, `git` refuses with exit code 1
and the coordinator returns `RolledBack { reason:
ValidationFailed }` — same as a validate failure with policy
`Rollback`. The transaction is *never* allowed to silently
overwrite a concurrent change to the base branch. There is no
`--force` option. Tests cover this case explicitly.

We deliberately do **not** add `merge` to `GitOperations` (ADR
[0030](0030-git-operations.md)'s safety rails defer it). The
coordinator's `update-ref` invocation is *the* policy-bearing
merge in the codebase: there is exactly one place that advances a
named branch in response to agent work, and that place is here.
The same code path emits an `audit.transaction.committed` event
(ADR [0022](0022-observability.md)) so the action is observable.

After the FF merge:

1. The coordinator deletes the task branch ref (`git branch -D
   <task_branch>`) — the work now lives on the base branch and the
   task branch is bookkeeping noise.
2. It calls `GitOperations::close_workspace` to remove the
   worktree.
3. It records `TransactionPhase::Committed { merge_commit }`.

On rollback:

1. The coordinator calls `GitOperations::close_workspace` with
   `force = true` — the worktree's working tree may be dirty (a
   half-applied edit), and that is *expected* on the rollback path.
2. It optionally deletes the task branch ref unless the rollback
   reason is `ApprovalRejected` and the operator opted to retain
   rejected branches (`config.transaction.retain_rejected_branches
   = true`, default `false`).
3. It records `TransactionPhase::Aborted { reason,
   diff_artifact_id }`.

### `RequestApproval` flow (composes with ADR 0027)

When `OnTransactionFailure::RequestApproval { ... }` fires, the
coordinator:

1. Captures the validation output as an artifact (`ArtifactStore`,
   scope `(tenant_id, run_id, "transactions")`, name
   `tx-{transaction_id}.validation.txt`).
2. Builds a `NewHumanInputRequest` (ADR 0027) with:
   - `kind = HumanInputKind::Approval`,
   - `prompt = the on_failure.prompt`,
   - `input_schema = the fixed JSON Schema below`,
   - `required_scopes = on_failure.required_scopes`,
   - `expires_at = now + on_failure.approval_timeout`,
   - `on_timeout = TimeoutPolicy::Default(value)` where `value`
     is `{"decision": "rollback"}` if
     `on_approval_timeout = Rollback`, or
     `{"decision": "fail_run"}` if `FailRun`.
3. Calls `HumanInputGate::open`. The push payload (ADR 0009)
   carries deep links to both artifacts so external reviewers
   (Slack, email) can read the diff and validation output.
4. Records `TransactionPhase::AwaitingApproval`.
5. Returns `TransactionResolution::AwaitingApproval { request_id }`
   to the engine, which yields.

The fixed schema:

```json
{
  "type": "object",
  "properties": {
    "decision": { "type": "string", "enum": ["approve", "reject", "edit"] },
    "notes":    { "type": "string" }
  },
  "required": ["decision"]
}
```

When `HumanInputGate::resolve` fires (Web UI form submit, peer
A2A `message/send`, CLI `ork hitl approve`), the engine's resume
path calls `TransactionCoordinator::resume_from_approval` with the
mapped `ApprovalDecision`:

- `Approve` → run the FF merge as if validation had passed. The
  audit event is `audit.transaction.committed_after_approval` so
  human-overridden landings are searchable.
- `Reject` → call `abort(reason: ApprovalRejected)`.
- `Edit` → reserved; v1 returns
  `OrkError::Validation("approval edit not supported in v1")`. ADR
  0027 already calls out the `Edit` payload as a v2 follow-up.

### Persistence: `workflow_transactions`

```sql
-- migrations/010_workflow_transactions.sql

CREATE TABLE workflow_transactions (
    transaction_id        UUID PRIMARY KEY,
    tenant_id             TEXT NOT NULL,
    run_id                UUID NOT NULL,
    step_id               TEXT NOT NULL,
    repo                  TEXT NOT NULL,
    base_branch           TEXT NOT NULL,
    task_branch           TEXT NOT NULL,
    base_commit           TEXT NOT NULL,
    workspace_root        TEXT NOT NULL,
    phase                 JSONB NOT NULL,
    on_failure            JSONB NOT NULL,
    diff_artifact_id      TEXT,
    validation_artifact_id TEXT,
    human_input_request_id UUID,
    merge_commit          TEXT,
    abort_reason          TEXT,
    opened_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_phase_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at             TIMESTAMPTZ,
    UNIQUE (tenant_id, run_id, step_id)
);

CREATE INDEX workflow_transactions_in_flight_idx
    ON workflow_transactions (tenant_id, last_phase_at)
    WHERE closed_at IS NULL;
```

The `UNIQUE (tenant_id, run_id, step_id)` constraint backs the
idempotency contract on `open`: a second `open` call for the same
key fetches the existing row instead of creating a new
transaction or worktree.

`step_results` (ADR 0018) gains an optional `transaction_id UUID`
column joining to `workflow_transactions`, surfaced as a
`transaction: Option<TransactionStepResult>` field on
`StepResult` for the engine and Web UI to render.

### Resume / crash-recovery semantics

On engine startup, after the existing run-recovery loop in
[`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs),
the engine calls `TransactionCoordinator::list_in_flight` per
tenant. For each row, the action depends on `phase`:

| Persisted phase | Action on restart |
| --------------- | ----------------- |
| `Opened` | `abort(EngineRestart)`. The apply hadn't started; nothing has been edited; dropping the worktree is free. The parent step is re-tried per the existing run-recovery logic. |
| `Applying` | `abort(EngineRestart)`. The apply may have partially run; the worktree state is unobservable from the outside (no commit yet); drop it. |
| `Applied` / `Validating` | `abort(EngineRestart)`. Validation hadn't completed; we cannot land an unvalidated diff. |
| `Validated { Pass }` | Re-attempt `finalize` against the recorded resolution. The FF merge is idempotent — if the base branch already advanced to `task_branch_tip`, we observe `merge_commit` matches and record `Committed`. |
| `Validated { Fail }` | Re-dispatch `on_failure`. For `Rollback` and `FailRun` this is fast and safe; for `RequestApproval` the engine re-opens the HITL request **only if** no `human_input_request_id` is recorded — otherwise it joins the existing one. |
| `AwaitingApproval` | No action; the existing HITL resume path handles it (the Redis pubsub channel `ork.hitl.<request_id>` is durable in the sense that ADR 0027 polls for resolution on startup). |
| `Committed` / `Aborted` | Terminal; nothing to do. The closed_at is non-null on these rows; a daily prune deletes rows older than `config.transaction.retention_days`. |

The "abort on restart" cases (`Opened`/`Applying`/`Applied`/
`Validating`) are conservative on purpose: re-running an apply
phase that may have touched external state (network calls, MCP
tool invocations) is not safe in general, even though the
worktree itself is. The engine surfaces the abort as a step
failure and lets the run's containing logic decide whether to
retry. Workflow authors who want automatic retry wrap the
`TransactionalCodeChange` in a `Loop` with an `until` condition
that inspects the transaction's resolution.

### Observability

ADR [0022](0022-observability.md) gains transaction-aware events;
no new pillar, just structured log/trace records on the existing
audit and tracing surfaces:

| Event | Emitted when | Carries |
| ----- | ------------ | ------- |
| `audit.transaction.opened` | `open` succeeds | `transaction_id`, `repo`, `base_branch`, `task_branch`, `base_commit`, `workspace_root` |
| `audit.transaction.applied` | `Applied` phase recorded | `transaction_id`, `diff_artifact_id`, files-changed/+/- counts |
| `audit.transaction.validated` | `Validated` phase recorded | `transaction_id`, `outcome` (Pass/Fail), failing step_id if Fail |
| `audit.transaction.approval_requested` | `AwaitingApproval` phase recorded | `transaction_id`, `request_id`, `required_scopes` |
| `audit.transaction.committed` | FF merge succeeded | `transaction_id`, `merge_commit`, whether approval-overridden |
| `audit.transaction.aborted` | `Aborted` phase recorded | `transaction_id`, `reason`, `diff_artifact_id` |

OTLP spans nest: `transaction:<id>` is the parent of every
`apply:*` and `validate:*` span. The diff-capture and FF-merge
sub-operations get their own spans for latency visibility.

### Configuration

```toml
# config/default.toml
[engine.transaction]
default_timeout_seconds = 1800            # 30 min
diff_max_bytes          = 524288          # 512 KiB inline; rest spills to artifact
retain_rejected_branches = false
retention_days          = 30              # closed transactions pruned after N days
```

## Acceptance criteria

- [ ] Step-kind enum variant `WorkflowStep::TransactionalCodeChange`
      defined in
      [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
      with the fields shown in `Decision`, alongside
      `OnTransactionFailure` and `ApprovalTimeoutPolicy` enums.
- [ ] YAML round-trip parses the example under `Decision` into the
      enum variant, verified by
      `crates/ork-core/tests/workflow_kinds/transaction_yaml.rs::round_trip_minimal`
      and `::round_trip_request_approval`.
- [ ] Trait `TransactionCoordinator` defined at
      `crates/ork-core/src/ports/transaction.rs` with the signature
      shown in `Decision`, re-exported from
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs).
- [ ] Supporting types `OpenTransactionRequest`, `TransactionHandle`,
      `TransactionId`, `TransactionPhase`, `ValidationOutcome`,
      `TransactionResolution`, `ApprovalDecision`, `AbortReason`,
      `InFlightTransaction` defined in the same module.
- [ ] `LocalTransactionCoordinator` defined at
      `crates/ork-integrations/src/transaction.rs`, constructed
      from an `Arc<dyn GitOperations>`, an `Arc<dyn ShellExecutor>`,
      an `Arc<dyn ArtifactStore>`, an
      `Arc<dyn HumanInputGate>` (optional — `None` => panic at
      step-time when `RequestApproval` policy is used), and an
      `Arc<dyn TransactionRepo>` (Postgres-backed).
- [ ] Migration `migrations/010_workflow_transactions.sql` creates
      the `workflow_transactions` table with the schema shown in
      `Persistence`, including the `UNIQUE (tenant_id, run_id,
      step_id)` constraint and the partial index.
- [ ] `LocalTransactionCoordinator::open` is idempotent for a
      repeated `(tenant_id, run_id, step_id)` tuple and returns
      the same `TransactionId` and `WorkspaceHandle`, verified by
      `crates/ork-integrations/tests/transaction.rs::open_is_idempotent`.
- [ ] `LocalTransactionCoordinator::finalize` performs a
      compare-and-swap fast-forward via
      `git update-ref refs/heads/<base> <new> <base_commit>` and
      records `Committed`, verified by
      `crates/ork-integrations/tests/transaction.rs::finalize_ff_merge_records_commit`.
- [ ] `LocalTransactionCoordinator::finalize` rolls back when the
      base branch advanced under the transaction (CAS failure),
      verified by
      `crates/ork-integrations/tests/transaction.rs::finalize_refuses_when_base_moved`.
- [ ] `LocalTransactionCoordinator::finalize` with policy
      `Rollback` and `ValidationOutcome::Fail` calls
      `GitOperations::close_workspace` with `force = true` and
      leaves the base branch ref unchanged, verified by
      `crates/ork-integrations/tests/transaction.rs::rollback_drops_worktree`.
- [ ] `LocalTransactionCoordinator::finalize` with policy
      `RequestApproval` writes the diff and validation artifacts,
      opens a `HumanInputGate` request with the fixed schema, and
      returns `AwaitingApproval`, verified by
      `crates/ork-integrations/tests/transaction.rs::request_approval_opens_hitl`.
- [ ] `LocalTransactionCoordinator::resume_from_approval` with
      `Approve` runs the FF merge and records
      `Committed`, verified by
      `crates/ork-integrations/tests/transaction.rs::approval_approve_commits`.
- [ ] `LocalTransactionCoordinator::resume_from_approval` with
      `Reject` aborts with `ApprovalRejected`, verified by
      `crates/ork-integrations/tests/transaction.rs::approval_reject_rolls_back`.
- [ ] `LocalTransactionCoordinator::resume_from_approval` with
      `Edit` returns `OrkError::Validation` carrying the message
      `"approval edit not supported in v1"`, verified by
      `crates/ork-integrations/tests/transaction.rs::approval_edit_rejected_v1`.
- [ ] `LocalTransactionCoordinator::list_in_flight` returns rows
      where `closed_at IS NULL`, verified by
      `crates/ork-integrations/tests/transaction.rs::list_in_flight_filters_closed`.
- [ ] Engine arm `WorkflowEngine::run_transaction` walks `apply`,
      captures the diff, walks `validate`, calls `finalize`, and
      writes the resolution into the parent `StepResult`,
      verified by integration test
      `crates/ork-core/tests/transaction_smoke.rs::happy_path_commits`.
- [ ] Integration test
      `crates/ork-core/tests/transaction_smoke.rs::validation_failure_rolls_back`
      asserts: agent edits a file in `apply`, `validate` fails,
      worktree is removed, base branch is at the original commit,
      step is `Failed`.
- [ ] Integration test
      `crates/ork-core/tests/transaction_smoke.rs::approval_pause_resume_commits`
      asserts: validation fails, run enters `InputRequired`,
      `HumanInputGate::resolve(Approve, ...)` resumes the run,
      base branch advances, run completes.
- [ ] Integration test
      `crates/ork-core/tests/transaction_smoke.rs::fail_run_skips_join_policy`
      asserts: a transaction inside a `Map` with policy `FailRun`
      fails the whole run, not just the map item, even when the
      map's `on_item_failure = Continue`.
- [ ] Crash-recovery test
      `crates/ork-core/tests/transaction_resume.rs::aborts_applying_on_restart`
      seeds a `Applying`-phase row, restarts the engine, asserts
      the row transitions to `Aborted { EngineRestart }` and the
      worktree is cleaned up.
- [ ] Crash-recovery test
      `crates/ork-core/tests/transaction_resume.rs::reattempts_validated_pass_finalize`
      seeds a `Validated { Pass }`-phase row, restarts the engine,
      asserts the FF merge runs and the row transitions to
      `Committed`.
- [ ] Diff capture writes an `ArtifactStore` artifact named
      `tx-{transaction_id}.diff` under scope `(tenant_id, run_id,
      "transactions")` containing the unified diff against
      `base_commit`, verified by
      `crates/ork-integrations/tests/transaction.rs::captures_diff_artifact`.
- [ ] Audit events `audit.transaction.{opened, applied, validated,
      approval_requested, committed, aborted}` are emitted at the
      transitions documented in `Observability`, verified by
      `crates/ork-core/tests/transaction_observability.rs`.
- [ ] `cargo test -p ork-core workflow::transaction::` is green.
- [ ] `cargo test -p ork-integrations transaction::` is green.
- [ ] Public API documented in
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
      and re-exported from
      [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs).
- [ ] [`README.md`](README.md) ADR index row added for `0031` and
      decision-graph edges drawn from `0027`, `0028`, `0029`,
      `0030` to `0031` and from `0031` to `0022`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Autonomous coding workflows finally have a *correct-by-construction*
  validation gate. "Edit → run tests → commit or roll back" stops
  being a prompt convention and becomes an engine-enforced
  primitive that emits audit events and survives restarts.
- The all-or-nothing observation invariant (the base branch is
  either at `base_commit` or at the merge commit, never in
  between) closes the obvious correctness hole for unattended
  agent runs. Operators can grant a coding agent commit privileges
  on a working branch with an audit trail that is meaningfully
  reviewable.
- Composes cleanly with the existing pieces: ADR 0030 provides
  isolation, ADR 0028 provides the validation primitive, ADR 0029
  provides the edit primitive, ADR 0027 provides the human pause,
  ADR 0016 provides the artifact spill. This ADR is the smallest
  glue between them.
- Fast-forward-only semantics keep the merge surface trivial. No
  conflict resolution, no merge commits, no rebase policy — the
  transaction either lands cleanly or rolls back. Workflow
  authors who need richer reconciliation can compose an explicit
  merge step downstream once a future ADR ships.
- Crash-recovery is durable and conservative: the worst case on
  restart is a re-run of the apply phase, not a half-applied
  diff on `main`.

### Negative / costs

- We are committing to fast-forward-only landing. Long-running
  transactions on a hot branch will lose to concurrent activity
  and rollback through no fault of the agent. Workflow authors
  who hit this will reach for `Loop`-with-retry, which works but
  is the user-visible cost of the simplicity. A future ADR can
  add `merge --no-ff` or rebase semantics behind a separate
  step kind.
- The wire shape of `TransactionPhase` is now a long-lived
  contract: persisted rows from any prior run must keep parsing.
  Adding new phases requires backward-compatible serde defaults.
  This is the same constraint ADR 0027 accepted for HITL phases;
  we follow that pattern.
- Crash recovery aborts every in-flight transaction in the
  `Applying`/`Applied`/`Validating` phases. For a `Map` of 100
  transactions where 90 had finished apply and were mid-validate
  at restart, all 90 are dropped and re-run. This is correct, but
  it is wasteful. Mitigation: workflow authors who care wrap each
  transaction in `Loop { until: ..., max_iterations: 3 }`.
- The `Edit` approval decision is reserved but not implemented.
  Reviewers who want to land a different diff than the agent
  produced have to reject, edit out-of-band, and re-run. This is
  conservative on purpose for v1; a follow-up ADR can wire `Edit`
  through the coordinator.
- `update-ref` invocation bypasses `GitOperations` deliberately.
  This is a calculated exception to ADR 0030's "all git through
  the port" stance: the FF-merge primitive is policy-bearing and
  scoped to this ADR, and putting it in `GitOperations` would
  drag merge semantics into the read-only port surface most
  callers want. The exception is local to
  `LocalTransactionCoordinator` and called out in the audit log.
  If a future ADR (`push`, full `merge`) lands, this primitive
  migrates to `GitOperations` cleanly.
- Adversarial review surfaces a real concern: at 10× scale a
  `Map` of 32 transactions all targeting the same base branch
  funnels through a single FF-merge serialisation point. The
  coordinator does not implement an admission queue; concurrent
  finalize calls on the same `(repo, base_branch)` will mostly
  see CAS failures and roll back. This is correct (no silent
  overwrite) but throughput-limited. A queue is a follow-up.

### Neutral / follow-ups

- A separate ADR can add `merge_strategy: Squash | Rebase` to
  the transaction step once we have a use case that doesn't fit
  fast-forward. For now, every commit on the task branch
  fast-forwards onto base, preserving the agent's commit log
  intact.
- A separate ADR may introduce per-transaction RBAC scopes
  (`workflow:transaction:commit`, `workflow:transaction:approve`)
  on top of ADR 0021. The hooks exist (`required_scopes` in the
  approval payload) but the runtime check awaits 0021's
  `ScopeChecker`.
- The `Edit` approval payload is the obvious next iteration:
  a reviewer-supplied patch that replaces the agent's diff,
  re-validates against the validate steps, then commits. This is
  reserved in `ApprovalDecision` precisely so the wire format
  doesn't churn when v2 lands.
- Garbage collection of `Aborted` worktree directories outside
  the per-tenant cache root is the scheduler's job (ADR
  [0019](0019-scheduled-tasks.md)) — same posture as ADR 0030.

## Alternatives considered

- **Implement transaction semantics in the agent prompt.** Reject:
  unobservable, unenforced, and the failure mode is silent half-
  application of an edit when the LLM hallucinates "I rolled it
  back." This is exactly the failure the ADR exists to prevent.
- **Hand the apply/validate orchestration to a dedicated
  meta-agent (an `OrchestratorAgent` that calls `git_*` tools).**
  Reject: same pathology as the prompt-only design — policy in a
  prompt is policy that drifts. The engine is the right home for
  rollback policy because it is where every other lifecycle
  guarantee already lives (cancellation, retry, persistence).
- **Use ADR 0027 (`HumanInputGate`) for *all* failure paths.**
  Reject: HITL is the right tool for "should we proceed?" but the
  common case is "the tests failed, drop it." Forcing a human in
  the loop on every test failure makes the autonomous loop
  non-autonomous.
- **Make every workflow step transactional by default.** Reject:
  most steps don't mutate a worktree and a transactional wrapper
  is overhead (worktree allocation, diff capture, FF merge) for
  the common case. Opt-in via an explicit step kind keeps the
  hot path cheap.
- **Use `git stash` for rollback instead of worktree-drop.**
  Reject: `stash` mutates the index of the working tree it runs
  in, conflicts with parallel transactions sharing a checkout,
  and leaves a stash entry behind. Worktree-drop is genuinely
  zero-side-effect and is what ADR 0030 is *for*.
- **Use `git revert` after merge for "rollback".** Reject: this
  is rollback at the *commit* layer, not the *apply* layer.
  By the time a revert ships, the broken code has already been
  on the base branch and observed by other consumers. The whole
  point of this ADR is that broken code never lands.
- **Add `merge` to `GitOperations` (ADR 0030) instead of carving
  out a one-off `update-ref` exception in the coordinator.**
  Considered. Decided against for v1 because (a) `GitOperations`
  is a read-and-write-locally port that other callers consume
  for non-merging purposes; bolting a merge surface onto it
  invites every consumer to grow opinions about merge strategy,
  and (b) the FF-merge here is *policy*, not mechanism — it
  enforces the all-or-nothing invariant of this ADR specifically.
  When a richer merge surface ships, this primitive migrates to
  `GitOperations` cleanly.
- **Two-phase commit across multiple worktrees (a transaction
  that spans `repo-a` and `repo-b` atomically).** Reject for
  v1: the implementation is genuinely 2PC-shaped (prepare, vote,
  commit, with timeouts and a coordinator log) and is its own
  ADR's worth of design. v1 transactions are single-repo. A
  follow-up can introduce `MultiRepoTransaction` once the
  single-repo case is battle-tested.

## Affected ork modules

- New: `crates/ork-core/src/ports/transaction.rs` —
  `TransactionCoordinator` trait and supporting types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `transaction`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — `WorkflowStep::TransactionalCodeChange` variant,
  `OnTransactionFailure`, `ApprovalTimeoutPolicy`,
  `StepResult.transaction` optional field.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
  — lower into `CompiledNode::Transaction`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  — `run_transaction` arm, diff-capture helper,
  resolution-handling helper, in-flight sweep on startup,
  approval-resume integration with the existing HITL pubsub
  loop.
- New: `crates/ork-integrations/src/transaction.rs` —
  `LocalTransactionCoordinator`, FF-merge invocation through
  `ShellExecutor`, artifact-spill plumbing, HITL composition.
- New: `crates/ork-persistence/src/postgres/transaction_repo.rs`
  — `PostgresTransactionRepo` implementing
  `TransactionRepo` (the persistence facet of the coordinator).
- New: [`migrations/010_workflow_transactions.sql`](../../migrations/)
  — `workflow_transactions` table + index, and the additive
  `step_results.transaction_id` column.
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs)
  — boot a `LocalTransactionCoordinator` from existing
  `Arc<dyn GitOperations>` (ADR 0030),
  `Arc<dyn ShellExecutor>` (ADR 0028),
  `Arc<dyn ArtifactStore>` (ADR 0016),
  `Arc<dyn HumanInputGate>` (ADR 0027),
  `Arc<dyn TransactionRepo>` and inject it into the
  `WorkflowEngine`.
- [`config/default.toml`](../../config/default.toml) —
  `[engine.transaction]` block.
- [`workflow-templates/`](../../workflow-templates/) — reference
  YAML for `fix-this-bug` and `implement-this-ADR` rebuilt on
  top of `transactional_code_change` once the implementation
  lands (out of scope for this ADR; tracked under the
  implementation issue).
- [`crates/ork-webui/`](../../crates/ork-webui/) — render
  `transaction.diff` and `transaction.validation` artifacts
  inline on the `InputRequired` form when the active HITL
  request was opened by a transaction (the
  `human_input_request_id` join). Visual treatment is a Web UI
  detail; the schema-driven form already works without it.

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
| Aider | `--auto-commits` writes per edit on a task branch; no validation gate | `transactional_code_change` couples edits to validation and rolls back on failure |
| Claude Code (this CLI) | Manual diff inspection + `Bash` to run tests + `git commit` | Engine-enforced apply/validate/commit triple |
| OpenHands | Per-task container, branch-per-task, manual review | Worktree-per-transaction, FF merge on validate-pass, HITL pause on validate-fail |
| GitHub Copilot Workspace | Branch-per-task with structured diff and explicit "submit" gate | `transactional_code_change` step with `RequestApproval` policy |
| Database transactions (BEGIN/COMMIT/ROLLBACK) | Atomic visibility of mutations | Atomic visibility of base-branch state: at `base_commit` or at `merge_commit`, never partial |
| Solace Agent Mesh | No first-class transactional code-change primitive | `WorkflowStep::TransactionalCodeChange` |
| LangGraph checkpointing | Step-level state checkpoints with resume | `TransactionPhase` persisted to `workflow_transactions`, resumed on engine restart |

## Open questions

- **`Edit` approval decision wiring.** ADR 0027 reserves the
  `Edit` decision. This ADR rejects it in v1 because letting a
  reviewer paste a replacement diff requires a re-validation pass
  and conflict-handling against the agent's diff. A follow-up
  ADR will design this; the wire enum is shaped so the v2 change
  is additive.
- **Multi-repo transactions.** A workflow that needs to land
  coordinated changes across `ork` and `ork-webui` requires 2PC.
  Out of scope for v1; see Alternatives.
- **Admission queue for FF merge contention.** Concurrent
  transactions targeting the same base branch will mostly roll
  back via CAS failure. Acceptable for v1; a follow-up may add
  per-`(repo, base_branch)` serialisation in the coordinator if
  contention becomes a real cost.
- **Auto-rollback during apply on apply-step failure.** Today, a
  failure inside `apply:` calls `abort(ApplyFailed)`
  unconditionally. Should `on_failure = RequestApproval` apply
  to apply-step failures too (e.g. surface the partial diff to a
  human), or stay validate-only? Default proposal: validate-only
  for v1, since an apply failure means the agent itself signalled
  inability to proceed. Revisit if real workflows want it.
- **Push integration.** This ADR commits to a *local* base branch.
  Publishing the result (open a PR, push to a remote) is out of
  scope; the same `push`-gating ADR that ADR 0030 anticipates
  will compose with this one.
- **Deterministic task-branch naming under retries.** The default
  template `ork/run-{run_id}/tx-{step_id}` collides with itself
  on a `Loop` retry of the same step. The Loop is expected to
  override `task_branch_template` to include `{iteration}`; the
  engine could synthesise this. Decided to defer the auto-suffix
  to implementation, but flag it in the open questions so the
  reviewer catches it if the implementation forgets.
- **GC of orphan worktree directories on disk.** If a row is
  marked `Aborted` but the on-disk worktree wasn't actually
  removed (e.g. crash between rollback and `close_workspace`),
  the directory leaks. ADR 0019's scheduler will sweep
  `_tasks/<tenant>/` against `workflow_transactions.workspace_root
  WHERE closed_at IS NOT NULL`. Out of scope here.

## References

- ADR [`0016`](0016-artifact-storage.md) — diff and validation
  artifact spill.
- ADR [`0018`](0018-dag-executor-enhancements.md) — workflow step
  enum, recursive walker, cancellation token.
- ADR [`0022`](0022-observability.md) — audit events and OTLP
  spans.
- ADR [`0027`](0027-human-in-the-loop.md) — `HumanInputGate` for
  the `RequestApproval` policy.
- ADR [`0028`](0028-shell-executor-and-test-runners.md) —
  `ShellExecutor`, `run_tests`, `TestSummary`.
- ADR [`0029`](0029-workspace-file-editor.md) —
  `WorkspaceHandle`, `WorkspaceEditor`.
- ADR [`0030`](0030-git-operations.md) — `GitOperations`,
  worktree provisioning, `close_workspace`.
- `git update-ref`:
  <https://git-scm.com/docs/git-update-ref>.
- `git merge --ff-only` semantics:
  <https://git-scm.com/docs/git-merge#_fast_forward_merge>.
- LangGraph checkpointing:
  <https://langchain-ai.github.io/langgraph/concepts/persistence/>.
