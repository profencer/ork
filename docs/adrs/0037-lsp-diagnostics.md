# 0037 — LSP Diagnostics as a Feedback Source

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0010, 0020, 0028, 0029, 0033, 0034, 0038, 0041, 0045
- **Supersedes:** —

## Context

Coding agents in ork close their feedback loops today by parsing the
output of build and test commands run through ADR
[`0028`](0028-shell-executor-and-test-runners.md)'s `ShellExecutor` —
`cargo build`, `cargo test`, `pytest`, `tsc`. That feedback is correct
but **slow and coarse**:

- A `cargo build` on a touched workspace member takes 5–60 seconds
  cold, 2–10 seconds warm; `tsc --noEmit` and a full `pytest` collect
  pass take comparable wall time.
- The output is a hundreds-of-lines text dump where the load-bearing
  signal is one or two diagnostics buried in it. ADR
  [`0028`](0028-shell-executor-and-test-runners.md)'s
  `tail_keep_bytes` (default 8 KiB) and `TestResultParser` mitigate
  this for tests but do nothing for `cargo check`-shaped output.
- Weak local models (per ADR
  [`0034`](0034-per-model-capability-profiles.md)) thrash much harder
  on a 200-line `cargo` dump than on a one-line "type mismatch at
  `engine.rs:47`: expected `&str`, found `String`" — every additional
  token of irrelevant context erodes their tool-selection accuracy.

A second feedback channel is sitting in plain sight: **language
servers**. Modern coding harnesses (Cline, opencode, Cursor's agent
mode) plumb LSP diagnostics back into the prompt because they are:

- **Structured.** Each diagnostic carries `(severity, range, code,
  message, source)` — already in the shape ADR
  [`0034`](0034-per-model-capability-profiles.md)'s weak-model
  profiles want to consume.
- **Fast.** rust-analyzer publishes diagnostics within
  hundreds of milliseconds of a `didChange`; pyright and
  typescript-language-server respond on the same order. Compared to
  `cargo build`, this is two orders of magnitude faster.
- **Semantic.** They distinguish "unresolved import" from "type
  mismatch" from "deprecated symbol used" by `code` and `source`
  fields, where a build dump conflates them in prose.

The verifier persona introduced by ADR
[`0033`](0033-coding-agent-personas.md) (the `plan_verifier`) is the
second concrete consumer waiting on this. ADR [`0038`] (planned)
gates a planner's plan through one or more `plan_verifier` peers
before the executor is allowed to touch source. A verifier that knows
the file the plan is about to edit *already* has unresolved errors at
line `Y` can flag "fix prerequisites first" without speculating —
turning a class of `plan_verifier` rejections from heuristic to
mechanical.

A third constraint comes from ADR [`0041`] (planned), which
introduces nested worktrees so multiple sub-agents can edit
concurrently. A naive "one LSP per agent" lifecycle would spawn
duplicate language servers per worktree; an
"LSP per workspace root" lifecycle reuses a single server across the
sub-agents that share a tree. We bake the right shape in here so
ADR [`0041`] does not have to retrofit it.

The closest existing surface is ADR
[`0028`](0028-shell-executor-and-test-runners.md)'s `run_tests` —
which proves the ergonomic wrapper pattern (structured summaries
beside truncated raw output) — and ADR
[`0029`](0029-workspace-file-editor.md)'s `WorkspaceEditor` (which
owns the writes the diagnostics observe). Neither speaks LSP; this
ADR adds the missing port.

## Decision

ork **introduces** an `LspClient` port in `ork-core` plus a
`LocalLspClient` implementation in `ork-integrations`, registers a
native `get_diagnostics` tool through the existing
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs),
and ships out-of-the-box adapters for **rust-analyzer**, **pyright**,
and **typescript-language-server**. Language-server processes are
keyed by `(tenant_id, workspace_root)` and shared across every agent
running in that workspace — including the nested sub-agents ADR
[`0041`] will introduce — with an idle-shutdown timer and a per-tenant
concurrency cap.

Diagnostics are an **internal** tool plane. ADR
[`0010`](0010-mcp-tool-plane.md) (MCP for external tools) does not
apply here for the same three reasons it didn't apply to ADR
[`0028`](0028-shell-executor-and-test-runners.md)'s shell executor:

