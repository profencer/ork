# 0028 — Shell Executor & Test Runner integration

- **Status:** Superseded by 0048
- **Date:** 2026-04-28
- **Deciders:** ork core team
- **Phase:** 3
- **Relates to:** 0002, 0010, 0011, 0016, 0020, 0021

## Context

ork has no way to run a shell command from inside an agent loop. The
only "code-touching" tools today live in
[`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
(`list_repos`, `code_search`, `read_file`, `list_tree`) and they are
strictly **read-only**: they walk shallow clones produced by
[`GitRepoWorkspace`](../../crates/ork-integrations/src/workspace.rs)
and never spawn a child process beyond `git` and `rg`. The
`Command::new(...)` calls inside `workspace.rs` are private; there is
no `ToolExecutor` arm an LLM can hit to run `cargo build`,
`cargo test`, `pytest`, `jest`, `eslint`, `tsc`, `terraform plan`, or
even `git diff`.

This blocks the autonomous-coding-agent direction the workflow
templates ([`workflow-templates/`](../../workflow-templates/)) and the
LangGraph demo ([`demo/langgraph-agent/`](../../demo/langgraph-agent/))
have been pointing toward:

- A "fix this failing test" agent must be able to *run* the test, read
  the failure, edit the file, and re-run. ADR
  [`0011`](0011-native-llm-tool-calling.md)'s tool-loop gives us the
  control flow but not the verbs.
- A "lint and format" agent must be able to invoke `cargo fmt`,
  `cargo clippy`, `ruff`, etc. and feed structured findings back into
  the prompt.
- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  verifier-agent is much weaker without a way to actually execute the
  thing it is verifying.

We deliberately did not paper over this with a single
`mcp:shell.run` tool from a community MCP server because:

1. Shell is the **trust boundary** for tenant workspaces. The
   sandboxing rule that today lives implicitly in
   [`GitRepoWorkspace::repo_path`](../../crates/ork-integrations/src/workspace.rs)
   (every tenant's cache lives under
   `<cache_dir>/<tenant_id>/<repo>`) must apply to every spawned
   process; we want that policy enforced in Rust, in-process, by the
   same code path that owns the clone. An MCP shell server would put
   the policy on the far side of an `rmcp` boundary it cannot reach.
2. ADR [`0010`](0010-mcp-tool-plane.md) is explicit that *internal
   tools stay native Rust under `ToolExecutor`*. Shell is the most
   internal tool there is — every other tool composes on top of it.
3. Spawning an MCP server is itself a process spawn. Putting the
   primitive that spawns processes behind a process is a chicken-and-egg.

## Decision

ork **introduces a `ShellExecutor` port** in `ork-core` plus a
`LocalShellExecutor` implementation in `ork-integrations`, and exposes
two native tools — `run_command` and `run_tests` — through the
existing [`CompositeToolExecutor`](../../crates/ork-integrations/src/tools.rs)
routing so [`LocalAgent`](../../crates/ork-agents/src/local.rs)'s
ADR-0011 tool loop can call them.

### Placement

The port lives in `ork-core` (alongside the existing
[`RepoWorkspace`](../../crates/ork-core/src/ports/workspace.rs) port)
and the implementation lives in `ork-integrations` (alongside
[`workspace.rs`](../../crates/ork-integrations/src/workspace.rs) and
[`code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)).
We do **not** create a new `ork-shell` crate because:

- The implementation is a few hundred lines of `tokio::process` + an
  output-truncation helper + the test parsers; spinning up a crate
  for that violates the "no premature abstractions" rule we have been
  applying since ADR [`0013`](0013-generic-gateway-abstraction.md).
- The test-runner parsers are the only thing that might want to live
  separately, and they are pure functions on `&str` → struct — easier
  to grow as a sub-module than a crate.
- The natural neighbours (`code_tools.rs`, `workspace.rs`) already
  live in `ork-integrations`. Tenant-cache layout and shallow-clone
  lifetime are already this crate's job; shell is the same job.

If the parser set grows beyond half a dozen frameworks or the
sandboxing strategy diverges meaningfully from `RepoWorkspace`, a
follow-up ADR can split a `crates/ork-shell` out cleanly.

