# 0030 — Git Operations & Worktree Management

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core team
- **Phase:** 3
- **Relates to:** 0002, 0011, 0018, 0021, 0028, 0029
- **Supersedes:** —

## Context

ork can read source from a git repository today but has no way to
*mutate* one. The whole git surface is the read-only side of
[`GitRepoWorkspace`](../../crates/ork-integrations/src/workspace.rs):
`ensure_clone` shells out to `git clone --depth 1` / `git fetch
--depth 1` / `git reset --hard origin/<branch>` via the private
[`git_cmd`](../../crates/ork-integrations/src/workspace.rs#L171) helper
and aggressively wipes any local changes on every refresh. There is
no port — and no tool — for `git status`, `git diff`, `git add`,
`git commit`, `git branch`, `git checkout`, or `git worktree`. ADR
[0028](0028-shell-executor-and-test-runners.md) (`ShellExecutor`)
deliberately stops at "spawn a process safely"; ADR
[0029](0029-workspace-file-editor.md) (`WorkspaceEditor`) consumes a
`WorkspaceHandle` that nobody has yet been delegated to provision.

This is now load-bearing. Three concrete pressures converge on it:

1. **Coding agents** — the
   [`workflow-templates/`](../../workflow-templates/) drafts for
   "fix-this-bug" and "implement-this-ADR" assume an agent loop that
   edits, runs tests (ADR 0028), inspects the diff, and commits
   incrementally so the verifier (ADR
   [0025](0025-typed-output-validation-and-verifier-agent.md)) and
   reviewer can see *what changed*. Without `git diff` and `git
   commit` the loop is a single monolithic edit blob.
2. **Parallel steps** — ADR
   [0018](0018-dag-executor-enhancements.md) introduces `parallel:`,
   `map:`, and `switch:` composition. Two sub-agents touching the
   same checkout will trample each other unless each gets its own
   working copy. `git worktree` is the cheap, well-understood unit of
   isolation: shared `.git` object database, per-branch index +
   working tree.
3. **Operator dogfooding** — [`AGENTS.md`](../../AGENTS.md) §8
   already advocates worktrees for human ADR work
   (`git worktree add ../ork-adr-0019 ...`). The agent platform
   should expose the same primitive to the agents it runs; humans
   and AIs working the same loop is a non-negotiable end state.

Two write paths into a checkout already exist or are being
introduced. They each need a partner that owns the *git side* of the
working tree:

- ADR 0029's `WorkspaceEditor` writes file bytes but explicitly
  forbids writes under `.git/`, leaving every git operation as
  someone else's problem.
- ADR 0028's `ShellExecutor` *can* invoke `git` (it's just argv) but
  putting branch/worktree policy on the LLM-prompt side would
  re-implement the sandbox in the agent, which is exactly what ADRs
  0028/0029 work to avoid. We want the policy in Rust, in-process,
  next to `GitRepoWorkspace`.

## Decision

ork **introduces** a `GitOperations` port in `ork-core` plus a
`LocalGitOperations` implementation in `ork-integrations`, exposes a
read-only tool family (`git_status`, `git_diff`, `git_log`) as
always-available native tools through
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs),
and reserves a mutating tool family (`git_add`, `git_commit`,
`git_branch_create`, `git_checkout`, `git_worktree_add`,
`git_worktree_remove`) gated behind RBAC scope names that ADR
[0021](0021-rbac-scopes.md) will enforce.

`LocalGitOperations` is also the **sole provisioner of
`WorkspaceHandle`** (defined in ADR 0029): an agent run obtains a
mutable working copy via `GitOperations::open_workspace`, which
allocates a worktree, branch, and the canonicalised tenant-scoped
root that the editor and shell ports share.

### Implementation choice — shell out to `git` via ADR 0028

`LocalGitOperations` invokes the system `git` CLI through
`ShellExecutor`. We considered three options:

| Option | What we'd get | Why we did/didn't pick it |
| ------ | ------------- | ------------------------- |
| `git2` (libgit2 bindings) | Programmatic API; mature for plumbing ops | Drags `libgit2-sys` + a C toolchain into every release artifact; `git2::Worktree` exists but is fiddly; CRLF/filter handling differs subtly from `git`; HTTPS clone needs `openssl-sys` on Linux, breaking the all-Rust posture we have today via `rustls`-only `reqwest`. |
| `gix` (gitoxide, pure Rust) | All-Rust, fast, no C deps | Worktree creation, `git add`, and `git commit` write-paths shipped recently and the API surface is still moving (we are tracking 0.66+); we'd be the early adopter on the operations that matter most. Read-only ops are solid; mutations are not yet where we need them for v1. |
| **Shell out to `git` via `ShellExecutor`** | Feature parity with whatever `git` ships; same auditing path as every other tool spawn | Output parsing is the cost; we mitigate by using stable porcelain (`git status --porcelain=v2 -z`, `git log --pretty=format:%H%x00%an%x00...`, `git diff --name-only`). `git` is already required by `GitRepoWorkspace`. **Recommendation: this option.** |