1. **Workspace lifecycle.** Diagnostics are computed against a
   working tree owned by ADR
   [`0029`](0029-workspace-file-editor.md)'s `WorkspaceHandle`. A
   language server has to be told about every `didChange` *before* it
   can answer a `pullDiagnostics`; that requires the LSP client to be
   a peer of the editor in the same process, not a tool sitting on the
   far side of an `rmcp` boundary that does not see edits.
2. **Process-spawn-via-process.** Spawning rust-analyzer / pyright is
   the same chicken-and-egg ADR
   [`0028`](0028-shell-executor-and-test-runners.md) flagged for
   shell: putting the primitive that spawns long-lived language
   servers behind a long-lived MCP server gains nothing.
3. **No common MCP-level diagnostic schema.** Each language server
   speaks LSP `Diagnostic`; an MCP tool would have to invent (and
   maintain) a separate schema layer per server. Native lets us
   normalise once.

Out of scope for this ADR: completions, hover, code actions /
quick-fixes, formatting, refactor / rename, semantic tokens. Those
are LSP capabilities ork's coding agent does not need today and
adding them is a per-feature follow-up, not a port redesign.

### `LspClient` port

```rust
// crates/ork-core/src/ports/lsp.rs

use std::path::PathBuf;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::TenantId;

use crate::ports::workspace::WorkspaceHandle;

/// One server-reported diagnostic. The wire shape is a normalised
/// projection of LSP's `Diagnostic` — server-specific extensions are
/// dropped so weak-model prompts see one schema regardless of which
/// language server produced the message.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Workspace-relative path. Always forward-slash; canonicalised.
    pub path: String,
    /// Zero-based start line/column (LSP-native).
    pub start: Position,
    /// Zero-based end line/column. May equal `start` for point
    /// diagnostics.
    pub end: Position,
    pub severity: DiagnosticSeverity,
    /// Stable code (e.g. `"E0308"`, `"reportMissingImports"`,
    /// `"TS2322"`). Empty when the server omits one.
    pub code: String,
    /// Human-readable text. Single-line; multi-line server messages
    /// are joined with `" | "`.
    pub message: String,
    /// Producing language server id (e.g. `"rust-analyzer"`,
    /// `"pyright"`, `"typescript-language-server"`).
    pub source: String,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// Filter applied at pull time. `None` means "all open documents in
/// this workspace."
#[derive(Clone, Debug, Default)]
pub struct DiagnosticFilter {
    /// Workspace-relative path. When set, only diagnostics under this
    /// file (or directory prefix) are returned.
    pub path: Option<String>,
    /// Minimum severity (inclusive). Defaults to `Warning`.
    pub min_severity: Option<DiagnosticSeverity>,
    /// Maximum number of diagnostics returned (oldest-first). The
    /// caller's prompt budget is finite; the executor enforces a cap.
    pub limit: Option<usize>,
}

#[async_trait]
pub trait LspClient: Send + Sync {
    /// Ensure a language server is running for `(tenant, workspace)`,
    /// initialise it if needed, and start watching the tree. Idempotent.
    /// Languages to start are inferred from the workspace's content
    /// (presence of `Cargo.toml`, `pyproject.toml`/`setup.py`,
    /// `package.json` + `tsconfig.json`).
    async fn open_workspace(
        &self,
        tenant_id: TenantId,
        ws: &WorkspaceHandle,
    ) -> Result<(), OrkError>;

    /// Notify the relevant language server that `path` was rewritten
    /// to `contents`. The editor (ADR 0029) calls this after every
    /// successful write when the profile flag from `Auto-injection`
    /// below is set.
    async fn notify_did_change(
        &self,
        tenant_id: TenantId,
        ws: &WorkspaceHandle,
        path: &str,
        contents: &str,
    ) -> Result<(), OrkError>;

    /// Pull current diagnostics from every relevant language server.
    /// Returns once each contributing server has either responded or
    /// hit `pull_timeout`. Servers that time out contribute nothing
    /// for that pull; the result still comes back so the agent loop
    /// is not blocked.
    async fn pull_diagnostics(
        &self,
        tenant_id: TenantId,
        ws: &WorkspaceHandle,
        filter: DiagnosticFilter,
    ) -> Result<Vec<Diagnostic>, OrkError>;
}
```