### `ShellExecutor` port

```rust
// crates/ork-core/src/ports/shell.rs

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::TenantId;

/// One shell invocation request, fully resolved by the caller.
#[derive(Debug, Clone)]
pub struct ShellRequest {
    /// Logical workspace name (must resolve to a tenant-scoped path
    /// via the same `RepositorySpec` set the `RepoWorkspace` port uses).
    pub workspace: String,
    /// Working directory **relative to** the workspace root. `""` for
    /// the workspace root itself. Absolute paths and `..` segments are
    /// rejected.
    pub cwd: String,
    /// Argv. `argv[0]` is the program; never goes through `/bin/sh`.
    pub argv: Vec<String>,
    /// Environment variables added on top of an empty base env. The
    /// executor injects `PATH`, `HOME`, `LANG`, and tenant-allowlisted
    /// keys — nothing else from the host environment leaks in.
    pub env: Vec<(String, String)>,
    /// Per-call wall-clock timeout. Required.
    pub timeout: Duration,
    /// Hard cap on captured stdout+stderr bytes; the rest spills to
    /// an ADR-0016 artifact. `None` uses `default_max_output_bytes`.
    pub max_output_bytes: Option<usize>,
    /// Optional stdin payload. `None` closes stdin immediately.
    pub stdin: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct ShellResult {
    pub exit_code: i32,
    /// Process termination shape: normal exit, signal, or our own
    /// timeout kill. The wire form is documented under
    /// `Tool wire format` below.
    pub termination: ShellTermination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Set when the captured stream exceeded `max_output_bytes`; the
    /// full stream lives at this `ArtifactRef` (ADR 0016).
    pub stdout_artifact: Option<ArtifactRef>,
    pub stderr_artifact: Option<ArtifactRef>,
    pub duration: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellTermination {
    /// Process exited on its own with `exit_code`.
    Exited,
    /// Killed by signal `n` (Unix). On Windows we report `Exited` with
    /// the OS-supplied exit code.
    Signaled(i32),
    /// We killed it because `timeout` elapsed. `exit_code` is `-1`.
    TimedOut,
    /// We killed it because `AgentContext::cancel` fired. `exit_code` is `-1`.
    Cancelled,
}

#[async_trait]
pub trait ShellExecutor: Send + Sync {
    /// Spawn the request inside the tenant's sandbox and return its
    /// captured result. Errors only fire for **operator-class**
    /// failures (sandbox violation, fork failure, lock contention) —
    /// a non-zero `exit_code` is *not* an error so the LLM can read it.
    async fn execute(
        &self,
        tenant_id: TenantId,
        req: ShellRequest,
    ) -> Result<ShellResult, OrkError>;
}
```

`ArtifactRef` is the existing handle from
[`crates/ork-core/src/ports/artifact_store.rs`](../../crates/ork-core/src/ports/artifact_store.rs).

### Sandbox guarantees (`LocalShellExecutor`)

`crates/ork-integrations/src/shell.rs` adds `LocalShellExecutor`. Its
contract — enforced for every call — is:

1. **Workspace-rooted cwd.** The executor resolves
   `<cache_dir>/<tenant_id>/<workspace>` (the same path
   `GitRepoWorkspace::repo_path` produces), canonicalises it, and
   joins `req.cwd`. The final path is `canonicalize`d and rejected
   unless it stays under the resolved workspace root. Symlink escapes
   fail closed.
2. **Empty base env.** `Command::env_clear()` is called first; only
   `PATH`, `HOME`, `LANG`, the tenant-allowlisted keys from
   `TenantSettings.shell_env_allowlist`, and `req.env` are added. No
   `SSH_AUTH_SOCK`, no `AWS_*`, no `GITHUB_TOKEN`. Tenant credentials
   come in only via the explicit `req.env` channel and are scoped per
   call.
3. **No shell interpreter.** The executor calls
   `Command::new(&argv[0]).args(&argv[1..])` — never
   `sh -c "<string>"`. Callers wanting shell features must invoke the
   shell explicitly (`["bash", "-c", "..."]`); this puts the shell
   metacharacter trust on the caller.
