# 0024 — WASM-based plugin system

- **Status:** Proposed
- **Date:** 2026-04-26
- **Phase:** 3
- **Relates to:** 0010, 0011, 0013, 0014, 0015, 0021, 0023
- **Supersedes:** [`0014`](0014-plugin-system.md)

## Context

Third-party extensibility for ork was first specified in
[`0014`](0014-plugin-system.md), which proposed a "Cargo source plugin"
model: each plugin is a crate, ork generates a `local-server/main.rs`
that imports each plugin's `register()` function, and the operator
runs `cargo build` to produce a custom `ork-server`. That model has
two real costs:

1. **The build-step tax.** Every plugin install or upgrade triggers a
   full Rust recompile (5–10 minutes cold, 30 s warm on a beefy
   workstation). That is fine for ork-team-maintained adapters added
   to the workspace, but a rough operator UX for "drop in a Slack
   gateway and restart". SAM's `pip install` round-trip is
   ~10 seconds; we are off by two orders of magnitude.
2. **No isolation.** A Tier-1 plugin in [`0014`](0014-plugin-system.md)
   runs in-process with full access to the address space, environment,
   filesystem and network. The ADR explicitly defers sandboxing to
   "the operator's responsibility". For an open marketplace of
   community plugins that is a real attack surface — a single
   compromised crate exfiltrates every secret in the process.

There is a working in-house prototype that demonstrates the WASM
alternative is feasible and inexpensive to operate: **wasmops**, a
self-hosted FaaS at [`~/wasmops`](https://example.invalid/wasmops/) (out
of repo). The relevant findings from reading the source:

