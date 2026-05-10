# 0061 — Mutable workspace + edit/write/shell tool plane

- **Status:** Proposed
- **Date:** 2026-05-10
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0010, 0016, 0020, 0021, 0048, 0049, 0051, 0052, 0053
- **Supersedes:** —

## Context

After [`0048`](0048-pivot-to-code-first-rig-platform.md) ork is a
code-first agent platform with a Mastra-shaped developer surface
([`0049`](0049-orkapp-central-registry.md)) and rig-driven agent
loops ([`0052`](0052-code-first-agent-dsl.md)). The runtime is in
many ways stronger than Claude Code's harness — typed sub-agent
delegation, threaded memory ([`0053`](0053-memory-working-and-semantic.md)),
multi-tenant RBAC ([`0021`](0021-rbac-scopes.md)) — but the
**coding-agent tool surface is missing**.

The pre-pivot ADRs that owned this surface were rolled into 0048
without replacement:

- [`0028`](0028-shell-executor-and-test-runners.md) — `ShellExecutor`,
  `run_command`, `run_tests`. Status: Superseded by 0048.
- [`0029`](0029-workspace-file-editor.md) — `WorkspaceHandle`,
  `WorkspaceEditor`, `update_file`, `write_file`, `apply_patch`.
  Status: Superseded by 0048.
- [`0030`](0030-git-operations.md) — `GitOperations`, worktree
  provisioning, `git_*` tool family. Status: Superseded by 0048.