4. **Per-call timeout.** Required field, no `Option`. Enforced via
   `tokio::time::timeout` around the wait; on expiry the executor
   sends `SIGTERM`, waits 2 s, then `SIGKILL`. `ShellTermination::TimedOut`.
5. **Cancellation.** `AgentContext.cancel` is wired through: a
   `tokio::select!` between the wait and the cancel token kills the
   process with the same SIGTERM/SIGKILL ladder.
   `ShellTermination::Cancelled`.
6. **Output capture.** stdout and stderr are read concurrently through
   `tokio::io::copy_buf` into bounded `Vec<u8>` buffers; on overflow
   the buffer is flushed to an artifact via
   `ArtifactStore::put` (ADR [`0016`](0016-artifact-storage.md)) and
   the captured slice is replaced by a tail of `tail_keep_bytes`
   (default 8 KiB). The truncation policy mirrors
   ADR [`0011`](0011-native-llm-tool-calling.md)'s
   `max_tool_result_bytes`.
7. **Concurrency.** A per-`(tenant_id, workspace)` semaphore bounds the
   number of in-flight shell calls (default 4). This keeps a single
   tenant from saturating the host while letting independent tenants
   run in parallel.
8. **Auditing.** Every spawn emits a `tracing` event
   `shell.spawn` with `tenant_id`, `tid_chain`, `workspace`, redacted
   `argv` (env values masked), `cwd`, `timeout_ms`. Every termination
   emits `shell.exit` with `exit_code`, `termination`, `duration_ms`,
   captured stdout/stderr byte counts. Both feed the audit stream
   ADR [`0022`](0022-observability.md) defines.

The `RepoWorkspace` port is **not** extended; `LocalShellExecutor`
takes an `Arc<GitRepoWorkspace>` (the concrete impl) so it can reuse
the per-path clone lock. A future ADR may split the path-resolution
helper out into a smaller `WorkspaceRoot` port if a non-git workspace
ever lands.

### Native tools: `run_command` and `run_tests`

Two new native tools register through
[`CodeToolExecutor`](../../crates/ork-integrations/src/code_tools.rs) (so
they share the workspace plumbing) and surface in the LLM catalog via
ADR [`0011`](0011-native-llm-tool-calling.md)'s
`tool_descriptors_for_agent`.

`run_command`:

```json
{
  "name": "run_command",
  "description": "Run a command inside a tenant workspace clone. Captures stdout, stderr, and exit code. Times out after the requested duration. argv[0] is the program — no shell expansion.",
  "parameters": {
    "type": "object",
    "properties": {
      "repo": {"type": "string"},
      "cwd": {"type": "string", "default": ""},
      "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1},
      "env": {
        "type": "array",
        "items": {"type": "object", "properties": {"key": {"type": "string"}, "value": {"type": "string"}}, "required": ["key", "value"]}
      },
      "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800},
      "stdin": {"type": "string"}
    },
    "required": ["repo", "argv", "timeout_seconds"]
  }
}
```

Result wire shape (returned as the JSON tool result the LLM sees):

```json
{
  "exit_code": 0,
  "termination": "exited" | "signaled" | "timed_out" | "cancelled",
  "signal": null,
  "duration_ms": 1234,
  "stdout": "string-truncated-to-tail",
  "stderr": "string-truncated-to-tail",
  "stdout_truncated": false,
  "stderr_truncated": false,
  "stdout_artifact_id": null,
  "stderr_artifact_id": null
}
```

`run_tests` is the ergonomic wrapper that runs a known framework and
returns a structured summary. Argv is constructed by the executor
from `framework` so the LLM does not have to remember each
runner's JSON-output flag:

```json
{
  "name": "run_tests",
  "description": "Run a project's test suite using a known framework adapter (cargo test, pytest, jest). Returns a structured summary — pass/fail counts and individual failure locations — alongside the raw stdout/stderr.",
  "parameters": {
    "type": "object",
    "properties": {
      "repo": {"type": "string"},
      "cwd": {"type": "string", "default": ""},
      "framework": {"type": "string", "enum": ["cargo", "pytest", "jest"]},
      "filter": {"type": "string"},
      "extra_args": {"type": "array", "items": {"type": "string"}},
      "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 3600}
    },
    "required": ["repo", "framework", "timeout_seconds"]
  }
}
```