- **Runtime stack.** wasmops embeds [`wasmtime`](https://wasmtime.dev/)
  43.x with `wasmtime-wasi` and runs `wasm32-wasip1` modules. The
  engine is configured with `consume_fuel(true)`,
  `epoch_interruption(true)`, and `wasm_component_model(false)`. See
  `~/wasmops/src/runtime/engine.rs`.
- **Per-invocation isolation.** Each call gets its own `Store` with a
  `StoreLimits::memory_size`, a fuel budget, and an epoch deadline.
  Stdin/stdout/stderr are `MemoryInputPipe` / `MemoryOutputPipe` — no
  filesystem, no network unless the host explicitly imports one. See
  `~/wasmops/src/runtime/executor.rs`.
- **Cold-start mitigation.** `Module::new` is run on
  `spawn_blocking`, then `Linker::instantiate_pre` produces an
  `InstancePre<HostState>` that is cached per `(function_id, version)`
  in an LRU with TTL. Repeat invocations skip the compile and
  instantiation cost. See `~/wasmops/src/runtime/pool.rs`.
- **Plain-old-Wasm interface.** Functions are standard `wasm32-wasip1`
  modules. Input is JSON on stdin; output is JSON on stdout; errors
  are stderr. Any toolchain that targets WASI works (Rust, TinyGo,
  AssemblyScript, Zig, C). The example
  `~/wasmops/examples/hello/src/main.rs` is 12 lines.
- **Custom host imports as a capability surface.** wasmops registers
  a `wasmops` module on the linker with four host functions
  (`kv_get`, `kv_put`, `kv_delete`, `kv_list`) that read raw bytes
  out of guest memory by `(ptr, len)` pairs. A guest crate
  (`guest-kv`) wraps the raw FFI in a typed Rust API. See
  `~/wasmops/src/runtime/host_kv.rs` and `~/wasmops/guest-kv/src/`.
  This pattern — module name + flat function list + guest helper
  crate — is exactly what ork needs for `ork.kv`, `ork.http_fetch`,
  `ork.log`, `ork.config_get`.
- **Single binary, embedded storage.** wasmops uses SQLite for
  metadata and `dbase` for KV. ork already has Postgres + Redis; we
  do not need to inherit the storage layer, only the runtime crate
  layout.

The hard invariants ork is built on
([`AGENTS.md`](../../AGENTS.md) §3) point at WASM as the right shape
for this problem:

- A2A-first ([`0002`](0002-agent-port.md)): plugins must fit behind
  the same trait surfaces (`Agent`, `ToolExecutor`,
  `GenericGatewayAdapter`) that in-tree code uses.
- Hexagonal boundaries ([`AGENTS.md`](../../AGENTS.md) §3.5):
  `ork-core` does not import `wasmtime`. The runtime lives in a new
  leaf crate; the glue lives in `ork-integrations` (already imports
  third-party I/O clients).
- MCP for external tools ([`0010`](0010-mcp-tool-plane.md)) covers
  the IPC case. WASM covers the **in-process, sandboxed, language-
  agnostic** case that MCP cannot — a tool that needs to manipulate
  bytes at native speed without the JSON-RPC + process-spawn round
  trip.

## Decision

ork **adopts a WASM-based plugin model** as the public extensibility
surface, replacing the Cargo-source path proposed in
[`0014`](0014-plugin-system.md). [`0014`](0014-plugin-system.md) is
flipped to `Superseded by 0024` and the in-tree adapter pattern it
describes (workspace member crates that depend on `ork-*`) remains
available for ork-team work but is no longer called a "plugin".

The model has three layers:

1. **Runtime** — a new `crates/ork-wasm/` crate embeds wasmtime,
   exposes the `ork` host import module, and caches pre-instantiated
   modules. Mirrors `~/wasmops/src/runtime/`.
2. **Plugin manifest + distribution** — a single signed `.wasm`
   artifact accompanied by an `ork-wasm-plugin.toml` manifest.
   Distributed via the existing `ork-plugin-catalog` repo (the
   catalog idea from [`0014`](0014-plugin-system.md) carries over,
   only the artifact format changes).
3. **Integration with ork's ports** — phased. v1 only ships
   `ToolExecutor`-backed plugins. `Agent` and
   `GenericGatewayAdapter` plugins are deferred to a follow-up ADR
   that introduces the WASI 0.2 Component Model bindings.

### Runtime: `crates/ork-wasm`

```text
crates/ork-wasm/
├── Cargo.toml                       # depends on wasmtime, wasmtime-wasi, ork-common
├── src/
│   ├── lib.rs
│   ├── engine.rs                    # WasmEngine: cache + linker + execute()
│   ├── executor.rs                  # per-invocation Store + WASI pipes (mirrors wasmops)
│   ├── host_imports/
│   │   ├── mod.rs                   # register_ork_module(linker, caps)
│   │   ├── kv.rs                    # ork.kv_get / kv_put / kv_delete / kv_list
│   │   ├── http.rs                  # ork.http_fetch (capability-gated)
│   │   ├── log.rs                   # ork.log (level, ptr, len)
│   │   ├── time.rs                  # ork.now_ms
│   │   └── config.rs                # ork.config_get (manifest config block)
│   ├── manifest.rs                  # PluginManifest deserialiser
│   ├── registry.rs                  # PluginRegistry: load/unload/list
│   └── store.rs                     # PluginStore: filesystem layout under plugins_dir/
└── tests/
    └── tool_smoke.rs                # builds examples/wasm-plugin-hello, executes it
```

`WasmEngine` is the single Wasmtime engine for the process:

```rust
pub struct WasmEngine {
    engine: wasmtime::Engine,
    linker: Arc<wasmtime::Linker<HostState>>,
    cache: ModuleCache,                        // (plugin_id, version) -> InstancePre
    semaphore: Arc<tokio::sync::Semaphore>,    // bounds concurrent executions
}

pub struct WasmEngineConfig {
    pub plugins_dir: PathBuf,
    pub max_pool_size: usize,
    pub max_concurrent_executions: usize,
    pub default_fuel_limit: u64,
    pub default_memory_limit_bytes: usize,
    pub default_timeout_ms: u64,
    pub epoch_tick_ms: u64,
}

pub struct WasmInvocation {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration: Duration,
    pub fuel_consumed: u64,
    pub status: WasmStatus,                    // Success | Error | Timeout | OutOfFuel
}

impl WasmEngine {
    pub fn new(cfg: &WasmEngineConfig) -> Result<Self, OrkError>;
    pub async fn load_plugin(&self, manifest: &PluginManifest, wasm_bytes: Vec<u8>)
        -> Result<(), OrkError>;
    pub async fn execute(
        &self,
        plugin_id: &str,
        input: &[u8],
        per_call: PerCallLimits,
        host_ctx: HostInvocationContext,
    ) -> Result<WasmInvocation, OrkError>;
    pub async fn unload(&self, plugin_id: &str);
}
```

`HostInvocationContext` carries the calling tenant id, request id,
trace context, and a closure that the host can call to enforce RBAC
on `http_fetch` outbound URLs (covered by
[`0021`](0021-rbac-scopes.md)). Engine-level epoch ticking runs in a
dedicated background tokio task identical to the wasmops loop.

### `ork` host import module

A guest declares `#[link(wasm_import_module = "ork")]` and calls flat
C-ABI functions. Same shape as the wasmops `wasmops` module. The
canonical signatures:

```rust
// In a guest helper crate (`ork-wasm-guest`), behind a typed wrapper.
#[link(wasm_import_module = "ork")]
unsafe extern "C" {
    // KV — keys/values are byte slices; same convention as wasmops kv_get.
    pub fn kv_get(key_ptr: *const u8, key_len: i32,
                  buf_ptr: *mut u8,   buf_cap: i32) -> i32;
    pub fn kv_put(key_ptr: *const u8, key_len: i32,
                  val_ptr: *const u8, val_len: i32) -> i32;
    pub fn kv_delete(key_ptr: *const u8, key_len: i32) -> i32;
    pub fn kv_list(prefix_ptr: *const u8, prefix_len: i32,
                   buf_ptr: *mut u8, buf_cap: i32) -> i32;

    // Logging — level: 0=trace, 1=debug, 2=info, 3=warn, 4=error.
    pub fn log(level: i32, msg_ptr: *const u8, msg_len: i32);

    // Wallclock millis since UNIX_EPOCH.
    pub fn now_ms() -> i64;

    // Read a value from the manifest's [config] block (typed JSON string).
    pub fn config_get(key_ptr: *const u8, key_len: i32,
                      buf_ptr: *mut u8,   buf_cap: i32) -> i32;

    // Capability-gated outbound HTTP. URL must match an `allowed_hosts`
    // entry in the manifest; returns -1 (denied), -2 (transport error),
    // or the response body length on success (with body written into buf).
    pub fn http_fetch(url_ptr: *const u8, url_len: i32,
                      method_ptr: *const u8, method_len: i32,
                      body_ptr: *const u8,   body_len: i32,
                      buf_ptr: *mut u8,      buf_cap: i32) -> i32;
}
```

Per-plugin KV is namespaced by `plugin_id` host-side, exactly the way
wasmops namespaces by `function_id`
(`~/wasmops/src/runtime/kv_store.rs`). Storage lives in Postgres
(table `wasm_plugin_kv (plugin_id, key, value, updated_at)`); we
re-use the existing `sqlx` pool rather than embedding `dbase`.

A typed guest helper crate `ork-wasm-guest` (analogous to
`~/wasmops/guest-kv/`) wraps the FFI for Rust authors. Plugin
authors using other languages call the imports directly.

### Wire protocol: stdin/stdout JSON

For v1 (tools) the convention matches wasmops:

- The host writes a JSON request object on the guest's `stdin`.
- The guest module's `_start` (i.e. `fn main()`) reads `stdin`,
  produces a JSON response on `stdout`, and exits.
- `stderr` is captured as logs.
- A non-zero exit code means the call failed; `stderr` is surfaced
  as the error message.

This keeps the v1 plugin contract identical to a CLI program reading
JSON, which makes plugins trivial to test outside ork (`echo '{...}'
| ./plugin.wasm` under any WASI-aware runtime).

The richer surfaces — typed streaming events for `Agent`, async
translate functions for gateways — are explicitly **not** modelled
on stdio JSON. They wait for the Component Model ADR (see Open
questions).

### Plugin manifest

A plugin is distributed as exactly two files: `<plugin>.wasm` and
`ork-wasm-plugin.toml`.

```toml
[plugin]
id = "ork-tool-jira-search"          # globally unique; matches catalog entry
name = "Jira Search"
version = "0.1.0"
type = "tool"                         # v1: only "tool" is supported
ork_api_version = ">=0.5,<0.6"        # ork-wasm host ABI version
description = "Search Jira issues from a workflow step."
homepage = "https://github.com/example/ork-tool-jira-search"
license = "Apache-2.0"

[runtime]
fuel_limit = 1_000_000_000            # optional override of host default
memory_limit_bytes = 67_108_864       # 64 MiB
timeout_ms = 5000

[capabilities]
# Empty by default. The host enforces these at the linker.
allowed_hosts = ["jira.example.com:443"]
kv_enabled = true

[tool]
# Tool-type metadata; mirrors ToolDescriptor (ports/llm.rs).
name = "jira_search"                  # what the LLM sees
description = "Search Jira issues by JQL."
parameters_schema_file = "tool.schema.json"

[config_schema]
# JSON Schema for the [config] block in config/default.toml's
# [plugins.<id>.config] section.
file = "config.schema.json"

[signing]
# Optional; required when the catalog flag enforce_signed = true.
public_key = "age1..."                # ed25519, base64
signature_file = "ork-wasm-plugin.sig"
```

### Workflow integration

A WASM tool plugin appears as just another tool in the workflow YAML.
The plugin id becomes the tool name; the tool's `ToolDescriptor` is
synthesised from the manifest's `[tool]` block plus the supplied
JSON schema file. From the workflow author's perspective the
following two YAMLs are interchangeable:

```yaml
# Native tool (in-tree)
- name: search-issues
  tool: github_recent_activity
  input:
    owner: example
    repo:  cool

# WASM plugin tool (third-party)
- name: search-issues
  tool: ork-tool-jira-search/jira_search
  input:
    jql: "project = PROJ AND status = Open"
```

The `WasmToolExecutor` adapter implements
`ork_core::workflow::engine::ToolExecutor`:

```rust
// crates/ork-integrations/src/wasm_tool.rs
pub struct WasmToolExecutor {
    engine: Arc<ork_wasm::WasmEngine>,
    plugin_id: String,
    descriptor: ToolDescriptor,
}

#[async_trait::async_trait]
impl ToolExecutor for WasmToolExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> { /* … */ }
}
```

The descriptor is registered with the existing tool catalog
([`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs))
so native and WASM tools are indistinguishable to LLM tool-calling
([`0011`](0011-native-llm-tool-calling.md)).

### CLI

`ork plugin` operates against a single `plugins_dir` (default
`~/.ork/plugins/`). No re-build of `ork-server` is required at any
point — the binary loads `.wasm` files at boot from `plugins_dir`.

```text
ork plugin install <path-or-url>     # copies .wasm + manifest into plugins_dir
ork plugin list                      # tabular output: id, version, type, status
ork plugin remove <id>
ork plugin verify [<id>]             # checks signatures + ork_api_version
ork plugin show <id>                 # prints the manifest
```

`install` accepts a local file path, an HTTPS URL to a `.wasm`
artifact, or a `<catalog>:<id>` shorthand resolved against the
catalog repo from [`0014`](0014-plugin-system.md) (catalog format
unchanged; only the artifact column changes from "crate name" to
"wasm artifact URL + manifest URL").

### Configuration surface

```toml
# config/default.toml
[plugins.wasm]
enabled = true
plugins_dir = "~/.ork/plugins"
max_pool_size = 100
max_concurrent_executions = 32
default_fuel_limit = 1_000_000_000
default_memory_limit_bytes = 268_435_456    # 256 MiB
default_timeout_ms = 5_000
epoch_tick_ms = 10
require_signed = false                       # production deployments flip this

[plugins.wasm.catalog]
url = "https://github.com/ork-platform/ork-plugin-catalog"
trusted_signers = ["age1ork-team-key..."]

# Per-plugin config — exposed to the guest via ork.config_get.
[plugins."ork-tool-jira-search".config]
base_url = "https://jira.example.com"
auth_token = { env = "JIRA_TOKEN" }
```

Sensitive values use the existing `_env` indirection from
[`0007`](0007-remote-a2a-agent-client.md). The host resolves them at
boot and serves them through `ork.config_get`; the guest never sees
the env-var name.

### Sandboxing model

Per-invocation:

- Fresh `Store<HostState>` per call.
- Memory ceiling enforced via `StoreLimitsBuilder::memory_size`.
- CPU bounded by fuel (`Store::set_fuel` + `consume_fuel`).
- Wallclock bounded by epoch deadline + a background ticker.
- WASI capabilities default to empty (no preopens, no env vars, no
  inherit-stdio).
- Outbound HTTP only via `ork.http_fetch`, allowlisted by
  `[capabilities].allowed_hosts`.
- KV scoped to the plugin id; cross-plugin reads are impossible.

Process-wide:

- A single `tokio::sync::Semaphore` bounds the number of concurrent
  WASM invocations (matches `~/wasmops/src/runtime/engine.rs`'s
  `execution_semaphore`).
- One `wasmtime::Engine` instance shared across plugins; modules are
  compiled once and cached pre-instantiated.

This is strictly stronger than [`0014`](0014-plugin-system.md)'s
"trust the operator" model.

### Hex boundaries

| Crate | Allowed to depend on | Role |
| ----- | --------------------| ----- |
| `crates/ork-wasm/` (new) | `wasmtime`, `wasmtime-wasi`, `ork-common`, `serde`, `tokio`, `sqlx` | Runtime, host imports, manifest, registry. |
| `crates/ork-integrations/` | `ork-wasm`, `ork-core`, existing deps | `WasmToolExecutor` adapter implementing the core `ToolExecutor` trait. |
| `crates/ork-api/` | `ork-wasm` (boot only), `ork-integrations` | Loads plugins from `plugins_dir` at startup; registers each one with the tool catalog. |
| `crates/ork-cli/` | `ork-wasm` (manifest types only) | `ork plugin` subcommands. |
| `crates/ork-core/` | **must not** import `wasmtime` | Workflow + ports are runtime-agnostic. |

This matches the rule that domain crates do not import I/O drivers
([`AGENTS.md`](../../AGENTS.md) §3.5).

### What this ADR explicitly does **not** do (deferred)

- **Agent and gateway plugin types.** The stdio-JSON wire is wrong
  for streaming `AgentEvent`s and for the bidirectional
  `translate_inbound`/`translate_outbound` shape from
  [`0013`](0013-generic-gateway-abstraction.md). A follow-up ADR
  introduces WIT-based bindings and the WASI 0.2 Component Model and
  defines `ork.agent` / `ork.gateway` host worlds.
- **Hot reload.** Operators restart `ork-server` to pick up new or
  upgraded plugins. The runtime supports `unload`, but the API
  endpoints to drive it from outside are out of scope.
- **GPU/Native hand-off.** Plugins that need GPU or native libraries
  ship as MCP servers ([`0010`](0010-mcp-tool-plane.md)).
- **Per-tenant plugin isolation.** All plugins are process-wide.
  Multi-tenant scoping ("tenant A may use plugin X, tenant B may
  not") is layered on by RBAC ([`0021`](0021-rbac-scopes.md)) at the
  tool-catalog level, not by spinning up separate `WasmEngine`s.

## Acceptance criteria

- [ ] New crate `crates/ork-wasm/` exists with `Cargo.toml` declaring
      `wasmtime`, `wasmtime-wasi`, `tokio`, `sqlx`, `serde`,
      `ork-common` as dependencies, version-pinned in the workspace
      `Cargo.toml` `[workspace.dependencies]` table.
- [ ] `crates/ork-wasm/src/engine.rs` defines `WasmEngine`,
      `WasmEngineConfig`, `WasmInvocation`, and `PerCallLimits` with
      the signatures shown in `Decision`.
- [ ] `crates/ork-wasm/src/executor.rs` builds a fresh `Store` per
      call with `consume_fuel(true)`, `epoch_interruption(true)`, a
      `StoreLimitsBuilder::memory_size(...)` ceiling, and isolated
      `MemoryInputPipe`/`MemoryOutputPipe` WASI stdio (mirrors
      `~/wasmops/src/runtime/executor.rs`).
- [ ] Background epoch ticker task spawned by `WasmEngine::new` and
      shut down on drop; tick interval driven by
      `WasmEngineConfig::epoch_tick_ms`.
- [ ] Module cache `crates/ork-wasm/src/cache.rs` keyed by
      `(plugin_id, version)`, LRU bounded by `max_pool_size`, with a
      TTL eviction matching the wasmops pool default.
- [ ] Host import module `ork` registered via
      `crates/ork-wasm/src/host_imports/mod.rs::register_ork_module`,
      providing `kv_get`, `kv_put`, `kv_delete`, `kv_list`, `log`,
      `now_ms`, `config_get`, `http_fetch` with the C ABI shown in
      `Decision`.
- [ ] `http_fetch` enforces `[capabilities].allowed_hosts` and
      returns -1 for denied requests; integration test
      `crates/ork-wasm/tests/host_http_caps.rs::denies_offlist`
      asserts this.
- [ ] Per-plugin KV persisted via `sqlx` migration
      `migrations/NNN_wasm_plugin_kv.sql` creating
      `wasm_plugin_kv (plugin_id text, key text, value bytea,
      updated_at timestamptz, primary key(plugin_id, key))`.
- [ ] `crates/ork-wasm/src/manifest.rs` defines `PluginManifest`
      (and `[plugin]`, `[runtime]`, `[capabilities]`, `[tool]`,
      `[config_schema]`, `[signing]` substructs) deserialisable from
      the TOML shown in `Decision`. Round-trip test
      `crates/ork-wasm/tests/manifest_roundtrip.rs::sample_manifest`.
- [ ] `crates/ork-integrations/src/wasm_tool.rs` defines
      `WasmToolExecutor` implementing
      `ork_core::workflow::engine::ToolExecutor` and a
      `register_wasm_plugins(catalog: &mut IntegrationToolExecutor,
      engine: &Arc<WasmEngine>, plugins_dir: &Path)` boot helper.
- [ ] `crates/ork-cli` exposes
      `ork plugin install <path-or-url>`,
      `ork plugin list`,
      `ork plugin remove <id>`,
      `ork plugin verify [<id>]`,
      `ork plugin show <id>`. Snapshot test on `--help` output.
- [ ] `examples/wasm-plugin-hello/` is a `wasm32-wasip1` Cargo crate
      that reads JSON `{ "name": "..." }` from stdin and writes
      `{ "greeting": "Hello, ..." }` to stdout, with an
      `ork-wasm-plugin.toml` matching the schema.
- [ ] Integration test
      `crates/ork-wasm/tests/tool_smoke.rs::execute_hello_plugin`
      compiles `examples/wasm-plugin-hello`, registers it with a
      `WasmEngine`, executes it via `WasmToolExecutor` against an
      `AgentContext` fixture, and asserts on the output JSON, fuel
      consumption (> 0), and duration (> 0).
- [ ] `cargo test -p ork-wasm` is green.
- [ ] `cargo test -p ork-integrations wasm_tool::` is green.
- [ ] `config/default.toml` adds the `[plugins.wasm]` and
      `[plugins.wasm.catalog]` sections shown in `Decision` with
      defaults documented inline.
- [ ] `ork-api` boot path
      ([`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs))
      reads the new `[plugins.wasm]` block, constructs a
      `WasmEngine`, walks `plugins_dir`, and registers each
      plugin's `ToolExecutor` with the existing tool catalog before
      the `WorkflowEngine` is wired.
- [ ] When `[plugins.wasm].enabled = false`, ork boots without
      touching `wasmtime` (verified by an integration test that
      asserts the plugin loop is not entered).
- [ ] [`0014`](0014-plugin-system.md) status header changed to
      `Superseded by 0024`.
- [ ] [`README.md`](README.md) ADR index row for 0014 updated to
      reflect the new status; new row added for 0024 in the same
      Phase 3 block.
- [ ] [`metrics.csv`](metrics.csv) row appended (see
      [`METRICS.md`](METRICS.md)).

## Consequences

### Positive

- **Real isolation.** Memory + fuel + epoch + capability gates make a
  malicious plugin a contained problem rather than a process-wide
  exfiltration. The threat model matches what cloud FaaS vendors
  (Fastly Compute, Cloudflare Workers, Fermyon Spin) ship in
  production.
- **No per-install rebuild.** `ork plugin install` becomes a file
  copy + a service restart, not a 5-minute Cargo rebuild. Operators
  ship one statically linked `ork-server` and add `.wasm` files to a
  directory.
- **Language-agnostic.** Anything that compiles to `wasm32-wasip1`
  works (Rust, TinyGo, AssemblyScript, Zig, C). Catalogue can grow
  without forcing every author to learn ork's Rust trait surfaces.
- **Stable plugin ABI.** WASM bytecode + WASI p1 + the `ork` host
  module is a frozen contract. Bumping `wasmtime` major versions
  does not invalidate plugins as long as the host module signatures
  hold.
- **Testable in isolation.** `wasmtime` + a stub `HostState` runs the
  plugin out of band of the rest of ork, which is much smaller test
  surface than spinning up a full ork-server for a plugin smoke test.
- **Reuses proven design.** wasmops has been running this shape in
  production-adjacent loads; the runtime architecture is borrowed
  rather than invented.

### Negative / costs

- **Wasmtime is a heavy dep.** `wasmtime` 43.x is ~12 MB of compiled
  artifact and a non-trivial number of transitive crates. The
  `[plugins.wasm].enabled = false` flag mitigates this for
  deployments that do not want plugins at all (still pays the
  compile-time cost; `--no-default-features` on `ork-wasm` is a
  follow-up).
- **Cold start.** First invocation of a plugin pays the
  `Module::new` + `instantiate_pre` cost. wasmops measures this at
  10–50 ms for a small Rust plugin; the LRU cache makes it a
  one-time cost. Documented and measured in
  `crates/ork-wasm/benches/cold_start.rs`.
- **stdio-JSON contract is too narrow for streaming agents.** A
  follow-up ADR is required to add Component Model bindings before
  WASM `Agent`s and gateways are possible.
- **Operational debt.** Catalog signing keys, catalog repo
  governance, vulnerability disclosure for shipped plugins — these
  are real ops work that an in-tree-only model would not need.

### Neutral / follow-ups

- Component Model adoption ADR (host worlds for agent and gateway
  plugins, WIT bindgen, async streams) — the natural next step.
- A `--no-default-features` mode on `ork-wasm` so that
  zero-plugin deployments do not pay wasmtime's compile cost.
- A `wasi-preview2` migration when guest toolchains catch up.
- An `ork plugin dev` subcommand that scaffolds a plugin crate
  from a template (parallel to `ork init` from
  [`0023`](0023-migration-and-rollout-plan.md)).

## Alternatives considered

- **Cargo source plugins ([`0014`](0014-plugin-system.md)).** The
  predecessor decision. Rejected here because the per-install Cargo
  rebuild and the no-isolation posture make it a poor public
  extension surface. Retains its value for in-tree adapters that
  ork ships itself, but those are not "plugins".
- **WASI Preview 2 / Component Model on day one.** Tempting because
  it provides typed async streaming bindings (a much better fit for
  agents and gateways). Rejected for v1 because: (a) WASI 0.2
  toolchain support outside Rust is still rough as of mid-2026,
  (b) `wasm-tools component new` adds a build-step that not every
  language community has fluent ergonomics for, (c) the wasmops
  prototype that informed this ADR uses preview 1 and works.
  Adopted as the explicit successor for richer plugin types
  (`Agent`, `GenericGatewayAdapter`).
- **Dynamic libraries (`libloading`).** Rejected per
  [`0014`](0014-plugin-system.md): Rust ABI is not stable across
  compiler versions; UX is "rebuild your plugin every time ork
  updates" without any of the safety upside.
- **Python plugins via PyO3.** Rejected per
  [`0014`](0014-plugin-system.md): drags Python into the ork
  process and recreates SAM's stack inside ork.
- **MCP only.** [`0010`](0010-mcp-tool-plane.md) covers external,
  out-of-process tools. It is the right answer for tools that need
  GPU, native libraries, or a separate language runtime. It is the
  wrong answer for the "drop a tiny pure function into ork without
  a separate process" case, which is what plugins are for.
- **Embed Wasmer instead of Wasmtime.** Wasmer has a friendlier
  packaging story (`WAPM`). Rejected because (a) wasmtime is the
  reference WASI implementation backed by the Bytecode Alliance,
  (b) wasmtime ergonomics in async Rust + tokio are well understood
  in this codebase via the wasmops prototype, (c) feature parity
  on epoch interruption + fuel + Component Model is on wasmtime's
  side first.
- **Build a fresh sandbox using `seccomp` + a forked process.**
  Possible on Linux only, no good Mac/Windows story, much harder to
  ship as a single binary, no language portability story. Rejected.

## Affected ork modules

- [`crates/ork-wasm/`](../../crates/) — **new crate**: WasmEngine,
  host imports, manifest, registry, plugin store. Phase boundary —
  this is the leaf crate that owns wasmtime.
- [`crates/ork-integrations/src/`](../../crates/ork-integrations/src/) —
  new file `wasm_tool.rs` defining `WasmToolExecutor` +
  `register_wasm_plugins`; one new entry in
  [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/lib.rs).
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) —
  adds the `ork plugin` subcommand tree and shells out to
  `ork-wasm` for manifest validation.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) —
  boot wiring: build `WasmEngine` from config, scan `plugins_dir`,
  register each plugin's `ToolExecutor` with the integration
  catalog before the `WorkflowEngine` is constructed.