What ork ships today, all read-only, lives in
[`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
and uses
[`crates/ork-integrations/src/workspace.rs`](../../crates/ork-integrations/src/workspace.rs)
as a per-tenant **read cache**: `ensure_clone()` runs
`git fetch --depth && git reset --hard origin/<branch>` on every
call, wiping any local changes. The cache is the wrong shape for
mutation: an edit made on top of it disappears on the next refresh,
and there is no notion of "the file the agent just observed."

Three concrete consequences:

1. The `read_file` tool ([`code_tools.rs:63`](../../crates/ork-integrations/src/code_tools.rs#L63))
   returns a plain string capped at 64 KiB. No line numbers, no
   offset/limit pagination. The model cannot cite line ranges
   without re-deriving them, and any downstream `edit_file` would
   not know what the agent saw.
2. There is **no edit, no write, no patch, no shell** tool.
   Every coding-agent workflow template under
   [`workflow-templates/`](../../workflow-templates/) stalls the
   moment the planner needs to mutate a file or run `cargo test`.
3. The `codebase-memory-mcp` server (a SAM-compatible knowledge-graph
   tool) can be wired through ADR 0010 as an external MCP, but it is
   not a default and there is no native `codebase_search` tool that
   wraps it. Agents end up needing to know whether their search
   route is local-or-MCP, which is exactly the kind of detail
   [`0049`](0049-orkapp-central-registry.md) is supposed to hide.

This ADR restores the coding-agent tool plane on top of the
post-pivot substrate (`OrkApp` + the `tool()` DSL +
[`0051`](0051-code-first-tool-dsl.md)). It is **not** a re-issue of
0028/0029/0030 — the shape is updated for the code-first registry,
typed `Tool<I, O>` builders, the read-before-edit invariant the
post-Claude-Code generation of harnesses settled on, and the
codebase-graph integration the team has been routing through MCP.

## Decision

ork **introduces** a coordinated mutable-workspace + edit + write +
shell + codebase-search tool surface, registered through
[`0049`](0049-orkapp-central-registry.md) and authored with the
[`0051`](0051-code-first-tool-dsl.md) `tool()` builder.

Five new ports in `ork-core`, one new tool family registered through
`OrkAppBuilder`, and one MCP server spec wired by default in dev
configs:

| Port (in `ork-core/src/ports/`) | Concrete impl (in `ork-integrations/`) | Tool(s) |
| ------------------------------- | -------------------------------------- | ------- |
| `WorkspaceProvisioner` | `LocalWorkspaceProvisioner` | (lifecycle, no LLM tool) |
| `WorkspaceReader` (extends `read_file`) | `LocalWorkspaceReader` | `read_file` (v2) |
| `WorkspaceEditor` | `LocalWorkspaceEditor` | `edit_file`, `write_file`, `apply_patch` |
| `ShellExecutor` | `LocalShellExecutor` | `run_command` |
| `CodebaseIndex` | `PgVectorCodebaseIndex` / `McpCodebaseIndex` | `codebase_search` |

Tools are authored with `ork_tool::tool(...)` and registered via a
single `OrkAppBuilder::coding_tools(...)` convenience that pulls the
five ports out of the `OrkApp` adapter set. The hexagonal boundary
([`AGENTS.md`](../../AGENTS.md) §3 invariant 5) is maintained:
`ork-core` defines the ports; `ork-integrations`, `ork-persistence`,
and `ork-mcp` provide the impls; `ork-app` does the wiring.

### `WorkspaceHandle` and `WorkspaceProvisioner`

`WorkspaceHandle` from ADR 0029 is restated here, lightly adapted to
the post-pivot world (run id is now a `RunId` from `ork-core::workflow`
rather than the pre-pivot string):

```rust
// crates/ork-core/src/ports/workspace.rs

#[derive(Clone, Debug)]
pub struct WorkspaceHandle {
    pub id: WorkspaceId,
    pub tenant_id: TenantId,
    pub run_id: RunId,
    pub repo: String,
    pub root: PathBuf,           // canonicalised under <state_dir>/_runs/<tenant>/<run>/
    pub head_commit: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceId(pub uuid::Uuid);

#[async_trait::async_trait]
pub trait WorkspaceProvisioner: Send + Sync {
    /// Allocate a fresh mutable working copy for one workflow run.
    /// Idempotent on (tenant_id, run_id, repo, base_branch).
    async fn open(
        &self,
        tenant_id: TenantId,
        run_id: RunId,
        req: OpenWorkspaceRequest,
    ) -> Result<WorkspaceHandle, OrkError>;

    /// Tear down a workspace. Always called from `Drop` on the
    /// run's `WorkspaceLease`; idempotent.
    async fn close(&self, ws: &WorkspaceHandle) -> Result<(), OrkError>;
}

#[derive(Clone, Debug)]
pub struct OpenWorkspaceRequest {
    pub repo: String,
    pub base_branch: String,
    pub task_branch: Option<String>,         // default: ork/run/<run_id>
}
```

`LocalWorkspaceProvisioner` (in `ork-integrations`) builds the
mutable copy as a `git worktree add` off the existing read cache —
shared `.git/objects`, per-run working tree under
`<state_dir>/_runs/<tenant_id>/<run_id>/<repo>/`. The read cache
([`workspace.rs`](../../crates/ork-integrations/src/workspace.rs))
is unchanged; this ADR adds a second, mutable surface alongside it.

**Lifecycle wiring.** `OrkApp::run_agent` and `OrkApp::run_workflow`
([`0049`](0049-orkapp-central-registry.md)) construct a
`WorkspaceLease` per run when a `WorkspaceProvisioner` is registered
and the agent's tool list contains any of the workspace-bound tools
below. The lease holds the `WorkspaceHandle` and a `Drop` impl that
calls `provisioner.close(...)`. On terminal state (`Completed` /
`Failed` / `Cancelled`) the temp directory is removed; if the
process crashes mid-run the next `OrkApp` boot's GC sweeper
reclaims `_runs/<tenant>/<run_id>/` directories whose runs are no
longer in `Running` state. GC sweep is the
[`0019`](0019-scheduled-tasks.md) scheduler's responsibility once it
ships; until then a startup sweep in `LocalWorkspaceProvisioner::new`
covers the gap.

### `read_file` (v2): line numbers + pagination + content hash

```rust
// crates/ork-core/src/ports/workspace_reader.rs

#[async_trait::async_trait]
pub trait WorkspaceReader: Send + Sync {
    async fn read(
        &self,
        ws: &WorkspaceHandle,
        rel_path: &str,
        opts: ReadOptions,
    ) -> Result<ReadOutput, OrkError>;
}

#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    /// 1-based line number to start at; default 1.
    pub offset: Option<u32>,
    /// Max lines to return; default 2000, capped at 5000.
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadOutput {
    pub path: String,
    pub total_lines: u32,
    pub returned_offset: u32,
    pub returned_lines: u32,
    pub truncated: bool,
    pub lines: Vec<NumberedLine>,
    pub content_hash: ContentHash,        // BLAKE3 of full file bytes
    pub kind: FileKind,                   // Text | Binary { hint: mime } | TooLarge
}

#[derive(Clone, Debug, Serialize)]
pub struct NumberedLine { pub n: u32, pub text: String }

#[derive(Clone, Debug, Serialize)]
pub enum FileKind {
    Text,
    /// File is non-text; the tool returns kind + mime + size only.
    /// Image rendering is deferred (see Open questions).
    Binary { mime: String, bytes: u64 },
    /// File exceeds the per-call hard cap (default 16 MiB).
    TooLarge { bytes: u64 },
}
```

Reading a binary or oversized file does not error; the model gets a
typed `kind` it can react to. Each successful **text** read also
records `(run_id, ws.id, rel_path) → content_hash` in the per-run
**read set** (held inside `LocalWorkspaceEditor`), which is what
`edit_file` and `write_file` consult below.

The `read_file` tool's wire shape is `ReadOutput` serialised; the
model receives line objects directly rather than `cat -n`-formatted
text, because the rig-driven agent loop ([`0052`](0052-code-first-agent-dsl.md))
sees JSON tool responses anyway.

### `WorkspaceEditor` and the read-before-edit invariant

```rust
// crates/ork-core/src/ports/workspace_editor.rs

#[async_trait::async_trait]
pub trait WorkspaceEditor: Send + Sync {
    /// Exact-match string replace. Fails if `old_string` is not
    /// uniquely present (unless `replace_all = true`) or if
    /// `(run_id, ws.id, rel_path)` is not in the read set.
    async fn edit(
        &self,
        ws: &WorkspaceHandle,
        run_id: RunId,
        rel_path: &str,
        edit: EditRequest,
    ) -> Result<FileWrite, WorkspaceEditError>;

    /// Whole-file write. For an existing file requires the path to
    /// be in the read set; for a new file requires `create = true`.
    async fn write(
        &self,
        ws: &WorkspaceHandle,
        run_id: RunId,
        rel_path: &str,
        content: &[u8],
        opts: WriteOptions,
    ) -> Result<FileWrite, WorkspaceEditError>;

    /// Apply a unified diff atomically across one or more files.
    /// Pre-image hashing seeds the read set automatically — agents
    /// do not need to call `read` first for paths the diff covers.
    async fn apply_patch(
        &self,
        ws: &WorkspaceHandle,
        run_id: RunId,
        diff: &str,
        opts: PatchOptions,
    ) -> Result<PatchOutcome, WorkspaceEditError>;
}

#[derive(Clone, Debug)]
pub struct EditRequest {
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,                // default false
}

#[derive(Clone, Debug, Default)]
pub struct WriteOptions {
    pub create: bool,                     // false ⇒ must already exist
    pub mkdir_parents: bool,              // default true
}

#[derive(Clone, Debug, Default)]
pub struct PatchOptions {
    pub context_fuzz: u8,                 // 0..=3, default 0
}

#[derive(Clone, Debug)]
pub struct FileWrite {
    pub rel_path: String,
    pub before_hash: Option<ContentHash>, // None on create
    pub after_hash: ContentHash,
    pub bytes_written: u64,
    pub kind: WriteKind,                  // Created | Modified | Deleted
}

#[derive(thiserror::Error, Debug)]
pub enum WorkspaceEditError {
    /// `(run_id, ws.id, rel_path)` is not in the read set, and the
    /// caller did not supply a pre-image (apply_patch). The agent
    /// must `read_file` (or `apply_patch` with pre-image lines)
    /// before mutating this path.
    #[error("invariant: path not read in this run")]            InvariantViolation,
    #[error("path escapes workspace root")]                     PathEscape,
    #[error("absolute paths are not permitted")]                AbsolutePath,
    #[error("path traverses a symlink that escapes root")]      SymlinkEscape,
    #[error("writes to .git/ are forbidden")]                   ProtectedPath,
    #[error("file already exists: {0}")]                        AlreadyExists(String),
    #[error("file not found: {0}")]                             NotFound(String),
    /// Disk content changed since the read; carries a unified
    /// diff between expected and current bytes for reconciliation.
    #[error("content changed since read")]
    Conflict { expected: ContentHash, actual: ContentHash, captured_diff: String },
    /// `old_string` matches zero or >1 occurrences and
    /// `replace_all` is false.
    #[error("old_string not uniquely matched: {0}")]            NotUniquelyMatched(String),
    #[error("patch did not apply cleanly: {0}")]                PatchReject(String),
    #[error("io: {0}")]                                         Io(#[from] std::io::Error),
}
```

The **read set** is the per-run map
`HashMap<(WorkspaceId, String), ContentHash>` held by
`LocalWorkspaceEditor`. It is populated by:

- `WorkspaceReader::read` on success (text reads only).
- `WorkspaceEditor::edit` and `::write` on success (the post-write
  hash becomes the new entry, so an agent can edit-then-edit
  without re-reading).
- `WorkspaceEditor::apply_patch` on success (the post-write hash
  for every touched path).

It is consulted by:

- `edit` and `write` for existing files: the entry must exist and
  match the on-disk hash; mismatch returns `Conflict` with a
  captured diff (recoverable: the agent reads, rebuilds, retries).
  Missing entry returns `InvariantViolation` (also recoverable: the
  agent reads first).

Pre-image hashing in `apply_patch` is the same trick ADR 0029
documented: a unified diff carries the pre-image, so the editor
hashes the pre-image and compares against current bytes, no caller
bookkeeping required.

The invariant is the single biggest reason Claude Code's `Edit`
tool stays useful at scale: it forecloses the "agent hallucinates an
edit against text that isn't there" failure mode entirely. Every
modern coding-agent harness that has run on real codebases
(Cursor, Aider, OpenHands, Devin) has converged on some variant of
this.

### `ShellExecutor` and `run_command`

```rust
// crates/ork-core/src/ports/shell.rs

#[async_trait::async_trait]
pub trait ShellExecutor: Send + Sync {
    async fn execute(
        &self,
        ws: &WorkspaceHandle,
        req: ShellRequest,
    ) -> Result<ShellResult, OrkError>;
}

#[derive(Clone, Debug)]
pub struct ShellRequest {
    pub cwd: String,                     // relative to ws.root; "" = root
    pub argv: Vec<String>,               // argv[0] is program; never sh -c
    pub env: Vec<(String, String)>,      // added on top of empty base env
    pub timeout: Duration,               // default 120s, max 600s
    pub max_output_bytes: Option<usize>, // default 64 KiB
    pub stdin: Option<Vec<u8>>,
    pub network: NetworkPolicy,
}

#[derive(Clone, Debug)]
pub enum NetworkPolicy {
    /// Default. The child runs in a network namespace with
    /// loopback only (Linux) or with PF rules denying egress
    /// (macOS dev). Falls back to host-network with a warning if
    /// the platform cannot enforce, recorded in the audit log.
    Deny,
    /// Host network. Requires tenant config flag
    /// `shell_network_allow = true`.
    Allow,
    /// Allow-list of CIDRs / hostnames; tenant config must list
    /// every entry under `shell_network_allowlist`.
    AllowList(Vec<String>),
}

#[derive(Clone, Debug, Serialize)]
pub struct ShellResult {
    pub exit_code: i32,
    pub termination: ShellTermination,
    pub stdout: String,                  // tail-truncated to max_output_bytes
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_artifact_id: Option<ArtifactId>,
    pub stderr_artifact_id: Option<ArtifactId>,
    pub duration_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum ShellTermination { Exited, Signaled(i32), TimedOut, Cancelled }
```

The sandbox contract follows ADR 0028's posture: argv-only (no
`sh -c`), `env_clear()` then add `PATH`/`HOME`/`LANG` plus the
tenant allowlist plus `req.env`, per-call timeout enforced via
`tokio::time::timeout`, SIGTERM→SIGKILL ladder on expiry,
`AgentContext::cancel` wired through `tokio::select!`,
output captured concurrently into bounded buffers with overflow
spilled to ADR-0016 artifacts and tail kept in-band, per-tenant
concurrency cap (default 4).

Network defaults to **Deny** (departure from 0028, which left the
host network on). The post-Claude-Code consensus is that "agent
runs `cargo test` and accidentally touches the public network" is
both a security risk and a flake source; tenants opt into network
explicitly. On Linux we use a network namespace with loopback only;
on macOS dev hosts we use a `pf` rule set; on platforms where
neither is available, the executor logs a warning to the audit
stream and falls back to host network so the developer experience
does not silently break.

RBAC is enforced before spawn:

| Scope | Required for |
| ----- | ------------ |
| `tool:run_command:invoke` | Any `run_command` call. |
| `shell:cmd:<program>:invoke` | Per-binary allow-list for `argv[0]`. |
| `shell:cmd:*:invoke` | Wildcard. |
| `shell:network:allow:<tenant>` | Required when `network != Deny`. |

ADR [`0021`](0021-rbac-scopes.md)'s `ScopeChecker` runs on
`AgentContext.identity.scopes`; missing scope returns
`OrkError::Unauthorized` with `kind = "scope_denied"`. Negative
tests assert the rejection.

### `codebase_search` and the `CodebaseIndex` port

```rust
// crates/ork-core/src/ports/codebase_index.rs

#[async_trait::async_trait]
pub trait CodebaseIndex: Send + Sync {
    async fn search(
        &self,
        ws: &WorkspaceHandle,
        req: CodebaseSearchRequest,
    ) -> Result<CodebaseSearchResult, OrkError>;
}

#[derive(Clone, Debug)]
pub struct CodebaseSearchRequest {
    pub query: String,
    pub mode: SearchMode,                // Semantic | Lexical
    pub top_k: u32,                      // capped at 100
    pub path_prefix: Option<String>,     // restrict to a subtree
}

#[derive(Clone, Copy, Debug)]
pub enum SearchMode { Semantic, Lexical }

#[derive(Clone, Debug, Serialize)]
pub struct CodebaseSearchResult {
    pub hits: Vec<CodebaseHit>,
    pub backend: &'static str,           // "pgvector" | "mcp:<server_id>"
}

#[derive(Clone, Debug, Serialize)]
pub struct CodebaseHit {
    pub path: String,
    pub line: Option<u32>,
    pub score: f32,
    pub snippet: String,
    pub kind: HitKind,                   // Symbol | Definition | Match
}
```

Two implementations ship:

1. **`PgVectorCodebaseIndex`** (in `ork-persistence`). Uses the
   pgvector backend already wired up by ADR
   [`0053`](0053-memory-working-and-semantic.md) for semantic
   memory; embeddings keyed on `(tenant_id, repo, path,
   chunk_index)` with the same embedder configured in `OrkApp`.
   Indexing is incremental: `LocalWorkspaceProvisioner::open`
   schedules a background indexer for the head commit if the
   `(tenant, repo, commit)` row is absent.
2. **`McpCodebaseIndex`** (in `ork-mcp`). Routes the search call
   to a registered MCP server (default `codebase-memory-mcp`,
   below) via `tools/call`. The MCP server's tool schema is
   adapted to the `CodebaseSearchRequest` shape inside
   `ork-mcp`; the agent never sees the MCP-specific shape.

Resolution at `OrkAppBuilder::build()`:

```text
if app.vectors().is_some() && app.config.codebase_index_backend != "mcp_only":
    use PgVectorCodebaseIndex
elif app.mcp_server("codebase-memory") is registered:
    use McpCodebaseIndex
else:
    codebase_search.gate(false)   // tool not exposed to agents
```

The `backend` field in the result is informative (Studio renders
it; the audit log records it) but the input/output shape is
identical across backends. A workflow that runs against pgvector
in dev and MCP in prod sees the same `CodebaseHit` stream.

### Tool registrations

All six tools are authored with the [`0051`](0051-code-first-tool-dsl.md)
`tool()` builder and registered through a single
`OrkAppBuilder::coding_tools(...)` convenience that pulls the five
ports out of the `OrkApp` adapter set. Authoring shape (one
example; the rest follow the same pattern):

```rust
#[derive(Deserialize, JsonSchema)]
pub struct EditFileIn {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct EditFileOut {
    pub path: String,
    pub before_hash: Option<String>,     // hex-encoded BLAKE3
    pub after_hash: String,
    pub bytes_written: u64,
}

pub fn edit_file_tool(editor: Arc<dyn WorkspaceEditor>) -> impl IntoToolDef {
    tool("edit_file")
        .description("Replace an exact substring in a file. Requires \
                      the file to have been read by `read_file` (or \
                      covered by `apply_patch`'s pre-image) earlier \
                      in this run.")
        .input::<EditFileIn>()
        .output::<EditFileOut>()
        .fatal_on(|err| matches!(err, OrkError::Unauthorized(_)))
        .execute(move |ctx, args| {
            let editor = editor.clone();
            async move {
                let ws = ctx.workspace().ok_or(OrkError::Validation(
                    "no workspace bound to this run".into()))?;
                let res = editor.edit(
                    &ws, ctx.run_id(), &args.path,
                    EditRequest {
                        old_string: args.old_string,
                        new_string: args.new_string,
                        replace_all: args.replace_all,
                    },
                ).await;
                map_edit_error_to_tool_result(res)
            }
        })
}
```

`map_edit_error_to_tool_result` follows ADR 0010's failure model:
`InvariantViolation`, `Conflict`, `NotUniquelyMatched`,
`AlreadyExists`, `NotFound`, `PatchReject` are non-fatal (the LLM
sees a structured `{ "error": "...", ... }` and may recover).
`Unauthorized` (scope denied) and `PathEscape`/`AbsolutePath`/
`SymlinkEscape`/`ProtectedPath` are fatal — they signal an agent
trying to do something the platform must refuse, not a recoverable
mistake.

The full surface registered by `coding_tools(...)`:

| Tool name | Wraps | Scope (per [`0021`](0021-rbac-scopes.md)) |
| --------- | ----- | ----------------------------------------- |
| `read_file` | `WorkspaceReader::read` | `tool:read_file:invoke` |
| `edit_file` | `WorkspaceEditor::edit` | `tool:edit_file:invoke` |
| `write_file` | `WorkspaceEditor::write` | `tool:write_file:invoke` |
| `apply_patch` | `WorkspaceEditor::apply_patch` | `tool:apply_patch:invoke` |
| `run_command` | `ShellExecutor::execute` | `tool:run_command:invoke` (+ shell:cmd:*) |
| `codebase_search` | `CodebaseIndex::search` | `tool:codebase_search:invoke` |

Read-only tools (`read_file`, `codebase_search`) default to "always
on" for any agent that registers `coding_tools`; mutating tools
(`edit_file`, `write_file`, `apply_patch`, `run_command`) default to
omitted from the per-agent tool list and require explicit opt-in
in the agent's builder via
`CodeAgent::builder("...").tool(edit_file_tool(...))`. This mirrors
ADR 0028's "always available read-only family vs. gated mutating
family" split.

### Default MCP wiring: `codebase-memory-mcp`

`config/default.toml` ships an `[mcp.servers]` entry for
`codebase-memory-mcp` under the `dev` environment profile, off by
default in `prod`:

```toml
[mcp.servers.codebase-memory]
enabled_in = ["dev"]
transport = "stdio"
command = "codebase-memory-mcp"
args = []
description = """
Codebase knowledge graph: search_code, search_graph, query_graph, \
get_architecture. Used as the fallback backend for the native \
`codebase_search` tool when pgvector is not configured. Operators \
opt out by setting `enabled_in = []` or removing the entry.
"""
```

This composes with [`0010`](0010-mcp-tool-plane.md)'s three-source
registration (tenant > workflow > global): operators opt out by
deleting the global entry or by setting per-tenant
`mcp_servers.codebase-memory.enabled = false`. ADR 0010's existing
plumbing carries this; nothing in `ork-mcp` needs to change beyond
the config file.

`McpCodebaseIndex` (in `ork-mcp`) consults
[`McpClient::list_tools_for(server_id="codebase-memory")`](../../crates/ork-mcp/src/client.rs)
on first use, picks the appropriate `search_code` / `search_graph`
tool based on `SearchMode`, and adapts the wire shape into
`CodebaseSearchResult`. The MCP-specific tool names (`search_code`,
etc.) are kept inside `ork-mcp`; the agent only sees `codebase_search`.

### Hexagonal placement summary

Per [`AGENTS.md`](../../AGENTS.md) §3 invariant 5:

- **Ports** in `ork-core/src/ports/{workspace.rs,
  workspace_reader.rs, workspace_editor.rs, shell.rs,
  codebase_index.rs}`. No `tokio::process`, no `git2`, no `sqlx`,
  no `rmcp` imports.
- **Implementations**: `LocalWorkspaceProvisioner`,
  `LocalWorkspaceReader`, `LocalWorkspaceEditor`,
  `LocalShellExecutor` in `ork-integrations`;
  `PgVectorCodebaseIndex` in `ork-persistence`;
  `McpCodebaseIndex` in `ork-mcp`. Each adapter crate already
  carries its infra dependency.
- **Wiring**: `ork-app` exposes `OrkAppBuilder::coding_tools(...)`
  which takes the five ports and registers the six tools.
  `ork-app` does not import infra crates directly; the user's
  `main.rs` constructs the adapters and passes them in.

### Acceptance criteria

- [ ] Trait `WorkspaceProvisioner` defined at
      `crates/ork-core/src/ports/workspace.rs` with the signature
      shown in `Decision`. `WorkspaceHandle` and `WorkspaceId`
      defined in the same module.
- [ ] Trait `WorkspaceReader` defined at
      `crates/ork-core/src/ports/workspace_reader.rs` with
      `ReadOptions`, `ReadOutput`, `NumberedLine`, `FileKind`.
- [ ] Trait `WorkspaceEditor` defined at
      `crates/ork-core/src/ports/workspace_editor.rs` with
      `EditRequest`, `WriteOptions`, `PatchOptions`, `FileWrite`,
      `WriteKind`, `WorkspaceEditError` (including the
      `InvariantViolation` variant).
- [ ] Trait `ShellExecutor` defined at
      `crates/ork-core/src/ports/shell.rs` with `ShellRequest`,
      `ShellResult`, `ShellTermination`, `NetworkPolicy`.
- [ ] Trait `CodebaseIndex` defined at
      `crates/ork-core/src/ports/codebase_index.rs` with
      `CodebaseSearchRequest`, `SearchMode`, `CodebaseSearchResult`,
      `CodebaseHit`, `HitKind`.
- [ ] All five port modules re-exported from
      `crates/ork-core/src/ports/mod.rs`.
- [ ] `LocalWorkspaceProvisioner` defined at
      `crates/ork-integrations/src/workspace_provisioner.rs`,
      shells out to `git worktree add` against the existing
      `GitRepoWorkspace` read cache, and provisions `<state_dir>/_runs/<tenant>/<run>/<repo>/`.
- [ ] `LocalWorkspaceProvisioner::open` rejects with
      `OrkError::Validation` when `repo` is unknown to the tenant's
      `RepositorySpec` set.
- [ ] `LocalWorkspaceProvisioner::close` removes the worktree and
      its directory; idempotent on a missing tree.
- [ ] Test
      `crates/ork-integrations/tests/workspace_provisioner_lifecycle.rs::dropped_lease_removes_temp_dir`
      asserts that dropping the `WorkspaceLease` returned by
      `OrkApp::run_agent` removes `<state_dir>/_runs/<tenant>/<run>/<repo>/`.
- [ ] `LocalWorkspaceReader` returns `ReadOutput.lines` with 1-based
      `n`, honours `offset`/`limit` (1-based, inclusive),
      `truncated = true` when `total_lines > offset + returned_lines`,
      and populates `content_hash` as BLAKE3-256 over the full file
      bytes.
- [ ] `LocalWorkspaceReader` returns `FileKind::Binary` (no
      `lines`) for non-text content; `FileKind::TooLarge` for
      files exceeding the per-call hard cap (default 16 MiB).
- [ ] Read-set wiring: `LocalWorkspaceEditor::edit` returns
      `WorkspaceEditError::InvariantViolation` when called for a
      `(run_id, ws.id, rel_path)` that has not been read in this
      run, verified by
      `crates/ork-integrations/tests/workspace_editor_invariant.rs::edit_before_read_fails`.
- [ ] `LocalWorkspaceEditor::edit` returns
      `WorkspaceEditError::Conflict { captured_diff, .. }` with
      non-empty `captured_diff` when the on-disk hash diverges from
      the read-set hash, verified by
      `crates/ork-integrations/tests/workspace_editor_invariant.rs::edit_after_external_mutation_conflicts`.
- [ ] `LocalWorkspaceEditor::edit` returns
      `WorkspaceEditError::NotUniquelyMatched` when `old_string`
      matches zero or >1 occurrences and `replace_all = false`,
      verified by
      `crates/ork-integrations/tests/workspace_editor_invariant.rs::edit_ambiguous_match_fails`.
- [ ] `LocalWorkspaceEditor::write` requires `create = true` for
      nonexistent paths and the path to be in the read set for
      existing paths; same `InvariantViolation` / `Conflict` shape
      as `edit`.
- [ ] `LocalWorkspaceEditor::apply_patch` is atomic across hunks:
      either every hunk applies (and the read set is updated for
      every touched path) or no file is modified, verified by
      `crates/ork-integrations/tests/workspace_editor_patch.rs::atomic_rollback_on_reject`.
- [ ] `LocalWorkspaceEditor::apply_patch` derives per-path
      `expected_hash` from the unified diff's pre-image and
      surfaces a mismatch as `Conflict`, not `PatchReject`,
      verified by
      `crates/ork-integrations/tests/workspace_editor_patch.rs::pre_image_mismatch_is_conflict`.
- [ ] Sandboxing tests in
      `crates/ork-integrations/tests/workspace_editor_sandbox.rs`
      cover: absolute path rejection, `..` traversal rejection,
      symlink-escape rejection, `.git/` rejection, cross-tenant
      root rejection.
- [ ] `LocalShellExecutor::execute` clears the host environment
      (no `SSH_AUTH_SOCK`, no `AWS_*`, no `GITHUB_TOKEN`) and only
      passes through `PATH`, `HOME`, `LANG`, the tenant allowlist,
      and `req.env`, verified by
      `crates/ork-integrations/tests/shell_executor_sandbox.rs::env_isolation`.
- [ ] `LocalShellExecutor::execute` rejects with
      `OrkError::Validation` when `req.timeout > 600s`, when
      `req.argv` is empty, or when the resolved cwd canonicalises
      outside `ws.root`.
- [ ] `LocalShellExecutor::execute` enforces the timeout via
      SIGTERM→SIGKILL ladder and surfaces `ShellTermination::TimedOut`
      with `exit_code = -1`, verified by
      `crates/ork-integrations/tests/shell_executor_sandbox.rs::times_out_long_sleep`.
- [ ] `LocalShellExecutor::execute` honours
      `AgentContext::cancel`, killing the child and returning
      `ShellTermination::Cancelled`, verified by
      `crates/ork-integrations/tests/shell_executor_sandbox.rs::cancel_token_kills_child`.
- [ ] `LocalShellExecutor::execute` denies network when
      `req.network = Deny` on Linux (network-namespace fallback),
      verified by
      `crates/ork-integrations/tests/shell_executor_network.rs::deny_blocks_egress` (Linux-only).
- [ ] `run_command` tool returns `OrkError::Unauthorized` with
      `kind = "scope_denied"` when the caller lacks
      `tool:run_command:invoke`, verified by
      `crates/ork-integrations/tests/coding_tools_rbac.rs::run_command_requires_scope`.
- [ ] `PgVectorCodebaseIndex` defined at
      `crates/ork-persistence/src/codebase_index.rs`, builds on
      the pgvector setup from ADR 0053, and provides incremental
      indexing keyed on `(tenant_id, repo, commit)`.
- [ ] `McpCodebaseIndex` defined at
      `crates/ork-mcp/src/codebase_index.rs`, routes
      `CodebaseSearchRequest::Semantic` to `search_code` and
      `Lexical` to `search_graph` on the configured MCP server,
      and adapts the response into `CodebaseSearchResult`.
- [ ] Resolution test
      `crates/ork-app/tests/codebase_index_resolution.rs::pg_vector_wins_over_mcp`
      builds an `OrkApp` with both pgvector and the
      `codebase-memory` MCP server registered and asserts the
      resolved backend is `pgvector`.
- [ ] Resolution test
      `crates/ork-app/tests/codebase_index_resolution.rs::mcp_fallback_when_no_pgvector`
      builds an `OrkApp` with only the MCP server and asserts the
      resolved backend is `mcp:codebase-memory` and the tool's
      `CodebaseSearchResult.backend` field reads `"mcp:codebase-memory"`.
- [ ] `OrkAppBuilder::coding_tools(...)` registers all six tools
      and the read-only / mutating split (read-only always on;
      mutating opt-in per agent) verified by
      `crates/ork-app/tests/coding_tools_registration.rs::default_visibility`.
- [ ] End-to-end test
      `crates/ork-agents/tests/coding_agent_e2e.rs::read_then_edit_round_trip`
      builds a `CodeAgent` with `read_file` + `edit_file`, runs it
      against a fixture repo, asserts the agent reads a file then
      edits it, and that the on-disk content reflects the edit
      after the run.
- [ ] End-to-end test
      `crates/ork-agents/tests/coding_agent_e2e.rs::edit_before_read_returns_invariant_violation`
      asserts that an agent calling `edit_file` first sees the
      `InvariantViolation` tool result (and may recover by reading).
- [ ] End-to-end test
      `crates/ork-agents/tests/coding_agent_e2e.rs::run_command_smoke`
      runs `["echo", "hello"]` and asserts the structured result.
- [ ] `config/default.toml` adds the
      `[mcp.servers.codebase-memory]` entry under the `dev`
      environment profile with `enabled_in = ["dev"]`.
- [ ] Per-component crate-level CI greps:
      - `crates/ork-core/src/ports/{shell,workspace,workspace_reader,workspace_editor,codebase_index}.rs`
        do not import `axum`, `sqlx`, `reqwest`, `rmcp`, `rskafka`,
        `tokio::process`, or `git2`.
      - `crates/ork-app/` does not import any of the above either
        (per [`0049`](0049-orkapp-central-registry.md) acceptance).
- [ ] Public API documented in `crates/ork-core/src/ports/mod.rs`
      doc-comments and re-exported from the relevant adapter
      crates' `lib.rs`.
- [ ] [`README.md`](README.md) ADR index row added for `0061`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Closes the hard blocker on coding agents inside ork: the audit
  in this branch's research showed `read_file`, `edit_file`,
  `write_file`, `apply_patch`, `run_command`, `codebase_search`
  all missing or read-only. After this ADR ork hosts coding
  agents with a tool surface comparable to what makes Claude Code
  feel good — line-numbered reads, exact-match edits with the
  read-before-edit invariant, atomic patches, sandboxed shell.
- The read-before-edit invariant + content-hash optimistic
  concurrency forecloses two of the three classes of
  coding-agent-on-real-codebases failure modes (hallucinated
  edits, lost-update overwrites). The third — semantic conflict
  — surfaces as a recoverable `Conflict` rather than silent
  corruption.
- One tool (`codebase_search`) hides the pgvector/MCP split:
  agents and prompt authors do not need to know whether the
  backend is local or remote. Operators choose the backend at
  `OrkApp::build()` time; switching is a config change, not an
  agent rewrite.
- Reuses every post-pivot abstraction: typed `Tool<I, O>`
  builders ([`0051`](0051-code-first-tool-dsl.md)), `OrkApp`
  registration ([`0049`](0049-orkapp-central-registry.md)),
  failure-model classification ([`0010`](0010-mcp-tool-plane.md)),
  RBAC scopes ([`0021`](0021-rbac-scopes.md)), pgvector + the
  `MemoryStore` shape ([`0053`](0053-memory-working-and-semantic.md)).
  No new substrate.
- Per-run `WorkspaceHandle` lease + `Drop`-driven cleanup gives
  filesystem isolation Claude Code only gets through git
  worktrees; ork has the same primitive at a lower layer for free
  because it owns the runtime.

### Negative / costs

- Trust surface expands sharply. Shell + writes + worktree
  provisioning + an MCP fallback for codebase search are four
  new attack surfaces. We mitigate with: argv-only spawning,
  empty base env, network-deny default, per-binary RBAC scopes,
  read-before-edit invariant, sandboxed cwd canonicalisation,
  audit events on every spawn / write / patch. Residual risk is
  real and explicit.
- A second on-disk tree per active run (the worktree) under
  `<state_dir>/_runs/<tenant>/<run>/`. Disk usage scales with
  active runs. Drop-driven cleanup covers the happy path; crash
  recovery depends on a startup sweeper and eventually
  [`0019`](0019-scheduled-tasks.md). Until 0019 lands the sweeper
  in `LocalWorkspaceProvisioner::new` covers the gap.
- The read-set is held in process memory. A long-running run
  with thousands of read paths grows the map; we cap entries at
  1024 per run with LRU eviction, which means a degenerate agent
  can rotate paths out of the read set and trip
  `InvariantViolation` on a path it really did read. This is
  preferred to unbounded growth; the cap is per-tenant
  configurable.
- pgvector indexing of a fresh repo is non-trivial work. The
  background indexer scheduled at `open` time can take seconds to
  minutes for large repos; until it completes, `codebase_search`
  with `mode = Semantic` either falls back to `Lexical` (if
  pgvector advertises partial-index ready) or returns a
  `backend_warming` flag. This is a UX nick the audit log records.
- We are committing to wire shapes for six tools' JSON inputs and
  outputs. Changing them later is a tool-catalog breaking change.
  The shapes are explicit in the schema-generated `Tool<I, O>`
  types so they show up in `OrkApp::manifest()` and the
  auto-generated REST surface ([`0056`](0056-auto-generated-rest-and-sse-surface.md)).
- Network namespace setup on Linux requires `CAP_NET_ADMIN` or
  `unshare(CLONE_NEWNET)` as the spawning process. ork running
  as a non-privileged user without these caps falls back to host
  network with a warning event. Documented as an Open question.

### Neutral / follow-ups

- A separate ADR can land `git_*` tools (status, diff, log,
  commit, branch, checkout) on top of `LocalWorkspaceProvisioner`
  + `LocalShellExecutor`. The shell + worktree substrate is here;
  the parsers and the dedicated `GitOperations` port can ship
  independently. ADR 0030 is the design reference.
- A separate ADR can add `run_tests` (cargo / pytest / jest with
  structured failure parsing) on top of `run_command`. ADR 0028's
  parser surface is the reference; nothing in this ADR blocks it.
- Image-rendering for `read_file` (a `kind = Image` variant
  carrying base64-encoded data) is deferred to a follow-up; the
  shape is forward-compatible because `FileKind` is non-exhaustive
  on the wire (`#[serde(tag = "kind")]`).
- Containerised execution (firejail / bubblewrap / Docker) for
  `LocalShellExecutor` can land later behind the same port; v1
  ships process-level sandboxing.
- A `MCPServer` adapter that re-exposes ork-native tools over
  the MCP wire (so external agents can call `read_file` /
  `edit_file` etc. through MCP) is the symmetric-MCP follow-up
  ADR 0051 already flagged. Out of scope here.

## Alternatives considered

- **Re-issue 0028 + 0029 + 0030 as separate ADRs.** Rejected.
  After the pivot the three are intertwined: the editor's
  read-set, the shell's `ws.root`, the codebase-search index
  scope all share `WorkspaceHandle`. Splitting them across three
  Proposed ADRs invites the same drift the original trio
  produced. One ADR lets the wire shapes and lifecycles be
  designed together.
- **Skip the read-before-edit invariant; rely on optimistic
  hashing alone (ADR 0029's shape).** Rejected. Optimistic
  hashing catches the race between read and write but not the
  hallucinated-edit failure mode where the agent invents an
  `old_string` it never observed. Read-set + hash is the smaller
  invariant that catches both, and it is what Claude Code,
  Cursor, and Aider all converged on. The only cost is the
  in-memory map, capped at 1024 entries per run.
- **Token-based read receipts** instead of a per-run read set.
  Rejected. A `ReadToken` returned by `read_file` and consumed by
  `edit_file` would be cleaner across run boundaries (a token can
  be persisted, an in-memory set cannot), but rig's tool-call
  loop would have to round-trip the token through the LLM, which
  the model has no reason to preserve verbatim. The read-set
  shape is invisible to the LLM, which is what we want.
- **Make `codebase_search` a workflow-level abstraction rather
  than a tool.** Rejected. ADR
  [`0050`](0050-code-first-workflow-dsl.md) workflows compose
  agents; agents compose tools. Putting `codebase_search` at the
  workflow layer makes it inaccessible to a single-step
  CodeAgent. The tool layer is the right placement.
- **Skip pgvector and route everything through MCP.** Rejected.
  The fallback path is fine for dev and for tenants who do not
  want to run a pgvector index; making it the only path adds an
  RPC hop and a process-spawn to every search, and it surrenders
  the integration with ADR 0053's existing pgvector setup. The
  port lets us prefer local when available.
- **Skip MCP and only ship pgvector.** Rejected. The
  `codebase-memory-mcp` ecosystem is real (the team uses it
  today; it surfaces architecture / call graph / dependency
  views that a pgvector chunked-text index does not). The
  fallback keeps the door open without forcing every operator
  onto pgvector.
- **Hard-mount Claude-Code-style tools on the existing
  read-only `code_tools.rs`** without introducing the
  `WorkspaceProvisioner` port. Rejected. The read cache's
  `git reset --hard` posture is fundamentally incompatible with
  mutation; bolting `edit_file` onto it would silently lose work
  every time the cache refreshed.
- **Allow host network by default for `run_command` (ADR 0028's
  posture).** Rejected for v1. The cost of the deny-by-default is
  a config knob tenants flip when they need it; the cost of the
  allow-by-default is exfiltration risk and flake. Post-Claude-Code
  consensus is deny-by-default; we adopt it.

## Affected ork modules

- New: [`crates/ork-core/src/ports/workspace.rs`](../../crates/ork-core/src/ports/workspace.rs)
  (extends; adds `WorkspaceProvisioner`).
- New: `crates/ork-core/src/ports/workspace_reader.rs`.
- New: `crates/ork-core/src/ports/workspace_editor.rs`.
- New: `crates/ork-core/src/ports/shell.rs`.
- New: `crates/ork-core/src/ports/codebase_index.rs`.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-exports.
- New: `crates/ork-integrations/src/workspace_provisioner.rs`,
  `workspace_reader.rs`, `workspace_editor.rs`,
  `shell_executor.rs`.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — re-shaped to author the six new tools with the
  [`0051`](0051-code-first-tool-dsl.md) builder; the read-only
  `code_search` / `list_tree` tools stay (rg-backed lexical
  search remains useful as a fallback when pgvector is warming).
- [`crates/ork-integrations/Cargo.toml`](../../crates/ork-integrations/Cargo.toml)
  — adds `diffy`, `blake3`, and (Linux-only) `nix` for network-namespace
  setup.
- New: `crates/ork-persistence/src/codebase_index.rs`
  (`PgVectorCodebaseIndex`).
- New: `crates/ork-mcp/src/codebase_index.rs`
  (`McpCodebaseIndex`).
- [`crates/ork-app/src/lib.rs`](../../crates/ork-app/src/lib.rs)
  — `OrkAppBuilder::coding_tools(...)` convenience and
  `WorkspaceLease` lifecycle wiring.
- [`crates/ork-agents/src/code_agent.rs`](../../crates/ork-agents/src/code_agent.rs)
  — `ToolContext::workspace()` accessor (depends on
  `WorkspaceLease` from `ork-app`).
- [`config/default.toml`](../../config/default.toml) —
  `[mcp.servers.codebase-memory]` entry plus `[shell]` and
  `[workspace]` sections.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Claude Code | `Read` (line-numbered, paginated), `Edit` (`old_string` exact-match + read-first invariant), `Write` (read-first for existing), `Bash` (timeout, truncation, sandbox), `Grep`, `Glob` | `read_file`, `edit_file`, `write_file`, `run_command`, `codebase_search` (Lexical) |
| Cursor | Per-run sandbox root + symlink-escape rejection in the editor | `LocalWorkspaceEditor` sandbox rules |
| Aider | Per-file SHA tracking; "file changed since I last saw it" surface; unified-diff edits | `WorkspaceEditError::Conflict { captured_diff }`, `apply_patch` |
| OpenHands / Devin | Worktree-per-task; sandboxed `BashSession` | `LocalWorkspaceProvisioner` worktrees + `LocalShellExecutor` |
| GitHub Copilot Workspace | Branch-per-task with structured diff surface | Per-run `task_branch` from `OpenWorkspaceRequest` |
| Mastra | `createTool` typed I/O + central registry | `tool().input::<I>().output::<O>()` via [`0051`](0051-code-first-tool-dsl.md) and `OrkAppBuilder::coding_tools` |
| ADR 0028 | `ShellExecutor` port shape and sandbox guarantees | `ShellExecutor` here, with deny-by-default network and integrated read-set lifecycle |
| ADR 0029 | `WorkspaceHandle`, `WorkspaceEditor`, optimistic hashing, sandbox rules | `WorkspaceHandle` + `WorkspaceEditor`, plus the read-set invariant on top |
| ADR 0030 | Worktree-per-run provisioning | `LocalWorkspaceProvisioner::open` |
| `codebase-memory-mcp` | SAM-compatible MCP server: `search_code`, `search_graph`, `query_graph`, `get_architecture` | Routed through `McpCodebaseIndex`; default dev wiring in `config/default.toml` |

## Open questions

- **Network-namespace fallback on macOS dev hosts.**
  `unshare(CLONE_NEWNET)` is Linux-only. macOS sandboxing for
  outbound network is non-trivial without `pf` rule installation.
  v1 falls back to host network on macOS with a warning event;
  the right answer is probably "macOS dev is best-effort, prod is
  Linux," but it deserves a follow-up if a tenant runs ork on
  macOS in production.
- **Read-set per-run cap.** 1024 paths feels right for typical
  agent runs; some refactor agents will exceed it. The LRU
  eviction model means an evicted-then-re-touched path requires
  a re-read. A future ADR may persist the read-set to memory
  ([`0053`](0053-memory-working-and-semantic.md)) so the cap can
  scale to the run length.
- **Cross-run read-set sharing.** Two parallel sub-agents inside
  the same workflow each see their own read-set today. If they
  share a workspace they could trip each other's
  `InvariantViolation` errors. Resolved by giving each sub-agent
  its own `WorkspaceLease` (own worktree); confirmed during
  workflow-engine integration.
- **`run_tests` as a separate tool vs. an `argv` template.**
  Bundling `cargo test` / `pytest` / `jest` parsers (ADR 0028's
  `TestResultParser`) gives the LLM structured failure-location
  output. Out of scope for v1 — `run_command` plus a parser tool
  in a follow-up ADR is the cleaner split, especially given how
  much surface area test parsers carry.
- **`apply_patch` fuzz factor.** Default 0 (exact). `git`-style
  fuzz of 1–3 lets minor whitespace drift apply, at the cost of
  occasionally applying to the wrong location. Open question on
  whether the default should be 1; v1 is conservative.
- **MCP-server health on startup.** If the
  `codebase-memory-mcp` stdio process is missing or fails to
  launch, the `McpCodebaseIndex` resolution should degrade
  silently (`gate(false)` on `codebase_search` with semantic
  mode) rather than fail `OrkApp::build()`. Confirmed during
  implementation; called out here so it doesn't surprise.
- **Indexing budget.** `PgVectorCodebaseIndex` indexes on first
  open. For a 100k-file repo the indexing cost is non-trivial.
  Defer: per-tenant config knob to disable auto-indexing and
  require an explicit `ork index <repo>` CLI step. Tracks with
  [`0057`](0057-ork-cli-dev-build-start.md).
- **Per-tenant `OrkApp` interaction
  ([`0058`](0058-per-tenant-orkapp.md)).** Per-tenant overlays
  may want to override the codebase-search backend choice (a
  tenant on a fork without pgvector should not get the global
  default). The resolution rule belongs in 0058's overlay
  semantics; this ADR's resolution is the single-tenant default.

## References

- ADR [`0010`](0010-mcp-tool-plane.md) — MCP plane, `mcp:<server>.<tool>`
  namespace, three-source server registration.
- ADR [`0016`](0016-artifact-storage.md) — artifact spill for
  oversized stdout/stderr.
- ADR [`0020`](0020-tenant-security-and-trust.md) — tenant trust
  frame; `shell_env_allowlist`, `shell_network_allow*`.
- ADR [`0021`](0021-rbac-scopes.md) — scope vocabulary (`tool:*:invoke`,
  `shell:cmd:*:invoke`, `shell:network:allow:*`).
- ADR [`0028`](0028-shell-executor-and-test-runners.md) — shell
  executor design reference (Superseded by 0048; restored here).
- ADR [`0029`](0029-workspace-file-editor.md) — workspace editor
  design reference (Superseded by 0048; restored here).
- ADR [`0030`](0030-git-operations.md) — git operations design
  reference (Superseded by 0048; partially restored here for
  worktree provisioning; full git tool family is a follow-up).
- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot
  that consolidated 0028/0029/0030.
- ADR [`0049`](0049-orkapp-central-registry.md) — registry the
  new tools attach to.
- ADR [`0051`](0051-code-first-tool-dsl.md) — `tool()` builder.
- ADR [`0052`](0052-code-first-agent-dsl.md) — `CodeAgent` and
  `ToolContext::workspace()`.
- ADR [`0053`](0053-memory-working-and-semantic.md) — pgvector
  setup that `PgVectorCodebaseIndex` builds on.
- BLAKE3: <https://github.com/BLAKE3-team/BLAKE3-specs>
- `diffy` crate: <https://crates.io/crates/diffy>
- `codebase-memory-mcp`: SAM-compatible knowledge-graph MCP
  server (search_code, search_graph, query_graph, get_architecture).