`run_tests` result wire shape:

```json
{
  "framework": "cargo",
  "command": ["cargo", "test", "--no-fail-fast", "--", "-Z", "unstable-options", "--format", "json"],
  "exit_code": 101,
  "termination": "exited",
  "duration_ms": 4231,
  "summary": {
    "passed": 12,
    "failed": 1,
    "skipped": 0,
    "ignored": 2,
    "errored": 0,
    "duration_ms": 4231
  },
  "failures": [
    {
      "name": "engine::workflow::tests::passes_args",
      "file": "crates/ork-core/src/workflow/engine.rs",
      "line": 482,
      "message": "assertion failed: ..."
    }
  ],
  "stdout": "...",
  "stderr": "...",
  "stdout_artifact_id": null,
  "stderr_artifact_id": null
}
```

### `TestResultParser` trait

```rust
// crates/ork-integrations/src/test_runners/mod.rs

use ork_common::error::OrkError;

#[derive(Debug, Clone)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub ignored: u32,
    pub errored: u32,
    pub duration_ms: u64,
    pub failures: Vec<TestFailure>,
}

#[derive(Debug, Clone)]
pub struct TestFailure {
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

pub trait TestResultParser: Send + Sync {
    /// Stable framework id, e.g. "cargo", "pytest", "jest".
    fn framework(&self) -> &'static str;

    /// argv to spawn for this framework, given a normalised request.
    /// Mutates nothing; called by `run_tests` before the spawn.
    fn argv(&self, req: &TestRunRequest) -> Vec<String>;

    /// Parse the captured stdout/stderr after the run finishes.
    /// `exit_code` is informative — most frameworks fail with non-zero
    /// when at least one test fails, but pytest's exit code is 1/2/3/4/5
    /// so the parser owns that mapping.
    fn parse(
        &self,
        exit_code: i32,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<TestSummary, OrkError>;
}
```

Built-in parsers shipped in this ADR:

| `framework()` | argv | Output format the parser consumes |
| ------------- | ---- | --------------------------------- |
| `cargo` | `cargo test --no-fail-fast -- -Z unstable-options --format json --report-time` | libtest JSON event stream (one event per line) |
| `pytest` | `pytest -p no:cacheprovider --tb=short --json-report --json-report-file=-` | `pytest-json-report` payload |
| `jest`  | `jest --json --reporters=default` | jest JSON test result blob |

Other languages (go test, mocha, rspec, junit) are deliberately
**out of scope** for this ADR; adding one is a small follow-up that
implements `TestResultParser` and registers it in the parser
registry.

### Tool wire format details

Truncation strategy: `stdout`/`stderr` strings are at most
`tail_keep_bytes` (default 8 KiB) of the **tail** of the captured
stream — the bottom is what test runners and compilers put their
useful output in. When truncation kicks in, the corresponding
`*_artifact_id` is set to an artifact whose `mime` is
`text/plain; charset=utf-8` and whose scope is the current
`context-<task_id>` (see ADR [`0016`](0016-artifact-storage.md)).
`*_truncated` is `true` whenever an artifact was created.

`termination = "signaled"` carries the signal in the `signal` field;
all other terminations leave `signal` as `null`.

### `LocalAgent` integration

No changes to `LocalAgent` itself. The new tools register in the
existing `CodeToolExecutor`-shaped path:

- `CodeToolExecutor::is_code_tool` grows two arms (`run_command`,
  `run_tests`).
- `CodeToolExecutor::descriptors` returns two more `ToolDescriptor`s.
- `CodeToolExecutor::execute` delegates the new arms to a
  `ShellToolExecutor` field that owns the `Arc<dyn ShellExecutor>`
  plus the `Arc<TestRunnerRegistry>`.
- `CompositeToolExecutor` does not need to change — the arms fall
  through `CodeToolExecutor::is_code_tool` like every other workspace
  tool.