- [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs) —
  adds `WasmPluginsConfig` to `OrkConfig` and parses
  `[plugins.wasm]`.
- [`config/default.toml`](../../config/default.toml) — adds
  `[plugins.wasm]`, `[plugins.wasm.catalog]`, and a documented
  `[plugins."<id>".config]` example.
- [`Cargo.toml`](../../Cargo.toml) — adds `wasmtime` and
  `wasmtime-wasi` to `[workspace.dependencies]`, pinned to the same
  major as wasmops uses (43.x at the time of writing).
- [`migrations/`](../../migrations/) — new migration
  `NNN_wasm_plugin_kv.sql` for the per-plugin KV table.
- New repo (out of this workspace): `ork-wasm-guest` — typed Rust
  helper crate wrapping the `ork` host module imports. Mirrors
  `~/wasmops/guest-kv/` for ork.
- New repo (out of this workspace): `ork-plugin-catalog` —
  unchanged from [`0014`](0014-plugin-system.md), only the entry
  format changes (artifact URL + manifest URL instead of crate
  name).

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on
the implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3,
step 3). Each finding gets one of:

- **Fixed in-session** — link to the commit / PR that addressed it.
- **Acknowledged, deferred** — link to the follow-up ADR or issue.
- **Rejected** — short justification.

