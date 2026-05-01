# 0029 — Workspace file editor and patch application

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0002, 0010, 0011, 0016, 0020, 0028, 0030, 0031
- **Supersedes:** —

## Context

ork agents can read source today but cannot write it. The
read-only surface lives behind
[`RepoWorkspace`](../../crates/ork-core/src/ports/workspace.rs) and is
exercised by
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs)
through the `read_file` / `code_search` / `list_tree` tools. The
backing implementation in
[`crates/ork-integrations/src/workspace.rs`](../../crates/ork-integrations/src/workspace.rs)
maintains a **per-tenant cache** of shallow clones at
`<cache_dir>/<tenant_id>/<repo_name>` and aggressively resets the
working tree to `origin/<branch>` on every `ensure_clone`. That
posture is correct for a read cache and wrong for anything an agent
edits — local changes would be wiped on the next fetch.

The artifact store from ADR [0016](0016-artifact-storage.md) is **not
the right home** for source mutation either. Artifacts are addressed
by `(tenant, context_id, name, version)`, persisted to S3/GCS/FS, and
designed for cross-task hand-off of generated outputs (PDFs, CSVs,
charts). They are emphatically not a working copy: they have no
relative paths into a repo, no git semantics, no concept of "the file
I just saw at line 42." Coding agents need to mutate
[`crates/...`](../../crates/) under a real working tree so that
[ADR 0028](0028-shell-executor.md) (`ShellExecutor`, e.g. running
`cargo test`) and [ADR 0030](0030-git-operations.md) (`GitOperations`,
e.g. `git diff` / `git commit`) observe the same bytes.

The workflow templates in [`workflow-templates/`](../../workflow-templates/)
already include drafts of "fix-this-bug" and "implement-this-ADR"
flows that today stall the moment the planner tries to call something
like `write_file`. The block is the missing port, not the missing
plumbing.

This ADR closes the gap by introducing a write surface that is
sandboxed per tenant, conflict-aware, and shareable with the shell
and git ports landing in ADRs 0028 / 0030.

## Decision

ork **introduces** a write surface that is split across three
concerns:

1. A shared **`WorkspaceHandle`** type — defined here, consumed by
   ADRs 0028 and 0030 — that names a single mutable working copy for
   the duration of one workflow run. It replaces ad-hoc `String`
   paths and is the unit of sandboxing.
2. A **`WorkspaceEditor`** port in `ork-core` with four methods
   (`create_file`, `update_file`, `delete_file`, `apply_patch`) and
   an optimistic-concurrency contract built on content hashes.