Per-agent tool allow-lists from ADR [`0011`](0011-native-llm-tool-calling.md)
gate visibility: agents whose `tools:` list does not match
`run_command` / `run_tests` (or the wildcard) never see them.

### RBAC scopes (reserved, not enforced)

ADR [`0021`](0021-rbac-scopes.md) owns enforcement. This ADR reserves
the following scope names so prompt authors and DevPortal can begin
modelling them today:

| Scope | Meaning |
| ----- | ------- |
| `tool:run_command:invoke` | Agent may call `run_command` at all |
| `tool:run_tests:invoke` | Agent may call `run_tests` at all |
| `shell:cmd:<program>:invoke` | Per-binary allow-list for `argv[0]` (`shell:cmd:cargo:invoke`, `shell:cmd:git:invoke`, etc.) |
| `shell:cmd:*:invoke` | Wildcard — any program permitted |
| `shell:write:<workspace>:invoke` | Permission to run *any* command (vs. read-only ones) inside a workspace; reserved for a follow-up that distinguishes mutating from non-mutating shell calls |

`LocalShellExecutor` only enforces the workspace-rooted cwd today;
the per-binary scope check lands when ADR 0021's `ScopeChecker` is
threaded through `ToolExecutor::execute`. The per-binary scope is
already shaped to slot into ADR 0021's `<resource>:<id>:<action>`
grammar.

### Tenant credential interaction

`TenantSettings` (ADR [`0010`](0010-mcp-tool-plane.md),
[`0020`](0020-tenant-security-and-trust.md)) gains:

- `shell_env_allowlist: Vec<String>` — environment-variable keys the
  executor will pass through from the host process. Defaults to
  `["LANG", "LC_ALL", "TERM"]`. Anything not on this list and not in
  `req.env` is absent from the child env.
- No new credential fields. Credentials a tool needs (e.g. a
  `GH_TOKEN` for `git push`) flow in via the explicit `req.env` channel
  populated by the agent prompt, **not** automatically from
  `TenantSettings`. This avoids accidentally leaking integration
  credentials into arbitrary shell calls; ADR 0021's per-binary scope
  is the right venue for "this binary may see this secret."

## Acceptance criteria

- [ ] Trait `ShellExecutor` defined at
      [`crates/ork-core/src/ports/shell.rs`](../../crates/ork-core/src/ports/shell.rs)
      with the signature shown in `Decision`.
- [ ] Types `ShellRequest`, `ShellResult`, `ShellTermination` defined
      in the same module and re-exported from
      `crates/ork-core/src/ports/mod.rs`.
- [ ] `LocalShellExecutor` defined at
      `crates/ork-integrations/src/shell.rs`, constructed from an
      `Arc<GitRepoWorkspace>`, an `Arc<dyn ArtifactStore>`, and a
      `LocalShellConfig { default_max_output_bytes, tail_keep_bytes,
      per_workspace_concurrency, default_timeout, max_timeout }`.
- [ ] `LocalShellExecutor::execute` rejects with
      `OrkError::Validation` when:
      - `req.argv` is empty;
      - `req.cwd` contains an absolute path or `..` segment;
      - the resolved cwd canonicalises outside the workspace root;
      - `req.timeout` exceeds `max_timeout`.
- [ ] `LocalShellExecutor::execute` emits `ShellTermination::TimedOut`
      with `exit_code = -1` for a sleep that exceeds `req.timeout`,
      verified by an integration test in
      `crates/ork-integrations/tests/shell_smoke.rs::times_out_long_sleep`.
- [ ] `LocalShellExecutor::execute` emits `ShellTermination::Cancelled`
      when the supplied cancel token fires, verified by
      `crates/ork-integrations/tests/shell_smoke.rs::cancels_on_token`.
- [ ] `LocalShellExecutor::execute` clears the host env (no
      `SSH_AUTH_SOCK`, no `AWS_*`, no `GITHUB_TOKEN`), verified by
      `crates/ork-integrations/tests/shell_smoke.rs::env_isolation`
      which spawns `/usr/bin/env` and asserts the absence of those
      keys.