Leave empty until the implementation lands.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| wasmops | `~/wasmops/src/runtime/engine.rs` | `crates/ork-wasm/src/engine.rs` (engine config: `consume_fuel + epoch_interruption + wasm_component_model(false)`). |
| wasmops | `~/wasmops/src/runtime/executor.rs` | `crates/ork-wasm/src/executor.rs` (per-call `Store`, fuel/memory/epoch limits, `MemoryInputPipe`/`MemoryOutputPipe`). |
| wasmops | `~/wasmops/src/runtime/pool.rs` | `crates/ork-wasm/src/cache.rs` (LRU cache of `Arc<InstancePre<HostState>>`, TTL eviction). |
| wasmops | `~/wasmops/src/runtime/host_kv.rs` | `crates/ork-wasm/src/host_imports/kv.rs` (host module + `(ptr, len)` ABI). |
| wasmops | `~/wasmops/guest-kv/src/` | New `ork-wasm-guest` crate (typed wrapper over `ork` host module). |
| wasmops | `~/wasmops/examples/hello/src/main.rs` | `examples/wasm-plugin-hello/src/main.rs` (stdio-JSON tool). |
| Fastly Compute@Edge | <https://developer.fastly.com/learning/compute/> | Confirms WASM + WASI + capability-gated host imports as a productionised pattern for sandboxed third-party code. |
| Cloudflare Workers | <https://developers.cloudflare.com/workers/runtime-apis/webassembly/> | Same shape; capability-gated host calls into platform APIs. |
| Fermyon Spin | <https://developer.fermyon.com/spin> | Wasmtime-backed component model FaaS; informs the deferred Component Model successor ADR. |
| SAM | [`cli/commands/plugin_cmd/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/cli/commands/plugin_cmd) | The `ork plugin add` UX from [`0014`](0014-plugin-system.md) carries over verbatim; only the artifact format changes. |

## Open questions

- **Component Model adoption.** When do we cut the v2 ADR for the
  WASI 0.2 host worlds (`ork.agent`, `ork.gateway`)? Driver: the
  first credible community contributor asks to ship a gateway as
  WASM. Until then v1 (tools only) is enough.
- **Catalog signing.** Do we require Sigstore-style attestations
  for catalog entries, or is a plain ed25519 signature in the
  manifest sufficient day one? Trade-off recorded; the current
  proposal is "ed25519 in v1, Sigstore is a follow-up".
- **Fuel pricing.** wasmops uses a single `fuel_limit` per call.
  ork may eventually want per-tenant fuel quotas to bound spend.
  Out of scope for this ADR; an artifact for the
  observability/quota ADR.
- **Plugin debugging.** Wasmtime supports DWARF source-level
  debugging via `wasmtime serve --debug`. We do not wire it up in
  v1; a `WASMTIME_BACKTRACE_DETAILS=1`-style operator switch is
  sufficient.
- **Per-tenant plugin gating.** The decision says "RBAC handles
  it"; the actual scope vocabulary
  ([`0021`](0021-rbac-scopes.md)) does not yet name a `plugin.*`
  scope. Coordinate with that ADR before implementation.
- **Hot reload.** Is the absence of a runtime "reload plugin" RPC
  a real operator pain, or is "restart `ork-server`" acceptable?
  Decide based on demo-stage feedback.
- **Adversarial review pass.** This ADR makes a long-term
  commitment to a wire format and a host module ABI. The
  adversarial review described in [`AGENTS.md`](../../AGENTS.md) §7
  is **recommended** before flipping to `Accepted` — the candidate
  attack surface is "what does the host module commit us to that
  we will regret in 12 months?".

## References

- wasmops source (out of repo): `~/wasmops/src/runtime/`,
  `~/wasmops/guest-kv/`, `~/wasmops/examples/`.
- Wasmtime book: <https://docs.wasmtime.dev/>
- Wasmtime async + fuel guide:
  <https://docs.wasmtime.dev/examples-fuel.html>
- WASI Preview 1 spec:
  <https://github.com/WebAssembly/WASI/blob/main/legacy/preview1/docs.md>
- WASI Preview 2 / Component Model (deferred successor):
  <https://github.com/WebAssembly/component-model>
- Bytecode Alliance security considerations for embedding wasmtime:
  <https://docs.wasmtime.dev/security.html>
- ADR [`0014`](0014-plugin-system.md) — superseded predecessor.
- ADR [`0010`](0010-mcp-tool-plane.md) — out-of-process tool plane.
- ADR [`0013`](0013-generic-gateway-abstraction.md) — gateway port
  whose WASM equivalent is deferred.
- ADR [`0021`](0021-rbac-scopes.md) — RBAC scope vocabulary that
  will need a `plugin.*` namespace.
- ADR [`0023`](0023-migration-and-rollout-plan.md) — milestone M17
  is updated to reference this ADR instead of
  [`0014`](0014-plugin-system.md) (a one-line edit landed alongside
  this ADR's implementation PR).