`WorkspaceHandle` is the type ADR
[`0029`](0029-workspace-file-editor.md) already defined; this ADR
does not extend it.

### `LocalLspClient` (ork-integrations)

`crates/ork-integrations/src/lsp/mod.rs` adds `LocalLspClient`. Its
contract — enforced for every call — is:

1. **Workspace-keyed lifecycle.** Internal state is keyed by
   `(TenantId, WorkspaceHandle.id)`. `open_workspace` is the only
   spawn point: it auto-detects languages from the tree, spawns each
   relevant server with `tokio::process::Command`, performs the LSP
   `initialize` / `initialized` handshake, then opens
   `textDocument/didOpen` for every source file under the workspace
   root (cap: `max_open_files_per_server`, default 2 000). Subsequent
   calls return immediately if the entry is healthy.
2. **Sub-agent sharing.** Multiple `LocalAgent`s in the same workspace
   — including the nested sub-agents ADR [`0041`] introduces — share
   one `LspWorkspaceEntry` and therefore one set of language-server
   processes. The entry holds an `Arc`; agents acquire and release it,
   and the entry is reaped only when refcount drops *and* the
   idle-shutdown timer expires.
3. **Per-tenant cap.** A configured `max_workspaces_per_tenant`
   (default 8) bounds how many distinct workspaces may have live
   language servers. When the cap is reached, the least-recently-used
   workspace is gracefully shut down (LSP `shutdown` then `exit`,
   2 s SIGTERM ladder) before a new one is admitted.
4. **Idle shutdown.** Each entry tracks a last-touched timestamp.
   A reaper task running every `idle_check_interval` (default 30 s)
   shuts down entries whose last `notify_did_change` /
   `pull_diagnostics` is older than `idle_shutdown` (default 10 min).
5. **Pull semantics.** `pull_diagnostics` issues
   `textDocument/diagnostic` (LSP 3.17 pull model) when the server
   advertises support, otherwise falls back to consuming the
   server's `textDocument/publishDiagnostics` cache. Each server
   contributes within `pull_timeout` (default 3 s); slower servers
   simply omit their contribution from this pull and feed the next one.
6. **Sandbox parity with shell.** Server processes run with
   `Command::env_clear()` then `PATH`, `HOME`, `LANG`, and the
   tenant-allowlisted env keys ADR
   [`0028`](0028-shell-executor-and-test-runners.md) already
   introduced via `TenantSettings.shell_env_allowlist`. Working
   directory is the canonicalised `WorkspaceHandle.root`. No
   `SSH_AUTH_SOCK`, no `AWS_*`, no `GITHUB_TOKEN` leak in.
7. **Auditing.** Each server start emits a `tracing` event
   `lsp.spawn` with `tenant_id`, `workspace_id`, `server`, `argv`,
   `cwd`. Each `pull_diagnostics` call emits `lsp.pull` with
   `tenant_id`, `workspace_id`, `path`, `min_severity`, `count`,
   `duration_ms`. Both feed ADR [`0022`](0022-observability.md)'s
   audit stream.

Built-in adapters shipped in this ADR:

| Language | Server binary | Trigger files |
| -------- | ------------- | ------------- |
| Rust | `rust-analyzer` | `Cargo.toml` anywhere in the workspace |
| Python | `pyright` (Node-based) | `pyproject.toml`, `setup.py`, or `setup.cfg` |
| TypeScript / JavaScript | `typescript-language-server` | `tsconfig.json` or `package.json` containing a `typescript` dependency |

The set is pluggable: each adapter implements an internal
`LanguageServerAdapter` trait (binary name, env, document filter,
init options). Adding `gopls`, `clangd`, `solargraph`, etc. is a
per-adapter follow-up that does not touch the port.

### Native tool: `get_diagnostics`

`get_diagnostics` registers through
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs)
so it surfaces in ADR
[`0011`](0011-native-llm-tool-calling.md)'s `tool_descriptors_for_agent`.