- [ ] `LocalShellExecutor::execute` truncates stdout/stderr to
      `tail_keep_bytes` and stores the full stream as an artifact,
      verified by
      `crates/ork-integrations/tests/shell_smoke.rs::truncates_large_output`.
- [ ] Trait `TestResultParser` defined at
      `crates/ork-integrations/src/test_runners/mod.rs` with the
      signature shown in `Decision`.
- [ ] `CargoTestParser` (`framework() == "cargo"`) implemented at
      `crates/ork-integrations/src/test_runners/cargo.rs`; round-trip
      test against a fixture libtest-JSON stream in
      `crates/ork-integrations/tests/test_runners_cargo.rs` produces
      the expected `TestSummary` for a known mix of pass/fail/ignored.
- [ ] `PytestParser` (`framework() == "pytest"`) implemented at
      `crates/ork-integrations/src/test_runners/pytest.rs`; round-trip
      test against a fixture pytest-json-report payload in
      `crates/ork-integrations/tests/test_runners_pytest.rs` produces
      the expected `TestSummary`.
- [ ] `JestParser` (`framework() == "jest"`) implemented at
      `crates/ork-integrations/src/test_runners/jest.rs`; round-trip
      test against a fixture jest `--json` payload in
      `crates/ork-integrations/tests/test_runners_jest.rs` produces
      the expected `TestSummary`.
- [ ] `TestRunnerRegistry::lookup("cargo" | "pytest" | "jest")`
      returns the corresponding parser; unknown framework names error
      with `OrkError::Validation`.
- [ ] `CodeToolExecutor::is_code_tool` returns `true` for
      `"run_command"` and `"run_tests"`.
- [ ] `CodeToolExecutor::descriptors` returns the two
      `ToolDescriptor`s shown under `Native tools` with their
      JSON-Schema parameter blocks.
- [ ] `CodeToolExecutor::execute` handles `run_command` by mapping
      the JSON input to a `ShellRequest`, calling
      `ShellExecutor::execute`, and returning the wire shape shown
      under `Tool wire format details`.
- [ ] `CodeToolExecutor::execute` handles `run_tests` by selecting a
      `TestResultParser` via `framework`, building argv via
      `parser.argv(...)`, calling `ShellExecutor::execute`, and
      returning the structured summary plus the truncated stdout/stderr.
- [ ] `cargo test -p ork-integrations shell::` and
      `cargo test -p ork-integrations test_runners::` are green.
- [ ] `cargo test -p ork-agents local::` still passes (no regressions
      in the agent loop from the new descriptors).
- [ ] `TenantSettings.shell_env_allowlist: Vec<String>` field added in
      [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs)
      with default `["LANG", "LC_ALL", "TERM"]` and serde round-trip
      coverage.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row added for
      `0028`.
- [ ] [`metrics.csv`](metrics.csv) row appended on flip to
      `Accepted`/`Implemented`.

## Consequences

### Positive

- Autonomous-coding agents become possible: the LLM can finally run
  the test it broke, read the failure, edit, and re-run inside one
  ADR-0011 tool loop.
- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  verifier-agent gains real teeth — it can execute the artifact under
  review.
- Sandboxing policy lives in one place
  ([`crates/ork-integrations/src/shell.rs`](../../crates/ork-integrations/src/shell.rs))
  and is auditable; today the policy is implicit in
  `GitRepoWorkspace::repo_path`.
- Test parsers feeding structured failure-location data into the LLM
  is significantly cheaper-per-fix than dumping raw stdout (fewer
  tokens, more accurate edits), per the public agent benchmarks the
  team has been tracking.

### Negative / costs

- Shell expands the trust surface considerably. A misconfigured tenant
  workspace, a bug in the cwd canonicalisation, or a missed env-clear
  is a tenant-isolation incident. Mitigations: the hard-coded empty
  base env, the canonicalisation tests in the acceptance criteria, and
  audit events on every spawn give us a defensible posture, but we
  carry the residual risk.
- Process spawning is platform-dependent. On macOS / Linux the SIGTERM
  → SIGKILL ladder works; on Windows we can only `kill` (no signal
  granularity). This ADR ships Unix-shaped behaviour and documents
  Windows as best-effort; a follow-up may add a Job Object integration.