Decisive considerations:

- ADR 0028 already lands the sandbox, the timeout/cancel ladder, and
  the audit trail. Implementing `GitOperations` on top of
  `ShellExecutor` reuses every one of those guarantees instead of
  re-inventing them. The `git` binary becomes a single entry on the
  per-binary RBAC list (`shell:cmd:git:invoke`).
- The operations we need (`status`, `diff`, `log`, `add`, `commit`,
  `branch`, `checkout`, `worktree add/remove/list`) all have stable
  porcelain output formats. Worktrees in particular are a CLI-first
  feature; libgit2's worktree support trails `git`'s by years, and
  gix's is in active development.
- The shell-out path lets us swap to a Rust-native implementation
  later **without changing the port**: callers depend on the trait,
  not on the spawn shape.
- Sub-second `git status` on the repos we run is fine; we are not
  fighting for microseconds against a process-fork.

The trade-off is parser fragility: `git`'s porcelain v2 status format
is documented as stable but has version-gated additions; `git log
--pretty=format:` requires us to pick delimiters that cannot occur
in commit metadata. We use `%x00` (NUL) field separators with `-z`
and parse byte-by-byte rather than line-by-line, matching how
`GitRepoWorkspace` already drives `git`.

### Placement

The port lives in `ork-core` (next to
[`workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)) and
the implementation lives in `ork-integrations` (next to
[`workspace.rs`](../../crates/ork-integrations/src/workspace.rs) and
the new `shell.rs` from ADR 0028). Like ADR 0028, we do **not**
create a new `ork-git` crate: the implementation is one module's
worth of `ShellExecutor` calls plus typed parsers, and the natural
neighbours (`workspace.rs`, `shell.rs`, `workspace_editor.rs`) are
already in `ork-integrations`. A follow-up ADR can split
`crates/ork-git` out if the parser surface grows or if a non-git
backend (mercurial, jj) ever lands.

### `GitOperations` port

```rust
// crates/ork-core/src/ports/git.rs

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{RunId, TenantId};

use crate::ports::workspace::WorkspaceHandle;     // ADR 0029

/// One git invocation, scoped to a `WorkspaceHandle`.
///
/// Read-only ops never mutate the working tree or refs. Mutating ops
/// are gated by an explicit `MutationKind` so the implementation can
/// audit, RBAC-check, and refuse classes of operation as a whole
/// (e.g. always refuse `Amend` and `ForcePush`).
#[async_trait]
pub trait GitOperations: Send + Sync {
    // ---- lifecycle (provisions WorkspaceHandle from ADR 0029) ----

    /// Allocate a worktree + branch for one workflow run. Idempotent
    /// per `(tenant_id, run_id, repo, branch)`: a second call with
    /// the same key returns the existing handle.
    async fn open_workspace(
        &self,
        tenant_id: TenantId,
        run_id: RunId,
        req: OpenWorkspaceRequest,
    ) -> Result<WorkspaceHandle, OrkError>;

    /// Tear down a worktree. Refuses if the working tree is dirty
    /// unless `force = true` is set; refuses to delete the primary
    /// worktree of any repo regardless.
    async fn close_workspace(
        &self,
        ws: &WorkspaceHandle,
        force: bool,
    ) -> Result<(), OrkError>;

    // ---- read-only ----

    async fn status(&self, ws: &WorkspaceHandle) -> Result<GitStatus, OrkError>;

    /// `paths.is_empty()` returns the diff of the entire working tree
    /// (vs. `HEAD` by default; vs. `base` if supplied).
    async fn diff(
        &self,
        ws: &WorkspaceHandle,
        opts: DiffOptions,
    ) -> Result<GitDiff, OrkError>;

    async fn log(
        &self,
        ws: &WorkspaceHandle,
        opts: LogOptions,
    ) -> Result<Vec<CommitMeta>, OrkError>;

    async fn list_branches(
        &self,
        ws: &WorkspaceHandle,
        scope: BranchScope,                 // Local / Remote / All
    ) -> Result<Vec<BranchRef>, OrkError>;

    async fn list_worktrees(
        &self,
        tenant_id: TenantId,
        repo: &str,
    ) -> Result<Vec<WorktreeMeta>, OrkError>;

    // ---- mutating ----

    async fn add(
        &self,
        ws: &WorkspaceHandle,
        paths: &[String],                   // empty = error; use `add_all`
    ) -> Result<(), OrkError>;

    async fn add_all(&self, ws: &WorkspaceHandle) -> Result<(), OrkError>;

    /// Creates a *new* commit. Never amends. The caller acknowledges
    /// they have inspected the index (the executor refuses if the
    /// index is empty unless `allow_empty = true`).
    async fn commit(
        &self,
        ws: &WorkspaceHandle,
        req: CommitRequest,
    ) -> Result<CommitMeta, OrkError>;

    async fn branch_create(
        &self,
        ws: &WorkspaceHandle,
        name: &str,
        start_point: Option<&str>,          // ref-ish; default = current HEAD
    ) -> Result<BranchRef, OrkError>;

    /// Switch the worktree to an existing branch. Refuses if the
    /// working tree is dirty unless `force = true` (which discards
    /// changes — the executor logs a warning event).
    async fn checkout(
        &self,
        ws: &WorkspaceHandle,
        target: &str,
        force: bool,
    ) -> Result<(), OrkError>;
}

#[derive(Debug, Clone)]
pub struct OpenWorkspaceRequest {
    /// Logical repo name from the tenant's `RepositorySpec` set.
    pub repo: String,
    /// Branch to base the worktree on. Required; v1 does not infer.
    pub base_branch: String,
    /// New branch to create on top of `base_branch`. The executor
    /// rejects names matching `^(main|master|release/.*)$` unless
    /// `allow_protected_branch = true`.
    pub task_branch: String,
    pub allow_protected_branch: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Empty = whole worktree.
    pub paths: Vec<String>,
    /// `None` = vs. `HEAD`. `Some("origin/main")` = vs. that ref.
    pub base: Option<String>,
    /// `Staged` (vs. `--cached`), `Unstaged`, or `Both` (default).
    pub stage: DiffStage,
    /// Cap on bytes returned to the caller; the rest spills to an
    /// ADR-0016 artifact, mirroring ADR-0028's truncation policy.
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffStage { Staged, Unstaged, #[default] Both }

#[derive(Debug, Clone)]
pub struct LogOptions {
    pub max_count: u32,                     // capped at 200
    pub paths: Vec<String>,                 // empty = whole repo
    pub since: Option<String>,              // ref-ish, exclusive
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchScope { Local, Remote, All }

#[derive(Debug, Clone)]
pub struct BranchRef {
    pub name: String,
    pub commit: String,
    pub is_remote: bool,
    pub upstream: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorktreeMeta {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub is_primary: bool,
    pub is_locked: bool,
}

#[derive(Debug, Clone)]
pub struct CommitRequest {
    pub message: String,
    /// Author override; defaults to `ork-agent <agent@ork.local>`.
    pub author: Option<Identity>,
    pub allow_empty: bool,
    /// Reserved; v1 always rejects `true` and surfaces a clear error.
    /// `Amend` is opt-in and lives behind a separate
    /// `commit_amend(...)` method that this ADR does **not** ship.
    pub amend: bool,
}

#[derive(Debug, Clone)]
pub struct Identity { pub name: String, pub email: String }

#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub sha: String,
    pub author: Identity,
    pub committer: Identity,
    pub message: String,
    pub timestamp_unix: i64,
    pub parents: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<StatusEntry>,
}

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: String,
    /// Two-character XY code from porcelain v2 (`M.`, `.M`, `??`, …).
    pub xy: String,
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GitDiff {
    pub patch: String,                      // unified diff, possibly truncated
    pub truncated: bool,
    pub artifact_id: Option<String>,        // ADR 0016 spill
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}
```

Errors are returned as `OrkError::Validation` for caller-class
mistakes (bad ref name, dirty index without `force`, protected-branch
target) and `OrkError::Integration` for `git`-class failures (network
error during fetch, malformed repo, parse failure). A non-zero exit
from a `git` invocation is **not** an error if the command's design
allows it — `git diff --quiet` for example exits 1 to signal "there
is a diff."

### Worktree as the unit of agent isolation

`open_workspace` does the following, in order:

1. Resolve the read-only `GitRepoWorkspace` clone for `(tenant_id,
   repo)` — the same path
   [`GitRepoWorkspace::repo_path`](../../crates/ork-integrations/src/workspace.rs)
   produces. Fetches `base_branch` if missing.
2. Compute the worktree root:
   `<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/`. Canonicalise it
   and assert it is a descendant of `<cache_dir>/_tasks/<tenant_id>/`.
3. Run `git worktree add -b <task_branch> <root> <base_branch>` from
   inside the read cache. The new worktree shares the cache's
   `.git/objects` so disk usage is roughly the working tree only.
4. Build and return a `WorkspaceHandle` (ADR 0029) with `tenant_id`,
   `run_id`, `repo`, the canonicalised `root`, and `head_commit` =
   the new worktree's HEAD SHA.

For ADR 0018's parallel branches, the executor opens **one workspace
per parallel sub-step**. The sub-steps run concurrently, each in its
own working tree against its own `<task_branch>`. Merging the
sub-steps' work back together is **not** automatic in this ADR — the
user-visible result of a parallel block is a list of branches the
parent step can `git diff` against the base or feed to a downstream
"merge results" step. Cross-branch reconciliation (cherry-pick,
merge, rebase) is deliberately deferred; this ADR only establishes
the isolation boundary and the read primitives that downstream ADRs
need to compose.

`close_workspace` removes the worktree (`git worktree remove
[--force]`) and prunes the entry. It refuses to remove the primary
worktree of the read cache — `GitRepoWorkspace` owns that — and
refuses dirty trees unless `force = true`. Garbage collection of
abandoned worktrees from crashed runs is the scheduler's
responsibility (ADR [0019](0019-scheduled-tasks.md)) and out of scope
here, mirroring how ADR 0029 punts on `_tasks/` GC.

### Safety rails (enforced by every method)

The executor refuses, *fails closed*, on each of the following:

1. **No auto-push.** No `push` method exists in v1. There is no way
   to reach a remote write through this port. A future ADR can
   introduce `push(remote, refspec)` behind its own scope; until
   then, publishing a branch is an out-of-band operator action.
2. **No force-push.** Same as (1); the absence of `push` makes this
   a non-issue but it is restated to fix expectations.
3. **No amend.** `CommitRequest.amend = true` is rejected with
   `OrkError::Validation("amend not supported in v1")`. A separate
   `commit_amend(...)` method, gated behind its own scope and step
   kind, is a follow-up if the need is concrete.
4. **No dirty-index surprises.** `checkout`, `close_workspace`, and
   any future ref-moving operation refuse a dirty working tree
   unless the caller passes `force = true`. The audit log records
   every `force` invocation with the diff that was discarded.
5. **No detached-HEAD commits.** `commit` requires the worktree to
   be on a named branch. A future ADR may relax this.
6. **No protected-branch mutation.** `branch_create` and the
   implicit branch named in `OpenWorkspaceRequest.task_branch`
   reject names matching `^(main|master|release/.*|prod.*)$`
   unless `allow_protected_branch = true`. The pattern lives in
   `LocalGitOperationsConfig` and is tenant-overridable.
7. **No host config leakage.** `git` is invoked with
   `-c protocol.allow=never -c http.sslVerify=true -c
   credential.helper= -c gc.auto=0 -c core.autocrlf=false`. Tenant
   credentials never enter `git`'s environment automatically;
   anything that needs them (clone URL with embedded token, future
   `push`) must thread them through the same explicit `req.env`
   channel ADR 0028 defined.
8. **Argv-only spawning.** Inherited from ADR 0028. No `sh -c "git
   ..."` shape; we always pass `argv = ["git", "-C", root, ...]`.

### Tool surface

Two families of tools register through
[`CodeToolExecutor::descriptors`](../../crates/ork-integrations/src/code_tools.rs).

**Read-only family — always available** (no scope required, like
today's `read_file` / `code_search`):

| Tool | Wraps |
| ---- | ----- |
| `git_status` | `GitOperations::status` |
| `git_diff` | `GitOperations::diff` |
| `git_log` | `GitOperations::log` |
| `git_list_branches` | `GitOperations::list_branches` |
| `git_list_worktrees` | `GitOperations::list_worktrees` |

**Mutating family — gated** (descriptors visible to the LLM only
when the agent's prompt declares them; runtime check deferred to ADR
0021):

| Tool | Wraps | Reserved scope |
| ---- | ----- | -------------- |
| `git_add` | `add` / `add_all` | `tool:git_add:invoke` |
| `git_commit` | `commit` | `tool:git_commit:invoke` |
| `git_branch_create` | `branch_create` | `tool:git_branch_create:invoke` |
| `git_checkout` | `checkout` | `tool:git_checkout:invoke` |
| `git_worktree_add` | `open_workspace` (LLM-facing alias) | `tool:git_worktree_add:invoke` |
| `git_worktree_remove` | `close_workspace` | `tool:git_worktree_remove:invoke` |

Sample descriptor (`git_diff`):

```json
{
  "name": "git_diff",
  "description": "Show the unified diff for the current workspace. Optionally limit to specific paths or compare against a base ref.",
  "parameters": {
    "type": "object",
    "properties": {
      "paths":     { "type": "array", "items": {"type": "string"} },
      "base":      { "type": "string" },
      "stage":     { "type": "string", "enum": ["staged", "unstaged", "both"], "default": "both" },
      "max_bytes": { "type": "integer", "minimum": 1024, "maximum": 1048576 }
    }
  }
}
```

Sample descriptor (`git_commit`):

```json
{
  "name": "git_commit",
  "description": "Create a new commit from the staged index. Never amends. Refuses an empty index unless allow_empty=true.",
  "parameters": {
    "type": "object",
    "properties": {
      "message":     { "type": "string", "minLength": 1 },
      "author_name": { "type": "string" },
      "author_email": { "type": "string", "format": "email" },
      "allow_empty": { "type": "boolean", "default": false }
    },
    "required": ["message"]
  }
}
```

Tool result wire shape (`git_status`):

```json
{
  "branch": "ork/run-01HX.../adr-0030",
  "head":   "9f1c...",
  "upstream": null,
  "ahead": 0,
  "behind": 0,
  "entries": [
    { "path": "crates/ork-core/src/ports/git.rs", "xy": "A.", "renamed_from": null },
    { "path": "docs/adrs/0030-git-operations.md",  "xy": "M.", "renamed_from": null }
  ]
}
```

The active `WorkspaceHandle` is resolved from `AgentContext`
(extended by ADR 0028 with a `workspace: Option<WorkspaceHandle>`
field). Tools that need a workspace and find none return a structured
tool result `{ "error": "no_workspace" }` — the agent can then call
`git_worktree_add` (when permitted) to provision one.

### `LocalGitOperations` configuration

```rust
// crates/ork-integrations/src/git.rs

pub struct LocalGitOperationsConfig {
    /// Default per-call timeout for `git` invocations. Each method
    /// can override; `clone`/`fetch` default higher than `status`.
    pub default_timeout: Duration,
    pub clone_timeout: Duration,
    /// Truncation cap for `diff` returned to the LLM, before the
    /// rest spills to an ADR-0016 artifact.
    pub diff_max_bytes: usize,
    pub log_max_count: u32,
    /// Regex for protected branch names (default
    /// `^(main|master|release/.*|prod.*)$`).
    pub protected_branch_pattern: String,
    /// Default committer identity for agent commits.
    pub default_identity: Identity,
}
```

`LocalGitOperations` is constructed from an `Arc<dyn ShellExecutor>`,
an `Arc<GitRepoWorkspace>` (for path resolution + the read cache it
worktrees off of), an `Arc<dyn ArtifactStore>` (diff spill), and the
config above.

## Acceptance criteria

- [ ] Trait `GitOperations` defined at
      `crates/ork-core/src/ports/git.rs` with the signature shown in
      `Decision`.
- [ ] Supporting types `OpenWorkspaceRequest`, `DiffOptions`,
      `DiffStage`, `LogOptions`, `BranchScope`, `BranchRef`,
      `WorktreeMeta`, `CommitRequest`, `Identity`, `CommitMeta`,
      `GitStatus`, `StatusEntry`, `GitDiff` defined in the same
      module and re-exported from
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs).
- [ ] `LocalGitOperations` defined at
      `crates/ork-integrations/src/git.rs`, constructed from an
      `Arc<dyn ShellExecutor>`, an `Arc<GitRepoWorkspace>`, an
      `Arc<dyn ArtifactStore>`, and `LocalGitOperationsConfig`.
- [ ] `LocalGitOperations::open_workspace` runs `git worktree add -b
      <task_branch> <root> <base_branch>`, canonicalises `<root>`
      under `<cache_dir>/_tasks/<tenant_id>/`, and returns a
      `WorkspaceHandle` whose `head_commit` equals the new worktree's
      HEAD SHA, verified by
      `crates/ork-integrations/tests/git_worktree.rs::open_creates_isolated_tree`.
- [ ] `LocalGitOperations::open_workspace` rejects with
      `OrkError::Validation` when `task_branch` matches the protected
      pattern and `allow_protected_branch = false`, verified by
      `crates/ork-integrations/tests/git_worktree.rs::refuses_protected_branch`.
- [ ] `LocalGitOperations::open_workspace` is idempotent for the same
      `(tenant_id, run_id, repo, task_branch)` tuple, verified by
      `crates/ork-integrations/tests/git_worktree.rs::open_is_idempotent`.
- [ ] `LocalGitOperations::status` parses `git status --porcelain=v2
      --branch -z` into `GitStatus`, verified by
      `crates/ork-integrations/tests/git_status.rs` covering: clean
      tree, modified file, untracked file, renamed file, ahead/behind
      counts.
- [ ] `LocalGitOperations::diff` honours `paths`, `base`, and `stage`
      options; spills oversized output to an artifact and sets
      `truncated = true` and `artifact_id`, verified by
      `crates/ork-integrations/tests/git_diff.rs::truncates_large_diff`.
- [ ] `LocalGitOperations::log` parses
      `git log --pretty=format:%H%x00%an%x00%ae%x00%cn%x00%ce%x00%ct%x00%P%x00%s -z`
      into `Vec<CommitMeta>`, capped at `log_max_count`, verified by
      `crates/ork-integrations/tests/git_log.rs::recent_commits_round_trip`.
- [ ] `LocalGitOperations::commit` rejects with `OrkError::Validation`
      when `CommitRequest.amend = true`, verified by
      `crates/ork-integrations/tests/git_commit.rs::refuses_amend`.
- [ ] `LocalGitOperations::commit` rejects an empty index unless
      `allow_empty = true`, verified by
      `crates/ork-integrations/tests/git_commit.rs::refuses_empty_index`.
- [ ] `LocalGitOperations::commit` rejects on detached HEAD, verified
      by `crates/ork-integrations/tests/git_commit.rs::refuses_detached_head`.
- [ ] `LocalGitOperations::checkout` rejects a dirty working tree
      unless `force = true`, verified by
      `crates/ork-integrations/tests/git_checkout.rs::refuses_dirty_tree`.
- [ ] `LocalGitOperations::close_workspace` refuses to remove the
      primary worktree of the read cache, verified by
      `crates/ork-integrations/tests/git_worktree.rs::refuses_primary_worktree_removal`.
- [ ] `LocalGitOperations` does not invoke `git push` anywhere; a
      grep over `crates/ork-integrations/src/git.rs` for the literal
      `"push"` finds zero matches outside of comments, verified by a
      negative test
      `crates/ork-integrations/tests/git_no_push.rs::source_has_no_push_command`.
- [ ] `LocalGitOperations` invokes `git` only via `ShellExecutor` —
      no direct `tokio::process::Command::new("git")` in the
      implementation file, verified by the same negative test.
- [ ] `CodeToolExecutor::is_code_tool` returns `true` for the seven
      tool names: `git_status`, `git_diff`, `git_log`,
      `git_list_branches`, `git_list_worktrees`, `git_add`,
      `git_commit`, `git_branch_create`, `git_checkout`,
      `git_worktree_add`, `git_worktree_remove`.
- [ ] `CodeToolExecutor::descriptors` returns descriptors for each
      tool above with the JSON-Schema parameter blocks shown under
      `Tool surface`.
- [ ] `CodeToolExecutor::execute` dispatches each `git_*` arm to the
      corresponding `GitOperations` method, mapping the JSON input to
      the typed request and returning the wire shapes documented in
      `Decision`.
- [ ] When `AgentContext.workspace` is `None`, the workspace-bound
      tools return the structured result `{ "error": "no_workspace" }`
      rather than `OrkError::Validation`, verified by
      `crates/ork-integrations/tests/git_tools.rs::missing_workspace_returns_structured_error`.
- [ ] `cargo test -p ork-integrations git::` is green.
- [ ] `cargo test -p ork-core ports::git::` is green.
- [ ] `cargo test -p ork-agents local::` still passes (no regressions
      in the agent loop from the new descriptors).
- [ ] Public API documented in
      [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
      and re-exported from
      [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs).
- [ ] [`README.md`](README.md) ADR index row added for `0030`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Coding agents can finally inspect what they did, commit
  incrementally, and present a reviewable diff at the end of a run.
  Closes the `git diff` / `git commit` half of the
  "implement-this-ADR" template that ADR 0028 / 0029 left dangling.
- Worktrees are the cheapest correct isolation boundary for ADR
  0018's parallel branches: shared object database (no clone
  duplication), per-branch index, no cross-talk through the working
  tree.
- Reuses ADR 0028's sandbox, cancellation, and audit pipeline. There
  is exactly one place in the codebase that spawns processes; `git`
  is not special-cased.
- The port boundary insulates the rest of the system from the
  shell-out choice. Swapping to `gix` later, or to a hybrid (gix for
  reads, shell for worktrees), is a within-crate change.
- Resolves the `WorkspaceHandle` provisioning question that ADR 0029
  left open: there is exactly one provisioner, and it is
  `GitOperations`.

### Negative / costs

- Output parsing is the parser's bug surface forever. We mitigate
  with `--porcelain=v2 -z` and explicit field-separator NUL bytes,
  but a future `git` version could still surprise us with new XY
  codes or a renamed `# branch.upstream` header. Each parser pins
  its expected wire shape with a fixture.
- Worktree directories live under `<cache_dir>/_tasks/...` and join
  ADR 0029's pile-up of per-run trees. Disk usage scales with active
  runs. Eventually the scheduler (ADR 0019) or a startup sweeper has
  to GC them; this ADR does not fix that.
- `git worktree add -b` is *fast* but not free: ~50–200 ms on a
  warmed cache, more on cold caches. A run that fans out to a
  parallel block of 16 sub-steps pays this 16 times. Acceptable
  today; if it bites we can pre-allocate a worktree pool.
- The "no auto-push, no amend, no force" posture is conservative on
  purpose. Workflow templates that *want* to push (e.g. an autonomous
  PR-opener) cannot do so via this port; they will need a follow-up
  ADR that wires `push` through ADR 0021 RBAC and ADR 0020 tenant
  credential plumbing.
- We are committing to a wire shape for the eleven tools' JSON
  inputs and outputs. Changing those later breaks prompt authors and
  in-flight workflow templates. We accept this in exchange for the
  predictable surface those templates need.
- Worktrees on macOS / Linux behave; on Windows the file-locking
  story for stale worktrees is messier. We document Linux-first; CI
  runs the smoke tests only on Linux for v1, mirroring ADR 0028.

### Neutral / follow-ups

- A separate ADR can add `push`, `pull`, `rebase`, and `merge` for
  the autonomous-PR-opener line of work. They sit cleanly behind the
  same port; the absence here is intentional, not architectural.
- A separate ADR can split `crates/ork-git` out if the parser surface
  grows or a non-git backend (`hg`, `jj`) ever materialises.
- ADR [0021](0021-rbac-scopes.md) wires `tool:git_*:invoke`
  enforcement when its `ScopeChecker` lands; the names here are
  pre-shaped for that grammar.
- ADR [0022](0022-observability.md) consumes the `git.spawn` /
  `git.exit` events emitted by `ShellExecutor`; no extra event types
  needed from this ADR.
- AST-aware refactors and per-hunk patch staging stay with
  `WorkspaceEditor` (ADR 0029); this ADR is whole-file-and-up.

## Alternatives considered

- **`git2` (libgit2 bindings).** Rejected for v1: drags `libgit2-sys`
  + a C toolchain into release artifacts, requires `openssl-sys` on
  Linux for HTTPS clone (we are otherwise pure-`rustls` via
  `reqwest`), and worktree support is more awkward than the CLI.
  Considered as a possible swap-in later behind the same port.
- **`gix` (gitoxide).** Rejected for v1 specifically because the
  worktree + commit + index write paths are still rapidly evolving;
  read-only ops are solid but the operations that *matter* for this
  ADR (worktree create, add, commit) are exactly the ones we'd want
  battle-tested. Strong candidate for a v2 rewrite once gix's
  high-level API stabilises; keeping the port boundary clean here
  makes that swap a within-crate change.
- **Extend `RepoWorkspace` directly.** Rejected: `RepoWorkspace` is
  the read cache (per-tenant, refresh-and-reset). Mixing per-run
  worktree lifecycle, branch creation, and commit semantics into it
  would force every read path to grow concerns it doesn't need.
  Sibling port composes more cleanly, same call as ADRs 0028 / 0029.
- **Single MCP `git` server.** Rejected: same reasoning as ADR 0028's
  shell rejection. The git client is the trust boundary for tenant
  worktrees; putting policy on the far side of `rmcp` cannot enforce
  per-tenant cache layout. ADR [0010](0010-mcp-tool-plane.md)'s
  "internal tools stay native" rule applies.
- **Bypass `ShellExecutor`, use `tokio::process::Command::new("git")`
  directly.** Rejected: would re-implement the sandbox + cancellation
  + audit pipeline that ADR 0028 just landed, and bifurcate the
  process-spawn audit into two code paths.
- **Per-run *clones* instead of worktrees.** Rejected: a clone for
  every parallel sub-step is full-duplication of the object database
  per step. Worktrees share `.git/objects`; on a 1 GB repo that is
  the difference between 80 MB and 1 GB per concurrent step.
- **Auto-merge sub-step branches at parallel-block close.** Rejected
  for v1: cross-branch reconciliation policy (cherry-pick? merge?
  rebase? conflict gate?) is its own ADR's worth of decisions. We
  expose the isolation primitive and let the workflow author choose
  the merge strategy explicitly.

## Affected ork modules

- New: `crates/ork-core/src/ports/git.rs` — `GitOperations` trait
  and supporting types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `git`.
- [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
  — `WorkspaceHandle` (defined by ADR 0029); this ADR consumes it
  unchanged.
- New: `crates/ork-integrations/src/git.rs` — `LocalGitOperations`,
  porcelain parsers, worktree provisioning, safety rails.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — register the eleven `git_*` tools, hold the
  `Arc<dyn GitOperations>` field.
- [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs)
  — public re-exports for `LocalGitOperations` and its config.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
  — boot `LocalGitOperations` from the existing
  `Arc<GitRepoWorkspace>` + `Arc<dyn ShellExecutor>` (ADR 0028) +
  `Arc<dyn ArtifactStore>`, wire it into the `CodeToolExecutor`
  builder.
- [`config/default.toml`](../../config/default.toml) — `[git]`
  section with `default_timeout_seconds`, `clone_timeout_seconds`,
  `diff_max_bytes`, `log_max_count`, `protected_branch_pattern`,
  `default_author_name`, `default_author_email`.
- [`crates/ork-integrations/tests/`](../../crates/ork-integrations/tests/)
  — new test files (`git_worktree.rs`, `git_status.rs`,
  `git_diff.rs`, `git_log.rs`, `git_commit.rs`, `git_checkout.rs`,
  `git_tools.rs`, `git_no_push.rs`).

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Solace Agent Mesh | No first-class git surface; SAM relies on whatever MCP server an integration brings | `GitOperations` + native `git_*` tools |
| Aider | Per-edit auto-commit on a task branch; `--auto-commits` flag | `commit` + worktree-per-run; auto-commit policy is a workflow-template decision, not a port decision |
| Claude Code (this CLI) | `Bash` tool with `git` allowed via permission prompt | `git_*` tool family with read-only ops always-on, mutating ops behind ADR 0021 scopes |
| OpenHands | Per-task workspace container, branch-per-task | `WorkspaceHandle` provisioned via `open_workspace`, branch-per-run |
| GitHub Copilot Workspace | Branch-per-task with structured diff surface | `git_diff` returning truncated unified diff plus artifact spill |
| Forgejo / Gitea Actions | `git worktree`-style per-job checkouts | `git worktree add` provisioning per parallel sub-step |

## Open questions

- **Cross-branch reconciliation at parallel-block close.** When ADR
  0018 runs three sub-steps in parallel, each on its own branch, who
  decides how their work combines? The current answer is "the
  workflow author, via an explicit downstream step." A follow-up ADR
  may introduce a `merge_strategy:` knob on `parallel:` blocks.
- **Network egress during `clone`/`fetch`.** `LocalGitOperations`
  inherits whatever network policy `ShellExecutor` ships (host
  network by default in v1). A no-network mode for `clone`/`fetch`
  would force pre-population of the read cache; deferred until ADR
  0028's containerisation question lands.
- **`git push` and remote authentication.** Out of scope for v1.
  When it lands, it must thread tenant credentials through the
  explicit `req.env` channel ADR 0028 defined and gate behind a
  `git:push:<remote>:invoke` scope. The shape is reserved here so
  the ADR-0021 vocabulary doesn't have to retrofit.
- **Garbage collection of `_tasks/<tenant>/<run>/`.** Same as ADR
  0029's open question; almost certainly ADR 0019 + a startup
  sweeper, but not fixed here.
- **LFS / submodules.** Both are off in v1. `git -c
  submodule.recurse=false` is set on every command; LFS smudge
  filters are disabled via `-c filter.lfs.smudge= -c
  filter.lfs.required=false`. Tenants needing LFS get a follow-up
  ADR.
- **Windows.** Acceptance criteria target Linux/macOS; Windows
  worktree behaviour around file locks is best-effort. CI runs the
  smoke tests only on Linux for v1, matching ADR 0028.
- **Per-binary RBAC scope vs per-tool scope.** ADR 0028 reserves
  `shell:cmd:git:invoke` (per-binary). This ADR reserves
  `tool:git_*:invoke` (per-tool). Both fire for a `git_commit` call.
  The double-scope is intentional — a tenant may grant "read-only
  git tools" without granting "arbitrary `git` argv via
  `run_command`" — but ADR 0021 will need to formalise the
  precedence.

## References

- ADR [`0002`](0002-agent-port.md) — `Agent` port (caller of the
  ADR-0011 tool loop that registers these tools).
- ADR [`0010`](0010-mcp-tool-plane.md) — "internal tools stay
  native" rule.
- ADR [`0011`](0011-native-llm-tool-calling.md) — tool catalog,
  per-agent tool allow-lists, output truncation policy.
- ADR [`0016`](0016-artifact-storage.md) — artifact spill for
  oversized `git diff` output.
- ADR [`0018`](0018-dag-executor-enhancements.md) — parallel /
  switch / map composition; the consumer of worktree isolation.
- ADR [`0020`](0020-tenant-security-and-trust.md) — tenant trust
  frame this ADR composes with.
- ADR [`0021`](0021-rbac-scopes.md) — scope vocabulary that
  enforces `tool:git_*:invoke`.
- ADR [`0028`](0028-shell-executor-and-test-runners.md) —
  `ShellExecutor`; the spawn substrate this ADR builds on.
- ADR [`0029`](0029-workspace-file-editor.md) — `WorkspaceHandle`
  type and `WorkspaceEditor` port; this ADR provisions the handle.
- `git status --porcelain=v2`:
  <https://git-scm.com/docs/git-status#_porcelain_format_version_2>.
- `git worktree`:
  <https://git-scm.com/docs/git-worktree>.
- `gitoxide` (`gix`) project: <https://github.com/Byron/gitoxide>.
- `git2` (libgit2 bindings): <https://crates.io/crates/git2>.