```json
{
  "name": "get_diagnostics",
  "description": "Return current language-server diagnostics for the active workspace. Useful after edits to confirm an issue is gone or to scope new errors before running heavier checks like cargo build. Diagnostics are typed (error / warning / information / hint), positioned (line / column), and source-stamped.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "Workspace-relative file or directory. Omit to pull diagnostics for every open document."
      },
      "min_severity": {
        "type": "string",
        "enum": ["error", "warning", "information", "hint"],
        "default": "warning"
      },
      "limit": {
        "type": "integer",
        "minimum": 1,
        "maximum": 200,
        "default": 50
      }
    }
  }
}
```

Result wire shape (returned as the JSON tool result the LLM sees):

```json
{
  "diagnostics": [
    {
      "path": "crates/ork-core/src/workflow/engine.rs",
      "start": {"line": 46, "character": 8},
      "end":   {"line": 46, "character": 24},
      "severity": "error",
      "code": "E0308",
      "message": "mismatched types | expected `&str`, found `String`",
      "source": "rust-analyzer"
    }
  ],
  "truncated": false,
  "servers": [
    {"name": "rust-analyzer", "ready": true, "responded": true},
    {"name": "pyright",       "ready": true, "responded": false}
  ]
}
```

`truncated` is `true` when `limit` clipped the result. `servers`
reflects which adapters contributed to *this* pull, so the LLM can
tell "no errors" from "rust-analyzer is still indexing."

### Auto-injection after `WorkspaceEditor` writes

ADR [`0029`](0029-workspace-file-editor.md)'s `WorkspaceEditor`
gains a hook in its dispatcher: after every successful
`create_file` / `update_file` / `apply_patch`, when the **active
agent's profile** carries `auto_diagnostics_after_edit: true` (a new
flag on ADR [`0034`](0034-per-model-capability-profiles.md)'s
`ModelCapabilityProfile`), the dispatcher emits a synthetic tool
result `get_diagnostics(path = <edited path>)` into the agent's
message stream as if the LLM had called the tool itself.

The shape is exactly the shape `get_diagnostics` returns above; the
agent's loop processes it through the same path it would have for an
explicit call. Profiles for weak local models default the flag on;
profiles for frontier models default it off (frontier models are
disciplined enough to ask for diagnostics when they need them, and
the auto-injection wastes tokens). The flag is also overridable
per-persona (ADR [`0033`](0033-coding-agent-personas.md)) so a
`tester` persona can demand diagnostics regardless of model.

### Plan-verifier consumption