- Test parsers are a maintenance burden. libtest's JSON output is
  unstable (`-Z unstable-options`), pytest-json-report is a third-party
  plugin not bundled with pytest, and jest's `--json` shape varies
  between major versions. Each parser pins the wire shape it reads
  with a fixture file, but version drift will land in user repos and
  surface as `OrkError::Integration` parse failures the LLM has to
  recover from.
- Per-tenant concurrency caps mean a long-running `cargo test` blocks
  other shell calls for that tenant. Default is 4 concurrent, tunable
  per tenant.
- We are committing to a wire shape for `ShellResult` and the
  `run_tests` JSON. Changing it later is a tool-catalog breaking
  change for prompt authors; we accept this in exchange for the
  parsers' structured-failure detail.

### Neutral / follow-ups

- Adding more test frameworks (go test, mocha, rspec, junit) is a
  per-parser ADR-free PR.
- A separate ADR can split shell into its own `crates/ork-shell` crate
  if the parser surface grows large or if the sandboxing policy
  diverges from `RepoWorkspace`.
- ADR [`0021`](0021-rbac-scopes.md) wires `tool:run_command:invoke`
  and `shell:cmd:<program>:invoke` enforcement when its
  `ScopeChecker` lands.
- ADR [`0022`](0022-observability.md) consumes the `shell.spawn` /
  `shell.exit` events.
- A follow-up may add `cargo nextest` (faster CI) and `cargo clippy`
  parsers for lint output.
- ADR [`0024`](0024-wasm-plugin-system.md) plugins do **not** get
  shell access; the plugin sandbox is intentionally narrower than the
  ork-process sandbox.

## Alternatives considered

- **Single `mcp:shell.run` tool from a community MCP server.**
  Rejected: violates ADR [`0010`](0010-mcp-tool-plane.md)'s "internal
  tools stay native" rule and puts the workspace-rooted cwd guarantee
  on the far side of an `rmcp` boundary that cannot enforce it.
  Process-spawn-via-process is also a chicken-and-egg.
- **Extend `RepoWorkspace` with `run_command` directly.** Rejected:
  `RepoWorkspace` is the read-side port (clones, search, file read).
  Mixing process-spawn semantics into it would force every existing
  `RepoWorkspace` impl to grow timeout/cancel/sandbox concerns it
  does not need. A sibling port composes more cleanly.
- **New crate `crates/ork-shell` from the start.** Rejected: a new
  crate per concept is the path to the 30-crate workspace. The
  implementation is small enough to live next to its peers; ADR
  recourse remains if it grows.
- **Per-test-framework arms inside a single executor (no parser
  trait).** Rejected: parsers are pure functions on output strings;
  putting them behind a trait makes the test-fixture-based unit
  testing trivial and lets follow-up parsers land without touching
  shell-execution code.
- **Run tests through a generic JUnit XML adapter.** Rejected for the
  v1 cut: cargo, pytest, and jest all have first-class JSON outputs
  more accurate than their JUnit converters, and JUnit XML loses the
  `passed` count in the common case where it only emits failures.
  JUnit can land as a follow-up parser when a non-JSON-emitting
  framework needs it.
- **Pre-execute test runs from the engine, append output to the
  prompt.** Rejected for the same reason ADR
  [`0011`](0011-native-llm-tool-calling.md) rejected pre-execute
  tools generally: agents need to *decide* when to run a test, not
  have one run pre-emptively.
- **Use `tokio::process::Command::new("sh").arg("-c").arg(cmd)`.**
  Rejected: shell metacharacter injection becomes the executor's
  problem; argv-only semantics make injection an explicit caller
  choice.

## Affected ork modules

- New: [`crates/ork-core/src/ports/shell.rs`](../../crates/ork-core/src/ports/shell.rs)
  — `ShellExecutor` port and its types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs)
  — re-export `shell`.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs)
  — add `shell_env_allowlist`.
- New: `crates/ork-integrations/src/shell.rs` — `LocalShellExecutor`,
  spawn/wait/cancel ladder, output capture + truncation.
