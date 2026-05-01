# 0041 — Nested workspaces and sub-worktree coordination

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0006, 0007, 0016, 0020, 0022, 0028, 0029, 0030, 0037, 0040, 0044, 0045
- **Supersedes:** —

## Context

ADRs [0028](0028-shell-executor-and-test-runners.md),
[0029](0029-workspace-file-editor.md), and
[0030](0030-git-operations.md) establish a per-run, per-tenant working
copy: `LocalGitOperations::open_workspace` allocates one
`WorkspaceHandle` (defined in
[`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs))
per workflow run, materialised as a `git worktree` under
`<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/`. ADR 0030 already
nods at parallel branches by saying "the executor opens **one
workspace per parallel sub-step**", but it deliberately stops there:
the parent–child relationship between those workspaces is not
modelled, the branch-naming convention is not standardised, and the
merge-back semantics are explicitly deferred.

That gap blocks two real workloads now in flight:

1. **Coding teams (ADR 0045).** A team-shaped agent topology — an
   orchestrator that spawns sub-agents to take parallel slices of one
   coding task — needs each sub-agent to have its own working copy
   branched off a shared trunk, with a tracked relationship back to
   that trunk. Without that, two sub-agents that touch the same file
   race; with it, every sub-agent's diff is independent and
   captured cleanly.
2. **Aggregation (ADR 0044).** The aggregator that combines
   sub-agent outputs into a single review-ready diff needs each
   sub-agent's contribution captured as a discrete artifact at the
   moment that sub-agent reports completion. Today the diff lives
   only in the sub-worktree's working tree; if that worktree is
   garbage-collected on cancellation or TTL, the work is lost.

Peer delegation (ADR [0006](0006-peer-delegation.md)) and the remote
agent client (ADR [0007](0007-remote-a2a-agent-client.md)) already
give us the *control plane* for spawning sub-agents over A2A. What
they don't give us is the *workspace plane*: the agreement between
orchestrator and sub-agent about "here is your isolated checkout,
here is the branch I expect you to push to, here is what counts as
'your work' when you report `Completed`." Without that agreement,
every team-style workflow re-invents the convention in prompt text,
which is exactly the failure mode ADRs 0028–0030 worked to remove.

This is also a feature that single-process coding harnesses (Aider,
opencode, Claude Code) **cannot** offer: they assume one human at
one terminal driving one working copy. ork is an A2A orchestration
platform; modelling the parent/child workspace relationship is one
of the things that's only worth doing in our shape.

The repo-map (ADR [0040](0040-repo-map.md)) and LSP-diagnostics (ADR
[0037](0037-lsp-diagnostics.md)) ports also already key their caches
on `WorkspaceHandle.id`. As soon as more than one handle exists per
run, those caches need to know the parent/child relationship so
they can share index data across siblings rather than rebuilding
from scratch.

## Decision

ork **introduces hierarchical workspaces**: a `WorkspaceHandle`
gains an optional `parent: Option<WorkspaceId>`, every non-trunk
handle is a `git worktree` branched off its parent's branch, and
the lifecycle (allocate → edit in isolation → capture diff at
sub-agent completion → drop) is owned by a new
`SubWorkspaceCoordinator` port in `ork-core` whose default
implementation lives next to `LocalGitOperations` in
`ork-integrations`.

The decision splits into six load-bearing pieces:

1. **Hierarchy on `WorkspaceHandle`** — one new field, one new
   invariant: a handle either has `parent = None` (a *trunk*
   worktree, what ADR 0030 already provisions) or a `parent =
   Some(WorkspaceId)` pointing at another live handle on the same
   `(tenant_id, run_id, repo)`.
2. **Allocator** — `SubWorkspaceCoordinator::open_sub` is invoked
   by the delegation path (ADR 0006 `delegate_to` step, ADR 0007
   remote-agent send) when the sub-task is itself a *coding*
   sub-task carrying a `WorkspaceRef` in its A2A message metadata.
   It chains `LocalGitOperations::open_workspace` with the parent's
   branch as the base.
3. **Branch and filesystem naming** — deterministic and tenant-scoped:
   `ork/run-<run_id>/<task_branch>/sub-<subtask_id>` for the branch,
   `<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/<handle_id>/` for
   the worktree root. No nesting on disk; siblings live as flat
   directories.
4. **Remote sub-agents get a tar snapshot via A2A `FilePart`**, with
   ADR 0016 spillover for large workspaces, and the orchestrator
   keeps a local *mirror* sub-worktree so diff-capture stays uniform
   regardless of where the sub-agent runs. The `git-clone-from-Kong`
   and `shared-FS` alternatives are deferred to follow-up ADRs.
5. **Merge-back protocol** — when a sub-agent's A2A task transitions
   to `Completed` (ADR [0008](0008-a2a-server-endpoints.md)), the
   coordinator runs `git diff <parent.head_commit>..<sub.head_commit>`
   in the sub-worktree, persists the patch as an ADR-0016 artifact
   named `subtask-<subtask_id>.patch`, and emits a
   `workspace.sub.merge_capture` event. The patch is the contract
   handed to ADR 0044; this ADR does not aggregate.
6. **Concurrency posture** — *no* file-level locking. Sub-worktrees
   are isolated by construction; semantic conflicts (two siblings
   editing the same file) are surfaced at merge time by ADR 0044, not
   at edit time. We document this loudly so workflow authors don't
   reach for cross-worktree locks that this ADR refuses to provide.

### `WorkspaceHandle` extension

```rust
// crates/ork-core/src/ports/workspace.rs   (extends ADR 0029)

#[derive(Clone, Debug)]
pub struct WorkspaceHandle {
    pub id: WorkspaceId,
    pub tenant_id: TenantId,
    pub run_id: RunId,
    pub repo: String,
    pub root: PathBuf,
    pub head_commit: String,

    // NEW (ADR 0041)
    pub parent: Option<WorkspaceId>,        // None = trunk; Some = sub
    pub branch: String,                     // canonical branch name
    pub subtask_id: Option<String>,         // None on trunk; Some on sub
    pub origin: WorkspaceOrigin,            // Local | RemoteMirror
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceOrigin {
    /// The owning sub-agent runs in this process (LocalAgent, ADR 0002).
    Local,
    /// The owning sub-agent runs over A2A (RemoteAgent, ADR 0007).
    /// This handle is the orchestrator's mirror; the canonical edits
    /// happen on the remote side and arrive as a patch on completion.
    RemoteMirror,
}
```

The new fields are *additive*: ADR 0030's `open_workspace` continues
to return a handle with `parent = None`, `subtask_id = None`,
`origin = Local`, and `branch` populated from the request that ADR
already carries. Existing call-sites compile unchanged once the
defaults are wired in.

### `SubWorkspaceCoordinator` port

```rust
// crates/ork-core/src/ports/sub_workspace.rs   (new file)

use async_trait::async_trait;
use ork_common::error::OrkError;

use crate::ports::workspace::{WorkspaceHandle, WorkspaceId};

#[async_trait]
pub trait SubWorkspaceCoordinator: Send + Sync {
    /// Allocate a sub-worktree branched from `parent`. The branch
    /// name is derived from `parent.branch` and `subtask_id` per the
    /// naming rules in §"Branch and filesystem naming". Idempotent
    /// per `(parent.id, subtask_id)`: a re-call returns the existing
    /// child handle.
    async fn open_sub(
        &self,
        parent: &WorkspaceHandle,
        req: OpenSubRequest,
    ) -> Result<WorkspaceHandle, OrkError>;

    /// Materialise an A2A-shippable workspace bundle for a remote
    /// sub-agent. Tars `child.root` (filtered by `req.include_paths`,
    /// excluding `target/`, `node_modules/`, `.git/worktrees/`),
    /// stages the bytes through ADR 0016 if larger than the inline
    /// FilePart cap, and returns a `WorkspaceBundle` with either an
    /// inline `Part::File { Bytes }` or a `Part::File { Uri }`.
    async fn export_for_remote(
        &self,
        child: &WorkspaceHandle,
        req: ExportRequest,
    ) -> Result<WorkspaceBundle, OrkError>;

    /// Apply a unified diff produced by a remote sub-agent into the
    /// orchestrator's mirror sub-worktree, then return the new HEAD
    /// commit. Used by the merge-back path when the canonical edits
    /// happened on the remote side.
    async fn import_remote_patch(
        &self,
        child: &WorkspaceHandle,
        diff: &str,
    ) -> Result<String /* new head commit */, OrkError>;

    /// Capture the sub-worktree's diff against its parent's
    /// `head_commit` as an ADR-0016 artifact. Idempotent per
    /// `(child.id, child.head_commit)`: a second call with an
    /// unchanged head returns the existing artifact ref. Emits
    /// `workspace.sub.merge_capture` (ADR 0022).
    async fn capture_diff(
        &self,
        child: &WorkspaceHandle,
    ) -> Result<SubDiffArtifact, OrkError>;

    /// Tear down a sub-worktree. Refuses with
    /// `OrkError::Validation("uncaptured diff")` if the worktree has
    /// modifications since the last `capture_diff` and `force` is
    /// false. On success emits `workspace.sub.drop`.
    async fn close_sub(
        &self,
        child: &WorkspaceHandle,
        force: bool,
    ) -> Result<(), OrkError>;

    /// Sweep sub-worktrees whose `run_id` is in a terminal state but
    /// whose handle is still on disk (orphaned by a crashed
    /// orchestrator). Captures the diff if it has not already been
    /// captured, then drops the worktree. Emits
    /// `workspace.sub.orphan_swept` per sweep.
    async fn sweep_orphans(
        &self,
        tenant_id: TenantId,
    ) -> Result<SweepReport, OrkError>;
}

#[derive(Debug, Clone)]
pub struct OpenSubRequest {
    pub subtask_id: String,                 // ULID; URL-safe
    pub origin: WorkspaceOrigin,
    /// Optional TTL after which `sweep_orphans` will reclaim this
    /// sub-worktree even if the parent run is still active. Defaults
    /// to `LocalSubWorkspaceConfig::default_sub_ttl`.
    pub ttl: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    /// Empty = export the whole worktree. Non-empty = restrict to
    /// these path prefixes (relative to `child.root`).
    pub include_paths: Vec<String>,
    /// Hard cap on inline bytes; output above this spills to ADR 0016.
    pub inline_cap_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct WorkspaceBundle {
    pub child: WorkspaceId,
    pub base_commit: String,                // = parent.head_commit at export
    pub bytes: Option<Vec<u8>>,             // Some => inline FilePart
    pub artifact_uri: Option<String>,       // Some => spilled FilePart
    pub mime_type: &'static str,            // "application/x-tar+gzip"
    pub byte_size: u64,
}

#[derive(Debug, Clone)]
pub struct SubDiffArtifact {
    pub child: WorkspaceId,
    pub parent: WorkspaceId,
    pub base_commit: String,                // parent.head_commit
    pub head_commit: String,                // child.head_commit at capture
    pub artifact_id: String,                // ADR 0016 id
    pub patch_bytes: u64,
    pub files_changed: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    pub captured: Vec<SubDiffArtifact>,     // diffs we managed to save
    pub dropped: Vec<WorkspaceId>,
    pub failures: Vec<(WorkspaceId, String)>,
}
```

### Branch and filesystem naming

| Handle  | Branch                                                    | Worktree root                                                         |
| ------- | --------------------------------------------------------- | --------------------------------------------------------------------- |
| trunk   | `ork/run-<run_id>/<task_branch>` (set by ADR 0030)        | `<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/<handle_id>/`         |
| sub     | `ork/run-<run_id>/<task_branch>/sub-<subtask_id>`         | `<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/<handle_id>/`         |

Three properties hold by construction:

1. **Flat layout on disk.** Sub-worktrees are siblings of the trunk
   under `<repo>/`, addressed by their own `WorkspaceId`. Git refuses
   to nest a worktree inside another worktree's tree, and we have no
   reason to fight that — addressing by handle id keeps the parent
   relationship in metadata, not in directory structure.
2. **Branch name encodes the chain.** `ork/run-<run_id>/<task_branch>`
   for the trunk; `…/sub-<subtask_id>` appended per child. A nested
   sub-of-sub (orchestrator delegating to a sub-orchestrator) appends
   another `/sub-<subtask_id_2>` segment. Git allows arbitrary slashes
   in branch names and `git worktree add` cares only that the leaf is
   unique; the chain is human-readable in logs and CI tooling.
3. **Tenant scoping is unchanged from ADR 0029/0030.**
   `<cache_dir>/_tasks/<tenant_id>/...` remains the only path the
   coordinator will write under, asserted by the same canonicalisation
   guard `LocalGitOperations` already runs.

### Remote sub-agent transport — chosen and rejected paths

Remote A2A sub-agents (ADR 0007) cannot read the orchestrator's
filesystem. We considered three transport options:

| Option                                              | Pros                                                                                         | Cons                                                                                                                               | Decision                                          |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| **(a) Tar snapshot via A2A `FilePart`**             | Uses the existing A2A wire model unchanged (ADR 0003 `Part::File { Bytes \| Uri }`). Works through any transport (Kong, Kafka, vendor mesh). Symmetric with ADR 0016 spill for large bundles. No new infrastructure. | Each sub-task pays the cost of one tar build at export time. No incremental sync — the remote agent gets a snapshot, not a live view. Multi-turn sub-agents must re-tar if base changes.                                                                | **Chosen for v1.**                                |
| (b) Read-only git clone over Kong                   | Incremental (`git fetch`). Sub-agent gets full history. Push-back is the same protocol.      | Requires a tenant-scoped `git http-backend` mounted under Kong with auth, ACLs, and a serving root that is *only* the tenant's `_tasks/` subtree. Multi-week project; needs its own ADR (RBAC scopes for git endpoints, credential rotation, etc.). | Deferred to a follow-up ADR. Port surface here is intentionally compatible: a future `export_for_remote` impl can return a `Part::File { Uri }` pointing at the git endpoint instead of a tar URI without changing call-sites. |
| (c) Shared filesystem assumption (NFS, hostPath)    | Zero serialisation cost; sub-agent edits the same bytes the orchestrator sees.               | Only works when orchestrator and sub-agent run on the same host or the same NFS volume. Breaks the "remote sub-agent could be anywhere in the mesh" invariant ADR 0007 sets up. Also weakens the tenant isolation story in ADR 0020 (one tenant's NFS export is one mistake away from another tenant's data). | Rejected. May be re-introduced as a *performance* opt-in for in-cluster deployments after ADR 0044 stabilises and the cost of tar serialisation actually bites. |

**Why tar wins for v1**: it's the only option that lets us ship sub-
worktree coordination *without* simultaneously shipping a new
public-facing endpoint family. A2A FileParts already round-trip
through Kong + ork-api; we get re-use of all of ADR 0009's payload
auth, ADR 0020's tenant frame, and ADR 0016's spill-to-storage. The
performance ceiling (a tar per sub-task export) is high enough for
the sub-task sizes ADR 0045 envisions (single crate, single feature,
typically <5 MB compressed); the day a real workload hits the
ceiling, option (b) is the right answer.

### Mirror sub-worktree for remote sub-agents

When a sub-agent runs remotely, the orchestrator still allocates a
local sub-worktree with `origin = RemoteMirror`. The mirror is
where the canonical *post-merge-back* state lives:

1. `open_sub(parent, { origin: RemoteMirror, ... })` allocates the
   mirror exactly as for a local sub-agent.
2. `export_for_remote(child)` builds the tar from the mirror's tree;
   the remote agent receives bytes-on-the-wire.
3. The remote agent does its work in its own filesystem (its choice
   how to host the tar — extract to a temp dir, mount in a sandbox,
   feed to a coding harness like Aider).
4. On A2A `Completed`, the remote agent returns the unified diff in
   a `Part::File { Bytes \| Uri }` part of the result message.
5. Orchestrator calls `import_remote_patch(child, diff)`, which
   applies the patch to the *mirror* sub-worktree and updates its
   `head_commit`.
6. `capture_diff(child)` then runs against the mirror exactly as it
   would for a local sub-agent.

This keeps the merge-back contract uniform: in both cases ADR 0044
receives a `SubDiffArtifact` produced by `capture_diff`. The only
local-vs-remote variance is *who put the bytes into the
sub-worktree*; once they're there, the protocol is identical.

### Concurrency posture

The repository deliberately does **not** introduce any cross-worktree
file-level locking. Two sub-worktrees on the same `(tenant, run,
repo)` can edit the same path freely; their edits are isolated by
the working tree boundary that `git worktree` provides.

Two consequences flow from that:

1. **Semantic conflicts are deferred to merge time.** If sub-A and
   sub-B both edit `crates/ork-core/src/lib.rs`, neither edit fails
   at `WorkspaceEditor::update_file` time — the optimistic-hash check
   from ADR 0029 is *intra-worktree only*. The conflict surfaces when
   ADR 0044 tries to apply both `SubDiffArtifact`s onto the trunk and
   one of the patches rejects.
2. **Workflow authors who want to serialise must do so explicitly.**
   The DAG executor (ADR [0018](0018-dag-executor-enhancements.md))
   already supports sequencing; expressing "sub-task B depends on
   sub-task A's merge" is a DAG decision, not a workspace decision.
   This ADR refuses requests for a `lock_path(...)` API on the
   coordinator: it would re-introduce the head-of-line blocking
   problem ADR 0029's `Concurrent-write safety` section explicitly
   rejected.

The repo-map (ADR 0040) and LSP-diagnostics (ADR 0037) caches read
the new `parent` field to share parent index data with siblings (the
trunk's repo-map is reused as the seed for each child's incremental
update). This is a perf optimisation enabled by hierarchy, not a
correctness requirement; ADRs 0037/0040 are unchanged in interface.

### Cleanup invariants

The coordinator enforces the following invariants on every
`close_sub` and `sweep_orphans`:

1. **No drop without capture.** A sub-worktree whose
   `head_commit` has advanced past `parent.head_commit` and which
   has not had `capture_diff` called for that head **cannot** be
   dropped. `close_sub(force = false)` returns
   `OrkError::Validation("uncaptured diff")`. `force = true` may
   override this; the executor logs an `audit.workspace_force_drop`
   event with the diff that was discarded (mirroring the
   `git checkout --force` audit posture in ADR 0030).
2. **Capture before terminal-state cleanup.** When the parent run
   transitions to a terminal state (Completed / Failed / Cancelled),
   the coordinator captures the diff for every sub-worktree that
   still has uncaptured changes *before* the standard
   `WorkspaceHandle` GC kicks in. The artifact is preserved even
   if the run failed; ADR 0044 may decide to discard it but the
   bytes are not lost.
3. **Orphan sweep is mandatory and event-driven.**
   `sweep_orphans(tenant_id)` runs on:
   - ork-api startup (sweep all tenants).
   - A periodic schedule registered via ADR
     [0019](0019-scheduled-tasks.md) (default: every 15 minutes).
   - The "run terminal state" hook above.
   It identifies sub-worktrees whose `parent.run_id` is in a terminal
   state per the runs table and processes them through invariant (2).
4. **Per-tenant root containment.** All cleanup paths re-canonicalise
   the worktree root and refuse to descend outside
   `<cache_dir>/_tasks/<tenant_id>/`. A symlink whose target escapes
   the tenant root causes the sweeper to skip and emit a
   `workspace.sub.sweep_skipped` event with `reason = "path_escape"`.

### Observability

Every coordinator state transition emits a structured event consumed
by ADR [0022](0022-observability.md)'s tracing pipeline and by the
`a2a_task_events` log:

| Event                              | When                                                    | Key fields                                                                                          |
| ---------------------------------- | ------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `workspace.sub.create`             | `open_sub` returns Ok                                   | `tenant_id`, `run_id`, `parent_id`, `child_id`, `subtask_id`, `branch`, `origin`                    |
| `workspace.sub.export`             | `export_for_remote` returns Ok                          | `child_id`, `byte_size`, `spilled` (bool), `artifact_uri` (if spilled)                              |
| `workspace.sub.import_patch`       | `import_remote_patch` returns Ok                        | `child_id`, `head_commit_before`, `head_commit_after`, `patch_bytes`                                |
| `workspace.sub.merge_capture`      | `capture_diff` returns Ok                               | `child_id`, `parent_id`, `base_commit`, `head_commit`, `artifact_id`, `files_changed`               |
| `workspace.sub.drop`               | `close_sub` returns Ok                                  | `child_id`, `forced` (bool)                                                                         |
| `workspace.sub.orphan_swept`       | `sweep_orphans` reclaimed a stale handle                | `child_id`, `parent_run_id`, `had_uncaptured_diff` (bool)                                           |
| `workspace.sub.sweep_skipped`      | `sweep_orphans` declined to act on a candidate          | `child_id`, `reason`                                                                                |
| `audit.workspace_force_drop`       | `close_sub(force = true)` discarded uncaptured changes  | `child_id`, `discarded_diff_bytes`                                                                  |

These events ride the existing `tracing` → OTLP → `a2a_task_events`
pipeline; this ADR adds no new sinks.

## Acceptance criteria

- [ ] `WorkspaceHandle` in
      [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
      gains the fields `parent: Option<WorkspaceId>`, `branch: String`,
      `subtask_id: Option<String>`, `origin: WorkspaceOrigin`. Enum
      `WorkspaceOrigin { Local, RemoteMirror }` defined in the same
      module.
- [ ] `LocalGitOperations::open_workspace` (ADR 0030) is updated to
      populate `parent = None`, `subtask_id = None`,
      `origin = Local`, `branch = ork/run-<run_id>/<task_branch>` —
      verified by
      `crates/ork-integrations/tests/git_worktree.rs::trunk_handle_has_no_parent`.
- [ ] Trait `SubWorkspaceCoordinator` defined at
      `crates/ork-core/src/ports/sub_workspace.rs` with the signature
      shown in `Decision`, plus the supporting types `OpenSubRequest`,
      `ExportRequest`, `WorkspaceBundle`, `SubDiffArtifact`,
      `SweepReport`. Re-exported from
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs).
- [ ] `LocalSubWorkspaceCoordinator` implements the port at
      `crates/ork-integrations/src/sub_workspace.rs`, constructed from
      an `Arc<dyn GitOperations>` (ADR 0030), an
      `Arc<dyn ArtifactStore>` (ADR 0016), and
      `LocalSubWorkspaceConfig { default_sub_ttl, sweep_interval,
      tar_inline_cap_bytes, exclude_globs }`.
- [ ] `open_sub` produces a sub-worktree with branch
      `ork/run-<run_id>/<task_branch>/sub-<subtask_id>` and root
      `<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/<handle_id>/`,
      verified by
      `crates/ork-integrations/tests/sub_workspace_alloc.rs::naming_and_layout`.
- [ ] `open_sub` is idempotent for `(parent.id, subtask_id)`,
      verified by
      `crates/ork-integrations/tests/sub_workspace_alloc.rs::open_is_idempotent`.
- [ ] `open_sub` rejects when `parent` is itself dropped or its
      worktree is missing on disk, verified by
      `crates/ork-integrations/tests/sub_workspace_alloc.rs::refuses_dead_parent`.
- [ ] Two parallel `open_sub` calls under the same parent succeed
      with isolated trees, and concurrent edits to the same relative
      path do not interfere, verified by
      `crates/ork-integrations/tests/sub_workspace_isolation.rs::siblings_do_not_race`.
- [ ] `export_for_remote` produces a tar+gzip bundle with the
      worktree contents minus `.git/worktrees/`, `target/`,
      `node_modules/`, and any path matched by
      `LocalSubWorkspaceConfig::exclude_globs`, verified by
      `crates/ork-integrations/tests/sub_workspace_export.rs::tar_excludes_build_artifacts`.
- [ ] `export_for_remote` spills bundles larger than
      `tar_inline_cap_bytes` to `ArtifactStore::put` and returns
      `WorkspaceBundle { bytes: None, artifact_uri: Some(_), .. }`,
      verified by
      `crates/ork-integrations/tests/sub_workspace_export.rs::spills_oversize_bundle`.
- [ ] `import_remote_patch` applies the diff atomically against the
      mirror sub-worktree, updates `head_commit`, and rejects with
      `OrkError::Validation` when the patch does not apply cleanly
      against the recorded `base_commit`, verified by
      `crates/ork-integrations/tests/sub_workspace_remote.rs::round_trip_apply`
      and `…::refuses_stale_patch`.
- [ ] `capture_diff` runs `git diff <parent.head_commit>..<HEAD>`
      inside the sub-worktree, persists the patch through
      `ArtifactStore::put` under the name
      `subtask-<subtask_id>.patch`, and returns a
      `SubDiffArtifact` whose `artifact_id` resolves back to the
      same bytes via `ArtifactStore::get`, verified by
      `crates/ork-integrations/tests/sub_workspace_capture.rs::captures_against_parent`.
- [ ] `capture_diff` is idempotent for the same `(child.id,
      child.head_commit)`, verified by
      `crates/ork-integrations/tests/sub_workspace_capture.rs::idempotent_for_unchanged_head`.
- [ ] `close_sub(force = false)` rejects with
      `OrkError::Validation("uncaptured diff")` when
      `head_commit > parent.head_commit` and no `capture_diff` call
      has been made for that head, verified by
      `crates/ork-integrations/tests/sub_workspace_lifecycle.rs::refuses_drop_without_capture`.
- [ ] `close_sub(force = true)` succeeds, emits
      `audit.workspace_force_drop` with `discarded_diff_bytes` set,
      and removes the worktree on disk, verified by
      `crates/ork-integrations/tests/sub_workspace_lifecycle.rs::force_drop_audits_and_removes`.
- [ ] `sweep_orphans` finds sub-worktrees whose `parent.run_id` is
      in a terminal state, captures their diff, and removes them,
      verified by
      `crates/ork-integrations/tests/sub_workspace_sweep.rs::reclaims_after_terminal`.
- [ ] `sweep_orphans` skips and emits `workspace.sub.sweep_skipped`
      for any worktree whose canonical root escapes
      `<cache_dir>/_tasks/<tenant_id>/`, verified by
      `crates/ork-integrations/tests/sub_workspace_sweep.rs::refuses_path_escape`.
- [ ] The delegation step (ADR 0006) is extended so a `delegate_to`
      step whose target carries a `coding` skill (ADR 0045 vocabulary,
      checked by skill name prefix `coding.*` in v1) calls
      `SubWorkspaceCoordinator::open_sub` on the parent's
      `WorkspaceHandle` before invoking the target. The child handle
      flows in `AgentContext.workspace`. Verified by
      `crates/ork-core/tests/workflow/delegate_sub_workspace.rs::delegated_coding_step_has_sub`.
- [ ] The remote agent client (ADR 0007) calls
      `SubWorkspaceCoordinator::export_for_remote` and includes the
      resulting `WorkspaceBundle` as a `Part::File` in the outbound
      A2A message; on `Completed` it pulls the diff `Part::File` and
      calls `import_remote_patch` followed by `capture_diff`, verified
      by `crates/ork-agents/tests/remote_sub_workspace.rs::tar_round_trip`.
- [ ] Each coordinator method emits the events listed in
      `§Observability` through `tracing::event!` at `INFO` level with
      the documented field set, verified by
      `crates/ork-integrations/tests/sub_workspace_events.rs::emits_full_event_taxonomy`.
- [ ] `cargo test -p ork-integrations sub_workspace::` is green.
- [ ] `cargo test -p ork-core ports::sub_workspace::` is green.
- [ ] `cargo test -p ork-agents remote_sub_workspace::` is green.
- [ ] [`README.md`](README.md) ADR index row added for `0041`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Coding teams (ADR 0045) get the workspace-plane primitive they
  need: each sub-agent starts from a known branch, edits in
  isolation, and produces a diff artifact at completion. Without
  this, the team topology either races on shared trees or
  re-implements the convention in prompt text.
- Aggregation (ADR 0044) consumes a uniform `SubDiffArtifact`
  shape regardless of where the sub-agent ran. The local/remote
  asymmetry is fully hidden behind the coordinator.
- Diffs are captured *at completion*, not at GC time. A run that is
  cancelled mid-flight still preserves whatever each sub-agent had
  finished, which is the right default for a system whose users will
  ctrl-C things they're unsure about.
- Reuses ADR 0030's worktree mechanism, ADR 0016's artifact spill,
  ADR 0029's sandbox guards, and ADR 0022's event pipeline. There is
  no new infrastructure introduced — this ADR is a coordinator
  layered on top of primitives that already exist.
- Branch names encode the parent/child chain, so an operator looking
  at `git branch -a` on the underlying repo can read the topology of
  any in-flight team run without consulting ork's database.
- Single-process coding harnesses (Aider, opencode, Claude Code)
  cannot offer this: the parent/child workspace relationship is one
  of the few features that's *only* worth doing inside an A2A
  orchestrator. ork gets a real differentiator at low marginal cost.

### Negative / costs

- One worktree per sub-agent multiplies disk usage. ADR 0029 already
  has the per-run-tree pile-up; this ADR makes the pile O(parallel
  sub-agents per run), not O(runs). The orphan sweeper is
  load-bearing — if it fails, disk fills. We mitigate by running it
  on every ork-api startup *and* on a 15-minute scheduled cadence;
  failure to sweep is a paging-class issue once tenants share a
  volume.
- Tar serialisation is an O(worktree size) cost paid per remote
  sub-agent allocation. For sub-tasks scoped to one crate
  (~5 MB compressed) this is sub-second; for whole-repo sub-tasks it
  could climb into double-digit seconds. Workflow templates that fan
  out to remote sub-agents on a 1 GB repo will feel this; the right
  fix is option (b) (git-over-Kong), and the port shape here is
  designed to absorb that swap when it lands.
- Sub-worktree allocation pays `git worktree add -b` per sub-agent
  (50–200 ms warm). A 16-way fan-out costs ~1–3 s of pure allocation
  before any sub-agent does work. Acceptable for v1; if it bites,
  ADR 0030's "pre-allocate a worktree pool" idea applies here too.
- Two delegation paths (`delegate_to` step, `agent_call` tool from
  ADR 0006) both need the sub-workspace allocator wired. We must
  remember to test both — only the step path is in the acceptance
  criteria above; the tool path lands as part of ADR 0045 because
  that's where the orchestrator persona that uses `agent_call` for
  coding sub-tasks is introduced.
- Force-drop-with-discarded-diff is a real escape hatch. Operators
  who paper over uncaptured-diff errors with `force = true` can
  silently lose work; the audit event is the only paper trail. ADR
  [0021](0021-rbac-scopes.md) should require an elevated scope
  (`workspace:sub:force_drop`) to make this a deliberate choice
  rather than the default panic button.
- The "no cross-worktree locking" stance pushes semantic-conflict
  handling onto ADR 0044. If 0044's merge strategy is naive (e.g. a
  3-way merge with no conflict-resolution agent), team workflows
  that fan out two sub-agents into the same file will fail at
  merge time. That's the right place to fail — better than silently
  losing one sub-agent's work — but it raises the bar for ADR 0044.
- Branch names get long fast: a 4-deep nested team produces
  `ork/run-<26 chars>/<task>/sub-<26>/sub-<26>/sub-<26>/sub-<26>` ≈
  170 chars. Most filesystems and remotes are fine; the GitHub UI
  truncates. Real-world teams stay 1–2 levels deep, so this is a
  noticed-but-acceptable cost.

### Neutral / follow-ups

- ADR 0044 (sub-agent diff aggregation) consumes the
  `SubDiffArtifact` shape. The schema is set here so 0044 can be
  written against a stable input.
- A follow-up "git-over-Kong" ADR can replace the tar transport
  with an incremental `git fetch`. The coordinator's `WorkspaceBundle`
  already accommodates a `Part::File { Uri }` return; only the
  `LocalSubWorkspaceCoordinator` impl changes.
- A follow-up "shared-FS for in-cluster sub-agents" ADR can add a
  third `WorkspaceOrigin::SharedMount` variant whose
  `export_for_remote` returns a `Part::Data` pointing at the
  pre-mounted path. The protocol seam is the same.
- ADRs 0037 and 0040 (LSP / repo-map caches keyed per worktree) read
  the new `parent` field to share index data across sibling
  sub-worktrees. The change is small (one field lookup, parent's
  cache as the seed) but should be tracked in those ADRs'
  follow-up sections.
- ADR 0019 (scheduled tasks) gains one new schedule registration:
  `subworkspace.sweep_orphans` at 15-minute cadence. The
  registration lands with this ADR's implementation, not 0019's.
- ADR 0021 should add the scope vocabulary
  `workspace:sub:open`, `workspace:sub:capture`,
  `workspace:sub:force_drop`. Reserved here, enforced by 0021.

## Alternatives considered

- **Flat workspaces, no parent/child relationship.** Reject: the
  existing ADR 0030 model (one worktree per parallel sub-step)
  *works* but reinvents the convention in workflow YAML every time
  and provides no place to capture the diff per sub-task. The
  hierarchical relationship is what lets ADR 0044 know which
  sub-agent contributed which patch.
- **Make every sub-agent open its own trunk worktree from the read
  cache.** Reject: it duplicates the `git worktree add` cost (and
  the disk space, since trunks always check out the full base
  branch), loses the `parent.head_commit` reference that
  `capture_diff` keys against, and makes "branched off this team's
  trunk" something the orchestrator must communicate via prompt
  text rather than via branch lineage.
- **In-process file-level locks across sibling sub-worktrees.**
  Rejected for the same reason ADR 0029 rejected per-handle locking:
  it introduces head-of-line blocking without solving the actual
  problem (semantic conflict between two sub-agents' edits, which is
  a merge-time decision, not a write-time one). Workflow authors
  who need ordering use the DAG.
- **Tar via A2A `FilePart` for *every* sub-agent (including local
  ones), discarding the sub-worktree concept entirely.** Reject:
  local sub-agents pay an unnecessary serialisation cost, lose the
  ability to run `cargo test` / `git status` directly, and the
  orchestrator gives up the merge-back diff capture (it would have
  to re-derive the diff from the returned tar, which is precisely
  the round-trip we're trying to avoid).
- **Read-only git clone over Kong as v1 transport for remote
  sub-agents.** Considered seriously; deferred. The transport itself
  is a multi-week project (per-tenant `git http-backend` mount
  under Kong, per-branch ACLs, credential rotation, push-back
  semantics, audit). Doing it before ADR 0044 stabilises is
  premature: we don't yet know whether remote sub-agents will be
  the common case or an edge case, and the tar path is the right
  shape for the edge-case end of that spectrum. Promoted to a
  follow-up ADR; the port shape here is forward-compatible.
- **Shared filesystem assumption (NFS / hostPath).** Rejected for
  the v1 default: it ties remote-sub-agent feasibility to
  deployment topology, which violates the "remote sub-agent could
  be anywhere in the mesh" promise of ADR 0007. Possible
  performance opt-in once the cost of tar is measured and shown
  to be limiting.
- **Drop sub-worktrees aggressively on completion (no
  uncaptured-diff guard).** Rejected: the whole point of capturing
  the diff is that aggregation (0044) needs it after sub-agent
  completion, sometimes minutes later. Dropping the worktree before
  the artifact is persisted would lose the work to the next run's
  GC pass.
- **Capture every sub-agent's diff continuously rather than at
  completion.** Considered for crash recovery (so a crashed
  sub-agent still leaves a partial artifact). Rejected for v1: it
  would multiply artifact-store writes by N edits per sub-agent
  and surface partial work that may not even compile. The orphan
  sweeper handles crashed-orchestrator recovery by running
  `capture_diff` once at sweep time, which is the same end state
  with one write per sub-agent instead of N.

## Affected ork modules

- [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
  — extends `WorkspaceHandle` with `parent`, `branch`, `subtask_id`,
  `origin`; adds `WorkspaceOrigin` enum.
- New: `crates/ork-core/src/ports/sub_workspace.rs` —
  `SubWorkspaceCoordinator` trait and supporting types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `sub_workspace`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  — `execute_delegated_call` (ADR 0006) calls
  `SubWorkspaceCoordinator::open_sub` for delegated coding steps and
  threads the child handle into `AgentContext.workspace`.
- New: `crates/ork-integrations/src/sub_workspace.rs` —
  `LocalSubWorkspaceCoordinator` impl, tar exporter, patch importer,
  diff capture, orphan sweeper.
- [`crates/ork-integrations/src/git.rs`](../../crates/ork-integrations/src/git.rs)
  — `LocalGitOperations::open_workspace` populates the new fields on
  the trunk handle; no behavioural change for existing callers.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — `git_diff` and `git_status` learn to render `WorkspaceHandle.parent`
  in their tool output so the LLM knows where it sits in the chain.
- [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs)
  — public re-exports for `LocalSubWorkspaceCoordinator` and config.
- [`crates/ork-agents/src/remote.rs`](../../crates/ork-agents/) (ADR 0007)
  — `RemoteAgent::send` calls `export_for_remote` before send and
  `import_remote_patch` + `capture_diff` after `Completed`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) —
  boot `LocalSubWorkspaceCoordinator` from the existing
  `Arc<dyn GitOperations>` + `Arc<dyn ArtifactStore>` and register the
  ADR-0019 sweep schedule.
- [`config/default.toml`](../../config/default.toml) —
  `[sub_workspace]` section with `default_sub_ttl_seconds`,
  `sweep_interval_seconds`, `tar_inline_cap_bytes`, `exclude_globs`.
- New tests under
  [`crates/ork-integrations/tests/`](../../crates/ork-integrations/tests/):
  `sub_workspace_alloc.rs`, `sub_workspace_isolation.rs`,
  `sub_workspace_export.rs`, `sub_workspace_remote.rs`,
  `sub_workspace_capture.rs`, `sub_workspace_lifecycle.rs`,
  `sub_workspace_sweep.rs`, `sub_workspace_events.rs`.
- New tests under
  [`crates/ork-core/tests/workflow/`](../../crates/ork-core/tests/):
  `delegate_sub_workspace.rs`.
- New tests under [`crates/ork-agents/tests/`](../../crates/ork-agents/):
  `remote_sub_workspace.rs`.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Solace Agent Mesh | No equivalent — SAM has no first-class workspace plane, let alone a parent/child hierarchy | `SubWorkspaceCoordinator` is net-new |
| Aider | Single-worktree, single-branch loop; multi-agent isn't supported | Net-new |
| opencode | Single-worktree | Net-new |
| Claude Code (this CLI) | Single-worktree per session | Net-new |
| GitHub Copilot Workspace | Branch-per-task, single-agent | Closest analogue; we extend with parent/child branch chain |
| Devin / Cognition | Container-per-agent isolation, but the parent/child *workspace* relationship is implicit in the agent tree, not a typed first-class object | `SubWorkspaceCoordinator` makes the relationship explicit and inspectable |
| `git worktree add` (upstream) | The base primitive | Used directly via ADR 0030's `LocalGitOperations` |
| Bazel `--package_path` overlays | One way to layer a sub-workspace on top of a base for isolated edits | Considered as an alternative to per-sub-worktree; rejected because the team workloads need real `cargo test` and real `git diff`, both of which assume a single materialised tree |

## Open questions

- **Maximum chain depth.** The branch-name length grows linearly with
  depth. Three-deep is fine (`run/task/sub/sub/sub` ≈ 100 chars);
  ten-deep starts hitting filesystem and remote limits. Should we
  cap the depth in the coordinator (e.g. reject `open_sub` when
  `parent.parent.parent.parent` is `Some`), or trust workflow
  authors? Lean toward a soft cap of 4 with a config override; the
  cap is for sanity, not correctness.
- **Cross-tenant sub-agents.** If an orchestrator delegates to a
  remote sub-agent in a *different* tenant (the
  [`tid_chain`](../../docs/adrs/0020-tenant-security-and-trust.md)
  case), whose tenant root does the mirror sub-worktree live under?
  Lean toward: the orchestrator's tenant, since the orchestrator
  owns the captured artifact and consumes it via ADR 0044. The
  remote tenant never sees the mirror.
- **Push-back from remote sub-agents.** `import_remote_patch` is
  one-shot. Multi-turn remote sub-agents that want to incrementally
  push partial work would need either repeated `export_for_remote`
  + `import_remote_patch` round-trips (works today, just chatty) or
  the future git-over-Kong transport (the natural fit). Defer until
  a real workload exists.
- **TTL granularity.** `OpenSubRequest.ttl` is per-call; should we
  also support a per-tenant default that overrides
  `LocalSubWorkspaceConfig::default_sub_ttl`? Probably yes — a
  tenant on a small disk volume needs a tighter sweep cadence — but
  out of scope for v1 since `[sub_workspace]` is global config in
  the default.toml shape above.
- **Snapshot include-paths granularity.** `ExportRequest.include_paths`
  is path-prefix matching today. Globs (`crates/ork-core/**/*.rs`)
  would let teams ship just the subset of files a sub-agent needs
  to look at, shrinking tar size. Defer; revisit when a real
  workload pushes against the inline cap.
- **What does ADR 0044 do when two `SubDiffArtifact`s conflict?**
  Out of scope here; this ADR provides the inputs and notes the
  semantic-conflict-at-merge invariant. 0044 owns the answer.
- **Concurrent `capture_diff` calls on the same head.** Documented
  as idempotent above; the implementation must serialise the
  underlying `git diff` + `ArtifactStore::put` to prevent two racers
  producing two artifact ids for the same content. A per-handle
  mutex inside `LocalSubWorkspaceCoordinator` is enough; flagged so
  the implementer doesn't skip it.

## References

- A2A spec: <https://a2a-protocol.org/latest/specification/>
- `git worktree`: <https://git-scm.com/docs/git-worktree>
- `git http-backend` (referenced for the deferred transport):
  <https://git-scm.com/docs/git-http-backend>
- Related ADRs: [0006](0006-peer-delegation.md) (delegation control
  plane that triggers `open_sub`),
  [0007](0007-remote-a2a-agent-client.md) (remote sub-agent
  transport),
  [0016](0016-artifact-storage.md) (where `SubDiffArtifact` lives),
  [0020](0020-tenant-security-and-trust.md) (tenant root containment),
  [0022](0022-observability.md) (event sink),
  [0028](0028-shell-executor-and-test-runners.md) (`ShellExecutor`
  substrate via `LocalGitOperations`),
  [0029](0029-workspace-file-editor.md) (`WorkspaceHandle` and
  `WorkspaceEditor` contracts that this ADR extends),
  [0030](0030-git-operations.md) (`GitOperations` and the trunk
  worktree this ADR layers on top of),
  [0037](0037-lsp-diagnostics.md) (per-worktree LSP cache, reads
  the new `parent` field),
  [0040](0040-repo-map.md) (per-worktree repo map, reads the new
  `parent` field),
  0044 — sub-agent diff aggregation (consumer of `SubDiffArtifact`),
  0045 — coding-team topology (primary caller of the
  delegation-with-sub-workspace path).