3. Two native tool descriptors (`write_file`, `apply_patch`)
   registered through the existing
   [`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs)
   so the LLM tool catalog from
   ADR [0011](0011-native-llm-tool-calling.md) picks them up
   automatically.

External code-mutation tools (e.g. an MCP filesystem server) remain
**out of scope**: ADR [0010](0010-mcp-tool-plane.md) is for tools we
do not own; the working copy underlying every coding agent is a
first-class internal concern, mirroring the placement of
`RepoWorkspace` rather than `MCPClient`.

### `WorkspaceHandle`

```rust
// crates/ork-core/src/ports/workspace.rs   (extends the existing module)

/// One mutable working copy, owned by exactly one workflow run.
///
/// Created when a coding-agent step starts; dropped when the run
/// terminates. Shared verbatim with `ShellExecutor` (ADR 0028) and
/// `GitOperations` (ADR 0030) so all three operate on the same bytes.
#[derive(Clone, Debug)]
pub struct WorkspaceHandle {
    pub id: WorkspaceId,           // ULID; correlates logs across ports
    pub tenant_id: TenantId,
    pub run_id: RunId,
    pub repo: String,              // logical name from RepositorySpec
    pub root: PathBuf,             // canonicalised; under tenant root
    pub head_commit: String,       // git SHA the handle was branched from
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceId(pub uuid::Uuid);
```

Lifecycle (the provisioning flow itself is owned by ADR 0030, which
introduces `GitOperations::open_workspace`; this ADR only consumes
the handle):

- A `WorkspaceHandle` is allocated against a *separate* directory
  from the read-only cache used by `RepoWorkspace`. Suggested layout:
  `<cache_dir>/_tasks/<tenant_id>/<run_id>/<repo>/`.
- The handle's `root` is canonicalised at construction; every editor
  call re-validates that the resolved write path stays under it.
- The handle is dropped (and its tree garbage-collected) when the
  owning run reaches a terminal state. Crash-recovery is the
  scheduler's problem (ADR [0019](0019-scheduled-tasks.md)) and out
  of scope here.

### `WorkspaceEditor` port

```rust
// crates/ork-core/src/ports/workspace_editor.rs

#[async_trait::async_trait]
pub trait WorkspaceEditor: Send + Sync {
    async fn create_file(
        &self,
        ws: &WorkspaceHandle,
        rel_path: &str,
        content: &[u8],
        opts: CreateOpts,
    ) -> Result<FileWrite, WorkspaceEditError>;

    async fn update_file(
        &self,
        ws: &WorkspaceHandle,
        rel_path: &str,
        content: &[u8],
        expected_hash: ContentHash,        // optimistic concurrency
    ) -> Result<FileWrite, WorkspaceEditError>;

    async fn delete_file(
        &self,
        ws: &WorkspaceHandle,
        rel_path: &str,
        expected_hash: ContentHash,
    ) -> Result<FileWrite, WorkspaceEditError>;

    /// Apply a unified diff (RFC-style `--- a/... +++ b/...`) atomically:
    /// either every hunk applies cleanly or no file is touched.
    async fn apply_patch(
        &self,
        ws: &WorkspaceHandle,
        diff: &str,
        opts: PatchOpts,
    ) -> Result<PatchOutcome, WorkspaceEditError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CreateOpts {
    pub overwrite: bool,                   // default false
    pub mkdir_parents: bool,               // default true
    pub mode: Option<u32>,                 // POSIX bits; ignored on Windows
}

#[derive(Clone, Debug, Default)]
pub struct PatchOpts {
    /// 0 = exact, 3 = git's default fuzz. Capped at 3.
    pub context_fuzz: u8,
    /// If true, fail when the patch would touch a path outside the
    /// already-touched set for this run (used by ADR 0031 to keep
    /// transactional bundles cohesive).
    pub strict_paths: bool,
}

#[derive(Clone, Debug)]
pub struct FileWrite {
    pub rel_path: String,
    pub before_hash: Option<ContentHash>,  // None for create
    pub after_hash: ContentHash,
    pub bytes_written: u64,
}

#[derive(Clone, Debug)]
pub struct PatchOutcome {
    pub touched: Vec<FileWrite>,           // one entry per affected path
    pub created: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);      // BLAKE3-256 of file bytes

#[derive(thiserror::Error, Debug)]
pub enum WorkspaceEditError {
    #[error("path escapes workspace root")]                PathEscape,
    #[error("absolute paths are not permitted")]           AbsolutePath,
    #[error("path traverses a symlink that escapes root")] SymlinkEscape,
    #[error("writes to .git/ are forbidden")]              ProtectedPath,
    #[error("file already exists: {0}")]                   AlreadyExists(String),
    #[error("file not found: {0}")]                        NotFound(String),
    #[error("expected hash {expected:?}, found {actual:?}")]
    Conflict { expected: ContentHash, actual: ContentHash, captured_diff: String },
    #[error("patch did not apply cleanly: {0}")]           PatchReject(String),
    #[error("io: {0}")]                                    Io(#[from] std::io::Error),
}
```

The default implementation lives in `ork-integrations` next to
`workspace.rs`, e.g. `crates/ork-integrations/src/workspace_editor.rs`.
Patch application uses the `diffy` crate (pure-Rust, MIT-licensed,
already a transitive dep candidate via `similar`) so the port has no
dependency on a `patch(1)` binary.

### Sandboxing rules (enforced by every method)

The editor mirrors the read-side
[`resolve_under_root`](../../crates/ork-integrations/src/workspace.rs#L329)
guard but is stricter, because mistakes here corrupt the working
copy rather than just leaking a read:

1. **No absolute paths.** A `rel_path` starting with `/` (or
   `<drive>:\` on Windows) returns `WorkspaceEditError::AbsolutePath`.
2. **No `..` traversal.** Verified textually before resolution.
3. **No symlink escape.** Each component is checked with
   `fs::symlink_metadata`; if any component is a symlink whose
   resolved target falls outside `ws.root`, the call returns
   `SymlinkEscape`. Following symlinks **inside** the root is
   permitted.
4. **No writes to `.git/`** (or any `.git/` discovered along the
   path). `GitOperations` (ADR 0030) is the only sanctioned writer
   for that subtree.
5. **Per-tenant root.** `ws.root` must canonicalise as a descendant
   of `<cache_dir>/_tasks/<tenant_id>/`; tenants cannot fabricate a
   handle pointing at another tenant's tree because handles flow
   from `GitOperations::open_workspace` and carry their own
   `tenant_id` that is checked at the editor boundary against the
   `AgentContext` (ADR 0020).

`apply_patch` runs the same checks **for every path the patch
touches** before writing the first byte. Atomicity is provided by
buffering all post-apply contents in memory, then doing the writes
under a per-handle mutex; partial application is impossible because
either every staged write succeeds or the editor returns
`PatchReject` with no on-disk effect.

### Concurrent-write safety: optimistic hashing

The editor adopts **optimistic concurrency via content hashes**:

- `read_file` (in `RepoWorkspace`) is extended to return
  `(content, ContentHash)` so callers learn the hash they observed.
- `update_file` and `delete_file` require the caller to pass
  `expected_hash`. The editor recomputes the on-disk hash under the
  per-handle mutex; mismatch returns
  `WorkspaceEditError::Conflict { expected, actual, captured_diff }`,
  where `captured_diff` is a unified diff between the expected and
  current bytes so the agent can reconcile rather than retry blind.
- `create_file` rejects `AlreadyExists` unless `opts.overwrite` is
  set; with `overwrite = true` it degrades to `update_file`
  semantics and requires `expected_hash` ≠ `None`.
- `apply_patch` derives the per-path `expected_hash` from the diff
  itself: a unified diff includes pre-image lines, so the editor
  hashes the pre-image and compares against current bytes. No extra
  caller bookkeeping required.

Why optimistic hashing rather than the alternatives weighed on the
desk:

- **Exclusive lock per task** (one writer per workspace handle, full
  stop) was rejected because the contention isn't between two ork
  agents — handles are 1:1 with runs — but between *the agent and
  external mutators*: a developer with the working copy mounted, a
  `cargo test` invocation that drops `target/`, a `git pull` from
  ADR 0030. Locking the handle does not stop those. It also
  head-of-line blocks `apply_patch` behind a still-running shell
  step, which is the wrong default for ReAct-style loops.
- **Last-writer-wins with diff capture** is what `git checkout
  --theirs` does. It's expedient for write-heavy LLM loops but
  silently destroys edits that the human or a parallel tool made
  between the agent's read and write. The captured diff is recovery
  evidence after the fact, not prevention.
- **Optimistic hashing** matches what every coding-agent harness
  already settled on (Cursor, Aider, Claude Code's `Edit` tool —
  see [References](#references)). It costs one BLAKE3 per file
  (cheap; BLAKE3 saturates a single core at ~3 GB/s), prevents
  silent overwrites of external edits, and degrades gracefully:
  a `Conflict` error is fully recoverable by re-reading and
  re-emitting the patch.

### Tool surface

Two new descriptors, registered alongside today's read-only set in
[`CodeToolExecutor::descriptors`](../../crates/ork-integrations/src/code_tools.rs#L32):

```jsonc
// write_file
{
  "type": "object",
  "properties": {
    "path":          { "type": "string" },
    "content":       { "type": "string" },         // utf-8 only via tool;
    "expected_hash": { "type": "string" },         // hex BLAKE3, omit on create
    "create":        { "type": "boolean", "default": false }
  },
  "required": ["path", "content"]
}

// apply_patch
{
  "type": "object",
  "properties": {
    "diff":          { "type": "string" },         // unified diff
    "context_fuzz":  { "type": "integer", "minimum": 0, "maximum": 3 }
  },
  "required": ["diff"]
}
```

The tools resolve the active workspace from `AgentContext` (which
ADR 0028 extends with a `workspace: Option<WorkspaceHandle>` field;
this ADR depends on that field but does not introduce it).

### Hexagonal placement (path argument)

- **Port** in `ork-core` (`ports/workspace_editor.rs`) — domain code
  must be able to talk about edits without depending on `tokio::fs`
  or a patch crate. This matches the placement of
  [`ArtifactStore`](../../crates/ork-core/src/ports/artifact_store.rs)
  (ADR 0016) and [`HumanInputGate`](../../docs/adrs/0027-human-in-the-loop.md)
  (ADR 0027), and keeps the §3.5 hexagonal invariant intact.
- **Implementation** in `ork-integrations` next to the read-side
  `workspace.rs`. Co-locating the two ensures the sandbox helpers
  (`resolve_under_root` and friends) are shared rather than
  duplicated.
- **Tools** in `ork-integrations/src/code_tools.rs` so the LLM
  catalog change is one extra match arm in the existing executor.
  No new tool family.

`WorkspaceHandle` itself ships in
[`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
(extending the existing module rather than creating a new one) so
that ADRs 0028 and 0030 can `use ork_core::ports::workspace::WorkspaceHandle`
without a circular path through whichever ADR lands first.

## Acceptance criteria

- [ ] Type `WorkspaceHandle` (with the fields shown in `Decision`)
      and `WorkspaceId` defined in
      [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
      and re-exported from `ork_core::ports`.
- [ ] Trait `WorkspaceEditor` defined at
      `crates/ork-core/src/ports/workspace_editor.rs` with the
      signature shown in `Decision`, plus the supporting types
      (`CreateOpts`, `PatchOpts`, `FileWrite`, `PatchOutcome`,
      `ContentHash`, `WorkspaceEditError`).
- [ ] `RepoWorkspace::read_file` extended (or a sibling
      `read_file_with_hash` added) so callers receive a
      `ContentHash` alongside the bytes.
- [ ] `LocalWorkspaceEditor` implements the port at
      `crates/ork-integrations/src/workspace_editor.rs`, sharing
      `resolve_under_root` (or its successor) with the read side.
- [ ] Sandboxing tests
      `crates/ork-integrations/tests/workspace_editor_sandbox.rs`
      cover: absolute path rejection, `..` traversal rejection,
      symlink-escape rejection (with a symlink fixture pointing
      outside `ws.root`), `.git/` rejection, cross-tenant root
      rejection.
- [ ] Concurrency test
      `crates/ork-integrations/tests/workspace_editor_concurrent.rs`
      writes a file, mutates it out-of-band, then asserts a second
      `update_file` call with the original `expected_hash` returns
      `Conflict` carrying a non-empty `captured_diff`.
- [ ] Patch test
      `crates/ork-integrations/tests/workspace_editor_patch.rs`
      covers: clean apply across multiple files, atomic rollback
      when one hunk rejects, `strict_paths` honouring an allow-list,
      pre-image hash mismatch surfacing as `Conflict` not
      `PatchReject`.
- [ ] `CodeToolExecutor::descriptors` returns `write_file` and
      `apply_patch` descriptors with the JSON Schemas shown in
      `Decision`.
- [ ] `CodeToolExecutor::execute` dispatches `write_file` and
      `apply_patch` to a `WorkspaceEditor` resolved from
      `AgentContext::workspace` and surfaces
      `WorkspaceEditError::Conflict` as a structured tool result
      `{ "error": "conflict", "captured_diff": "..." }` rather than
      a raw `OrkError::Validation`.
- [ ] `cargo test -p ork-integrations workspace_editor::` is green.
- [ ] `cargo test -p ork-core ports::workspace_editor::` is green.
- [ ] `WorkspaceEditor` and `WorkspaceHandle` are documented in
      `crates/ork-core/src/ports/mod.rs` doc-comments.
- [ ] [`README.md`](README.md) ADR index row added for 0029.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip-to-Accepted.

## Consequences

### Positive

- Closes the hard blocker on coding agents: the
  "implement-this-ADR" template in [`workflow-templates/`](../../workflow-templates/)
  becomes runnable end-to-end once 0028/0030 land alongside this ADR.
- Establishes `WorkspaceHandle` as the shared abstraction across the
  three "agent does work on a checkout" ADRs (0028 / 0029 / 0030).
  Without it each ADR would invent its own `(tenant, repo, root)`
  triple and they would inevitably drift.
- Optimistic concurrency means an agent that sees a `Conflict` can
  recover deterministically (re-read → rebuild diff → reapply)
  without operator involvement. This is the same loop human
  developers run when `git pull` collides with local edits.
- The port boundary keeps the patch crate (`diffy` or similar) out
  of `ork-core`. Replacing it with a different patch backend later
  is a one-crate change.

### Negative / costs

- A second on-disk tree per task (alongside the read-only cache
  under `<cache_dir>/<tenant>/`) doubles disk usage for active runs.
  Garbage collection of `_tasks/<tenant>/<run_id>/` on terminal
  states is mandatory; orphaned trees from crashed runs need a
  sweeper, which is currently nobody's problem and likely lands as
  follow-up work in ADR 0019 (scheduled tasks).
- BLAKE3 hashing is fast but not free: a 5 MB file hashes in
  ~2 ms. Workflows that read hundreds of files per turn will pay
  hundreds of milliseconds of hashing per turn. Acceptable, but
  worth measuring (ADR [0022](0022-observability.md) should record
  per-tool latency histograms).
- Optimistic hashing surfaces a new tool-result variant
  (`{"error": "conflict", ...}`) that workflow authors and the
  verifier (ADR [0025](0025-typed-output-validation-and-verifier-agent.md))
  must learn to expect. The verifier may otherwise classify a
  conflict as a model error and trigger a useless rewrite.
- Symlink-escape detection requires a per-component `lstat` walk on
  every write. On deep paths this is observable in micro-benchmarks
  but should not matter in practice.

### Neutral / follow-ups

- ADR 0031 (multi-file transactional edits) builds on
  `apply_patch`'s atomic-per-call guarantee; this ADR deliberately
  stops short of cross-call transactions.
- AST-aware refactors (rename-symbol, organize-imports) are not
  covered; a separate ADR can introduce them as a `SemanticEditor`
  port that may delegate to `WorkspaceEditor` for the actual file
  writes.
- Binary-file writes are supported by the trait
  (`content: &[u8]`) but the `write_file` *tool* accepts strings
  only; a future ADR can introduce a base64 path or a separate
  `write_binary_file` tool when the need is concrete.

## Alternatives considered

- **Reuse `ArtifactStore` (ADR 0016) as the write surface.**
  Rejected: artifacts are content-addressed by `(name, version)`
  with no notion of path-relative-to-a-repo, and they round-trip
  through S3/GCS by design. The shell port (ADR 0028) cannot run
  `cargo test` against an artifact; it needs a real working tree.
- **Expose write tools through an MCP filesystem server.**
  Rejected: contradicts §3 invariant on the MCP/native split.
  Filesystem semantics for the *primary working copy of an active
  ork run* are an internal contract, not a swappable external tool.
  An optional MCP filesystem catalog for *external* directories
  remains compatible with this decision and can land later.
- **Exclusive lock per `WorkspaceHandle`** (one writer at a time,
  blocking). Rejected for the reasons in the §Concurrent-write
  safety subsection: it doesn't help against external mutators,
  serialises the agent's own reads behind unrelated writes, and
  encourages workflow authors to hold the handle longer than
  necessary.
- **Last-writer-wins with diff capture.** Rejected: silently
  overwrites human or shell edits between the agent's read and
  write. The "captured diff" is forensic, not preventative; agents
  don't notice the loss until something downstream breaks.
- **Pessimistic hash + retry inside the editor.** Rejected:
  baking a retry loop into the port hides the conflict from the
  agent, which means the agent can't reconcile semantic conflicts
  (only byte-identical re-applications work). Surfacing
  `Conflict` to the caller preserves the option to retry *or* to
  hand off to a human via ADR [0027](0027-human-in-the-loop.md).
- **Add write methods to `RepoWorkspace` directly** rather than
  introducing a separate port. Rejected: the read cache and the
  task-scoped working copy have different lifetimes (per-tenant
  vs. per-run) and different sandbox postures. Mixing them would
  force every read to be aware of the write workspace and vice
  versa.

## Affected ork modules

- [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
  — adds `WorkspaceHandle`, `WorkspaceId`; extends `read_file` to
  surface `ContentHash`.
- `crates/ork-core/src/ports/workspace_editor.rs` — new file with
  `WorkspaceEditor` trait and supporting types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — exports the new module.
- [`crates/ork-integrations/src/workspace.rs`](../../crates/ork-integrations/src/workspace.rs)
  — exposes `resolve_under_root` (or factor a sibling
  `path_sandbox` module) so the editor implementation can share it.
- `crates/ork-integrations/src/workspace_editor.rs` — new
  `LocalWorkspaceEditor` implementation.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — adds `write_file` and `apply_patch` descriptors and dispatch.
- [`crates/ork-integrations/Cargo.toml`](../../crates/ork-integrations/Cargo.toml)
  — adds `diffy` and `blake3` dependencies.
- [`crates/ork-integrations/tests/`](../../crates/ork-integrations/tests/)
  — three new test files (sandbox, concurrent, patch).

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Claude Code | `Edit` tool's `old_string` exact-match guard, used as a content-precondition before every overwrite | `update_file(expected_hash)` and `apply_patch` pre-image hashing |
| Aider | Per-file SHA tracking with conflict surfaces ("file changed since I last saw it") | `WorkspaceEditError::Conflict { captured_diff }` |
| Cursor | Sandboxed workspace root + symlink-escape rejection in the editor | §Sandboxing rules |
| Solace Agent Mesh | No equivalent — SAM has artifact services but no first-class working-copy editor | Net-new for ork |

## Open questions

- Should `WorkspaceHandle` be allocated by ADR 0030 alone
  (`GitOperations::open_workspace`) or should there be a "no-git"
  variant for non-repo edits (e.g. editing rendered docs in a
  scratch dir)? Lean toward git-only for v1; revisit when a
  no-git use case appears.
- Should `apply_patch` accept binary diffs? `diffy` does; the
  current tool surface does not. Likely a follow-up once binary
  `write_file` is also supported.
- Does the `Conflict.captured_diff` need a size cap (large files
  produce large diffs which then become large LLM inputs)? Lean
  toward truncating to ~64 KiB with a marker, mirroring the tool
  result truncation policy in
  ADR [0011](0011-native-llm-tool-calling.md).
- Where does the `_tasks/` GC actually live? Most likely ADR 0019
  (scheduled tasks) plus a startup sweeper, but this ADR does not
  fix the answer.

## References

- A2A spec: <https://a2a-protocol.org/latest/specification/>
- BLAKE3 spec: <https://github.com/BLAKE3-team/BLAKE3-specs>
- `diffy` crate: <https://crates.io/crates/diffy>
- Aider — sha-based edit verification:
  <https://aider.chat/docs/troubleshooting/edit-errors.html>
- Related ADRs: [0002](0002-agent-port.md),
  [0010](0010-mcp-tool-plane.md),
  [0011](0011-native-llm-tool-calling.md),
  [0016](0016-artifact-storage.md) (contrast — artifact store, not
  working copy),
  [0020](0020-tenant-security-and-trust.md),
  [0028](0028-shell-executor.md) (shares `WorkspaceHandle`),
  [0030](0030-git-operations.md) (provisions `WorkspaceHandle`),
  [0031](0031-multi-file-transactional-edits.md) (builds on
  `apply_patch`).