- New: `crates/ork-integrations/src/test_runners/{mod.rs, cargo.rs,
  pytest.rs, jest.rs}` — parser trait, registry, three built-ins.
- [`crates/ork-integrations/src/code_tools.rs`](../../crates/ork-integrations/src/code_tools.rs)
  — register `run_command` / `run_tests` tools, hold the
  `Arc<dyn ShellExecutor>` and `Arc<TestRunnerRegistry>` fields.
- [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs)
  — public re-exports for `LocalShellExecutor` + the parser registry.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
  — boot `LocalShellExecutor` with the existing `GitRepoWorkspace`
  and `ArtifactStore`, wire it into the `CodeToolExecutor` builder.
- [`config/default.toml`](../../config/default.toml) — `[shell]`
  section with `default_max_output_bytes`, `tail_keep_bytes`,
  `per_workspace_concurrency`, `default_timeout_seconds`,
  `max_timeout_seconds`.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Solace Agent Mesh | no first-class shell tool; SAM relies on MCP servers (`mcp-shell`, etc.) for execution | `LocalShellExecutor` + `run_command` / `run_tests` natively, with workspace-rooted sandbox |
| LangGraph / OpenHands | `BashSession` + per-task sandbox container (Docker / Modal) | `LocalShellExecutor` (no container today) + `tail_keep_bytes` truncation policy |
| Aider | `cmd` block parser + heuristic test invocation | `run_tests` framework dispatch + `TestResultParser` |
| GitHub Copilot Workspace | structured test-failure rendering from runner JSON | `TestSummary { failures: [{name, file, line, message}] }` |
| Claude Code (this CLI) | `Bash` tool with timeout + truncation | `run_command` argv shape + truncation-with-spill policy |

## Open questions

- **Containerised execution.** Should `LocalShellExecutor` shell out
  via `firejail` / `bubblewrap` / a Docker daemon for stronger
  isolation than per-tenant cwds? Decision: not for the v1 cut. The
  current sandbox is process-level and matches what
  `GitRepoWorkspace` already commits to. A follow-up ADR can layer a
  containerised executor under the same port.
- **Network egress policy.** `cargo test` typically needs network for
  fresh crate fetches; `npm install` doubly so. Do we ship a
  no-network mode behind a tenant flag? Decision: defer; current
  default is "host network, audited via `shell.spawn` events." A
  follow-up tenant flag can flip in a `network: "none"` mode using
  the same containerisation work.
- **Parallel-tool-call interaction.** ADR-0011 fans out tool calls
  with `try_join_all` (default 4). `run_command` honours that, but
  long shell calls may starve the per-tenant semaphore. We accept
  this; tuning is per-tenant config.
- **`ChildStdout` streaming to the SSE.** Today `run_command` returns
  output only after the process exits. A follow-up may stream stdout
  deltas through `AgentEvent::status_text` so the user sees a
  long-running build's progress. Out of scope here.
- **Windows.** Acceptance criteria target Unix shells; Windows
  support is tracked separately. CI runs the smoke tests only on
  Linux for v1.

## References

- ADR [`0002`](0002-agent-port.md) — `Agent` port (caller of the
  ADR-0011 tool loop).
- ADR [`0010`](0010-mcp-tool-plane.md) — "internal tools stay native".
- ADR [`0011`](0011-native-llm-tool-calling.md) — tool loop, output
  truncation policy, `tool_descriptors_for_agent`.
- ADR [`0016`](0016-artifact-storage.md) — artifact spill for large
  stdout/stderr.
- ADR [`0020`](0020-tenant-security-and-trust.md) — tenant trust frame
  this ADR composes with.
- ADR [`0021`](0021-rbac-scopes.md) — scope vocabulary that enforces
  the names this ADR reserves.
- libtest JSON output:
  <https://doc.rust-lang.org/cargo/commands/cargo-test.html>
  (`-Z unstable-options --format json`).
- `pytest-json-report`:
  <https://github.com/numirias/pytest-json-report>.
- jest CLI `--json`:
  <https://jestjs.io/docs/cli#--json>.