ADR [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
persona — the consumer ADR [`0038`] (planned) calls during plan
verification — gets `get_diagnostics` in its default
`tool_catalog`. The verifier's contract is `(plan, repo_context) →
verdict`; with diagnostics in hand it can mechanically reject plans
whose touched files already carry unresolved errors (`fix
prerequisites first`) instead of guessing from prose. ADR
[`0038`]'s plan-verification gate is the place that wires the call;
this ADR only guarantees the tool is available.

### Configuration

`config/default.toml` gains an `[lsp]` section:

```toml
[lsp]
enabled = true
max_workspaces_per_tenant = 8
max_open_files_per_server = 2000
pull_timeout_ms = 3000
idle_shutdown_seconds = 600
idle_check_interval_seconds = 30
default_min_severity = "warning"
default_limit = 50

[lsp.servers.rust-analyzer]
binary = "rust-analyzer"
[lsp.servers.pyright]
binary = "pyright-langserver"
args = ["--stdio"]
[lsp.servers.typescript-language-server]
binary = "typescript-language-server"
args = ["--stdio"]
```

Operators may disable the whole feature (`enabled = false`) or
override binary paths per server. Adapters that fail to spawn (binary
missing, init handshake errors) are flagged unhealthy in
`servers[]` rather than crashing the workspace; the feature degrades
to "no LSP feedback for that language" instead of taking the agent
down.

### RBAC scopes (reserved, not enforced)

ADR [`0021`](0021-rbac-scopes.md) owns enforcement. This ADR
reserves:

| Scope | Meaning |
| ----- | ------- |
| `tool:get_diagnostics:invoke` | Agent may call `get_diagnostics` at all |
| `lsp:server:<name>:spawn` | Per-server allow-list (`lsp:server:rust-analyzer:spawn`, etc.); reserved for tenants that want to disable Node-based servers, etc. |
| `lsp:server:*:spawn` | Wildcard — any configured server permitted |

Enforcement lands when ADR
[`0021`](0021-rbac-scopes.md)'s `ScopeChecker` is threaded through
`ToolExecutor::execute`. The scope grammar matches that ADR's
`<resource>:<id>:<action>` shape.

## Acceptance criteria

- [ ] Trait `LspClient` defined at
      [`crates/ork-core/src/ports/lsp.rs`](../../crates/ork-core/src/ports/lsp.rs)
      with the signature shown in `Decision`.
- [ ] Types `Diagnostic`, `Position`, `DiagnosticSeverity`,
      `DiagnosticFilter` defined in the same module and re-exported
      from [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs).
- [ ] `LocalLspClient` defined at
      `crates/ork-integrations/src/lsp/mod.rs`, constructed from a
      `LocalLspConfig { max_workspaces_per_tenant,
      max_open_files_per_server, pull_timeout, idle_shutdown,
      idle_check_interval, server_overrides }`.
- [ ] `LocalLspClient::open_workspace` spawns rust-analyzer when the
      workspace contains a `Cargo.toml`, pyright when it contains
      `pyproject.toml` / `setup.py` / `setup.cfg`, and
      typescript-language-server when it contains `tsconfig.json` or a
      `package.json` with a `typescript` dep, verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::detects_languages`.
- [ ] `LocalLspClient::open_workspace` is idempotent: a second call
      with the same `WorkspaceHandle.id` does not spawn additional
      processes, verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::open_workspace_idempotent`.
- [ ] `LocalLspClient` shares one server per `(tenant_id,
      workspace_id)` across multiple `Arc<dyn LspClient>` callers,
      verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::shared_across_callers`
      (acquires the client from two tasks, asserts a single PID).
- [ ] `LocalLspClient` enforces `max_workspaces_per_tenant` by LRU
      eviction (graceful `shutdown` + `exit`), verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::lru_evicts_oldest`.
- [ ] `LocalLspClient::pull_diagnostics` returns within
      `pull_timeout` even when one adapter is unresponsive, with the
      unresponsive adapter marked `responded: false` in the
      `servers[]` block, verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::pull_times_out_per_server`.
- [ ] `LocalLspClient::pull_diagnostics` filters by `path`,
      `min_severity`, and `limit` per `DiagnosticFilter`, verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::filters_apply`.
- [ ] `LocalLspClient::notify_did_change` produces an updated
      diagnostic on the next `pull_diagnostics` for a deliberately
      broken Rust source file, verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::roundtrip_rust_diagnostic`.
- [ ] Idle reaper shuts down language servers after
      `idle_shutdown` elapses with no calls, verified by
      `crates/ork-integrations/tests/lsp_smoke.rs::idle_shutdown` with
      a low test-only `idle_shutdown` value.
- [ ] `CodeToolExecutor::is_code_tool` returns `true` for
      `"get_diagnostics"`.
- [ ] `CodeToolExecutor::descriptors` returns the
      `get_diagnostics` descriptor with the JSON-Schema shown under
      `Native tool: get_diagnostics`.
- [ ] `CodeToolExecutor::execute` routes `get_diagnostics` to
      `LspClient::pull_diagnostics` and emits the wire shape shown
      under `Native tool: get_diagnostics`.
- [ ] `WorkspaceEditor` dispatcher invokes
      `LspClient::notify_did_change` after every successful
      `create_file` / `update_file` / `apply_patch`, verified by
      `crates/ork-integrations/tests/workspace_editor_lsp.rs::notifies_on_write`.
- [ ] When the active agent's `ModelCapabilityProfile` carries
      `auto_diagnostics_after_edit: true`, a synthetic
      `get_diagnostics` tool result is appended to the agent's
      message stream after each successful write, verified by
      `crates/ork-agents/tests/local_lsp_autoinject.rs::auto_inject_when_enabled`.
- [ ] When the flag is `false`, no synthetic tool result is appended,
      verified by
      `crates/ork-agents/tests/local_lsp_autoinject.rs::no_auto_inject_when_disabled`.
- [ ] `ModelCapabilityProfile.auto_diagnostics_after_edit: bool` field
      added in
      [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
      (or wherever ADR
      [`0034`](0034-per-model-capability-profiles.md) lands the
      profile struct) with serde round-trip coverage and a default of
      `false` for frontier-tier profiles, `true` for weak-tier ones.
- [ ] `plan_verifier` persona's default `tool_catalog` in
      `crates/ork-agents/src/persona.rs` includes
      `get_diagnostics`.
- [ ] `cargo test -p ork-integrations lsp::` is green.
- [ ] `cargo test -p ork-agents local_lsp_autoinject::` is green.
- [ ] `[lsp]` section added to
      [`config/default.toml`](../../config/default.toml) with the
      keys shown under `Configuration`.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row added for
      `0037`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Coding agents — especially those running on weak local models — get
  a **fast, structured** signal between every edit, dropping the
  thrash that comes from parsing 200-line `cargo` dumps. Empirically
  this is the largest single quality lever in published agent-harness
  benchmarks (Cline, opencode, OpenHands).
- ADR [`0033`](0033-coding-agent-personas.md)'s `plan_verifier`
  persona, and through it ADR [`0038`]'s plan-verification gate, gets
  a mechanical "are prerequisites already broken?" check instead of a
  prose heuristic.
- Workspace-keyed lifecycle pre-empts the "duplicate LSP per
  sub-agent" footgun ADR [`0041`] would otherwise have to retrofit.
- LSP servers run *concurrently* with the agent loop: while the LLM
  is generating its next message, rust-analyzer is already indexing
  the latest write. The next `get_diagnostics` is essentially free.

### Negative / costs

- **Memory footprint.** rust-analyzer alone is 200–600 MiB resident
  per workspace; pyright is 100–300 MiB. With
  `max_workspaces_per_tenant = 8`, a tenant can hold ~3 GiB of LSP
  RAM steady-state per language. We accept this in exchange for the
  responsiveness; operators tune the cap downward when needed and
  idle-shutdown reclaims aggressively.
- **Cold-start latency.** rust-analyzer needs 5–60 s to index a
  fresh `Cargo.toml` workspace. The first `get_diagnostics` after
  `open_workspace` may return an incomplete picture — ADR [`0028`]'s
  `cargo build` remains the source of truth for "is this *really*
  fixed."
- **Adapter maintenance.** LSP is a stable spec but server-specific
  init options (rust-analyzer's `cargo.allTargets`, pyright's
  `python.analysis.diagnosticSeverityOverrides`) shift between
  versions. Each adapter pins the server major version and surfaces
  spawn failures as `servers[].ready: false` so a broken adapter
  degrades gracefully.
- **Operator dependency.** rust-analyzer / pyright /
  typescript-language-server must be installed on the ork host (or
  inside the container image). We document the binaries and their
  default invocations; missing binaries surface as `ready: false`.
- **Trust surface.** Each language server is a long-lived process
  with file-system access scoped to the workspace root. We rely on
  the same env-clear and cwd-canonicalisation guarantees ADR
  [`0028`](0028-shell-executor-and-test-runners.md) commits to;
  introducing a third long-lived process class slightly enlarges the
  audit surface.
- **Synthetic tool results in the message stream.** Auto-injecting a
  `get_diagnostics` result the LLM did not request is a deviation
  from ADR [`0011`](0011-native-llm-tool-calling.md)'s "the LLM
  decides when to call tools" stance. Profiles with the flag off
  preserve the original behaviour; profiles with the flag on are
  explicit about it.

### Neutral / follow-ups

- Adding more language servers (gopls, clangd, solargraph,
  hls, lua-language-server) is a per-adapter follow-up.
- A separate ADR may add LSP **code actions** (auto-fixes for
  `unused_imports`, `add_missing_use`, etc.) so the agent can apply
  cheap server-suggested fixes without a round-trip through the LLM.
- A separate ADR may add LSP **completions** as a hint source for
  unknown-symbol-style failures, behind a profile flag.
- A separate ADR may add a **containerised LSP executor** so a
  malicious server in a third-party adapter cannot read host files
  outside the workspace.
- ADR [`0022`](0022-observability.md) consumes the `lsp.spawn` /
  `lsp.pull` events.
- ADR [`0028`](0028-shell-executor-and-test-runners.md) and this ADR
  layer cleanly: agents are expected to use diagnostics for the inner
  loop (per-edit feedback) and `run_command` / `run_tests` for the
  outer loop (full-build / full-test confirmation).

## Alternatives considered

- **Stick with parsing build/test output (no LSP).** Rejected:
  loses the order-of-magnitude latency win and forces every weak-model
  prompt to consume a build dump. Build/test parsing remains in place
  as the *outer* loop; LSP is the *inner* loop.
- **Use an MCP language-server gateway (e.g. an `mcp:lsp` community
  server).** Rejected for the three reasons in `Decision`: workspace
  lifecycle has to live in-process with the editor, process-spawn-via-
  process is wasteful, and there is no common diagnostic schema across
  servers — normalisation has to happen somewhere, and native is the
  cheapest place.
- **One language server per agent (no workspace sharing).**
  Rejected: ADR [`0041`]'s nested-worktree design would
  spawn O(agents × languages) servers per workspace. Workspace
  sharing keeps the ratio at O(workspaces × languages).
- **Run LSP servers in containers.** Rejected for the v1 cut: matches
  the ADR
  [`0028`](0028-shell-executor-and-test-runners.md) decision to ship
  process-level isolation first and layer containerisation as a
  follow-up. The same containerisation work would benefit both
  shell and LSP.
- **Per-edit diagnostic streaming via `publishDiagnostics` only
  (no pull).** Rejected: the pull model (LSP 3.17) gives the agent
  a deterministic "as of now, here are the diagnostics" answer; the
  publish model alone forces the client to guess when the server is
  done indexing. The implementation accepts published diagnostics
  into a cache so servers without pull support degrade gracefully,
  but pull is the primary surface.
- **Synthesise diagnostics from `cargo check --message-format=json`
  / `tsc --noEmit` parse.** Rejected: this is a hand-rolled LSP
  re-implementation per language, and it loses the incremental
  reindex that gives LSP its latency advantage. `run_command` already
  exposes `cargo check` for agents that want it; this is a different,
  faster channel.
- **Auto-inject diagnostics for *every* model.** Rejected: frontier
  models tolerate the prompt cost but their tool-selection accuracy
  does not improve, so the spend is wasted. The profile flag puts the
  decision where it belongs.
- **Single `LocalLspClient::diagnose(handle, path)` synchronous
  method, no `open_workspace` / `notify_did_change`.** Rejected:
  pretending the LSP lifecycle does not exist hides the cold-start
  latency from callers and makes the per-write notify path
  impossible. The three-method shape mirrors LSP itself, which is
  the right abstraction.

## Affected ork modules

- New: [`crates/ork-core/src/ports/lsp.rs`](../../crates/ork-core/src/ports/lsp.rs)
  — `LspClient` port and its types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `lsp`.
- New: `crates/ork-integrations/src/lsp/mod.rs` — `LocalLspClient`,
  per-workspace entry, idle reaper, LRU eviction.
- New: `crates/ork-integrations/src/lsp/adapters/{rust_analyzer.rs,
  pyright.rs, typescript.rs}` — per-language adapters and the
  `LanguageServerAdapter` trait.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — register the `get_diagnostics` tool, hold the
  `Arc<dyn LspClient>` field.
- [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs)
  — public re-exports for `LocalLspClient` and the adapter registry.
- `crates/ork-integrations/src/workspace_editor.rs` (the home ADR
  [`0029`](0029-workspace-file-editor.md) lands) — invoke
  `LspClient::notify_did_change` after every successful write; emit
  the synthetic `get_diagnostics` tool result when the active
  profile's flag is set.
- [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
  — extend `ModelCapabilityProfile` (per ADR
  [`0034`](0034-per-model-capability-profiles.md)) with
  `auto_diagnostics_after_edit: bool`.
- `crates/ork-agents/src/persona.rs` (the home ADR
  [`0033`](0033-coding-agent-personas.md) lands) —
  `plan_verifier` persona default catalog includes
  `get_diagnostics`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
  — boot `LocalLspClient` from `[lsp]` config, wire it into the
  `CodeToolExecutor` builder and the `WorkspaceEditor` dispatcher.
- [`config/default.toml`](../../config/default.toml) — `[lsp]`
  section per `Configuration`.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Cline | `src/services/diagnostics/` — VS Code `languages.getDiagnostics` piped into the prompt after every edit | `WorkspaceEditor` auto-injection of `get_diagnostics` after writes |
| opencode | `packages/opencode/src/lsp/` — per-workspace LSP clients, normalised diagnostic shape | `LocalLspClient` + normalised `Diagnostic` |
| Cursor (agent mode) | LSP-fed quick-fix loop, undocumented but observable in trace logs | reserved for a follow-up (code actions) |
| Aider | parses `flake8` / `pylint` / `tsc` output as a diagnostics surrogate | this ADR's primary alternative — rejected for latency reasons |
| Solace Agent Mesh | none — SAM has no first-class language-server feedback channel | `LspClient` port + `get_diagnostics` tool |
| LSP 3.17 | pull-model `textDocument/diagnostic` | `LspClient::pull_diagnostics` |

## Open questions

- **Multi-root workspaces.** Cargo workspaces with member `Cargo.toml`
  files are handled by rust-analyzer natively, but a tenant repo that
  contains *both* a Rust crate and a Python package as siblings will
  spawn two servers rooted at the same `WorkspaceHandle.root`. This
  is the intended behaviour today; if memory pressure forces the
  question, a follow-up may add per-language sub-roots so each server
  scopes itself tighter.
- **Diagnostic pull timing.** `notify_did_change` is fire-and-forget;
  `pull_diagnostics` returns whatever the server has computed *so
  far*. Empirically rust-analyzer publishes within 100–400 ms of a
  notify, but a deliberately busy index can lag. We accept that the
  first pull immediately after a notify may return stale data; the
  next pull catches up. ADR
  [`0028`](0028-shell-executor-and-test-runners.md)'s `cargo build`
  remains authoritative for "really fixed."
- **Containerisation.** Should LSP servers run in a sandbox tighter
  than process-level (firejail, bubblewrap, Docker)? Decision: not
  for v1; same answer as ADR
  [`0028`](0028-shell-executor-and-test-runners.md). A follow-up ADR
  can layer a containerised executor under the same port.
- **Windows.** Acceptance criteria target Linux/macOS hosts;
  Windows support is best-effort. CI runs the smoke tests only on
  Linux for v1.
- **Network egress for servers.** rust-analyzer occasionally fetches
  rustup-managed components on first run; pyright pulls type stubs.
  We rely on the host having pre-fetched these; an air-gapped tenant
  may need a follow-up to ship vendored stubs.
- **Per-binary RBAC granularity.** Today the reserved scopes are
  `lsp:server:<name>:spawn`. If we discover tenants want to allow
  `rust-analyzer` but disable Node-based servers wholesale, ADR
  [`0021`](0021-rbac-scopes.md) can extend the grammar without a
  superseding ADR here.

## References

- ADR [`0010`](0010-mcp-tool-plane.md) — the "internal tools stay
  native" rule this ADR composes with (and the reason `get_diagnostics`
  is not an MCP tool).
- ADR [`0020`](0020-tenant-security-and-trust.md) — tenant trust
  frame the `lsp.spawn` events feed into.
- ADR [`0028`](0028-shell-executor-and-test-runners.md) — outer-loop
  feedback (`run_command` / `run_tests`) that this ADR's inner-loop
  feedback complements.
- ADR [`0029`](0029-workspace-file-editor.md) — `WorkspaceHandle` and
  `WorkspaceEditor` whose writes trigger `notify_did_change`.
- ADR [`0033`](0033-coding-agent-personas.md) — `plan_verifier`
  persona that consumes `get_diagnostics` during plan review.
- ADR [`0034`](0034-per-model-capability-profiles.md) —
  `ModelCapabilityProfile` extended with
  `auto_diagnostics_after_edit`.
- ADR [`0038`] (planned) — plan cross-verification protocol that
  layers on `plan_verifier` + diagnostics.
- ADR [`0041`] (planned) — nested worktrees that depend on the
  workspace-keyed LSP lifecycle decided here.
- ADR [`0045`] (planned) — multi-agent team orchestrator that
  composes diagnostics-aware personas.
- LSP 3.17 specification:
  <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/>.
- rust-analyzer manual: <https://rust-analyzer.github.io/manual.html>.
- pyright: <https://github.com/microsoft/pyright>.
- typescript-language-server:
  <https://github.com/typescript-language-server/typescript-language-server>.
