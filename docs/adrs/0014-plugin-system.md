# 0014 — Plugin system

- **Status:** Superseded by [`0024`](0024-wasm-plugin-system.md)
- **Date:** 2026-04-24
- **Phase:** 3
- **Relates to:** 0002, 0010, 0013, 0015, 0016, 0023
- **Superseded by:** [`0024`](0024-wasm-plugin-system.md) — public extensibility moves to a WASM-based plugin model leveraging the wasmops runtime design; the in-tree adapter pattern below remains useful for ork-team-maintained crates but is no longer the recommended path for third-party plugins.

## Context

ork agents, tools, and gateways currently have to be added to the workspace `Cargo.toml`, compiled into the `ork-server` and `ork` binaries, and shipped together. This is fine while ork is small but blocks third-party extension and forces every Slack/Teams/JIRA-shaped integration to ship in-tree.

SAM solves this with `sam plugin add <component> --plugin <plugin-name>` ([`cli/commands/plugin_cmd/add_cmd.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/cli/commands/plugin_cmd/add_cmd.py)) backed by:

- A plugin metadata block in `pyproject.toml` declaring `[tool.<name>.metadata] type = "agent" | "gateway" | "tool" | ...`.
- An "official catalog" pulled from a known GitHub repo ([`cli/commands/plugin_cmd/official_registry.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/cli/commands/plugin_cmd/official_registry.py)).
- Templates that get materialised into the user's project on `add`.

ork is Rust, not Python — `pip install` is not an option. We need a Rust-friendly plugin model that:

- Doesn't compromise type safety or memory safety;
- Allows third parties to ship binary plugins without forking ork;
- Stays reasonable to operate (no opaque ABI hell);
- Is small enough to land in a few PRs.

## Decision

ork **adopts a two-tier plugin model**:

- **Tier 1 — Source plugins (default).** Cargo crates in a side workspace that depend on ork's published crates and are compiled into a custom `ork-server` binary. The `ork plugin add` CLI manages this build.
- **Tier 2 — Out-of-process plugins (deferred but reserved).** Anything that wants to ship as a closed-source binary or live in a separate language. The mechanism is **MCP server** for tools (ADR [`0010`](0010-mcp-tool-plane.md)), **A2A server** for agents (ADR [`0007`](0007-remote-a2a-agent-client.md)), and **Kong-fronted HTTP** for gateways (ADR [`0013`](0013-generic-gateway-abstraction.md)). No new IPC.

This explicitly defers WASM-based dynamic loading — the rationale is documented under "Alternatives".

### Plugin manifest

A plugin is a Cargo crate with an extra `ork-plugin.toml` at its root:

```toml
[plugin]
name = "ork-gateway-slack"
version = "0.1.0"
type = "gateway"          # "agent" | "gateway" | "tool" | "embed" | "artifact_store" | "llm_provider"
ork_version = ">=0.5,<0.6"
description = "Slack gateway for ork — workspace-scoped chat ingress"
homepage = "https://github.com/example/ork-gateway-slack"
license = "Apache-2.0"

[register]
# Symbols ork-server invokes at boot to register the plugin.
factory = "ork_gateway_slack::register"

[config_schema]
# JSON Schema for the plugin's [plugins.<name>.config] block in config/default.toml
file = "ork-plugin.config.schema.json"
```

Each crate exports a single registration entry point:

```rust
// In the plugin crate
pub fn register(reg: &mut PluginRegistry, cfg: &serde_json::Value) -> Result<(), OrkError> {
    let parsed: SlackConfig = serde_json::from_value(cfg.clone())?;
    let adapter = Arc::new(SlackGatewayAdapter::new(parsed)?);
    reg.register_gateway_adapter("slack", adapter);
    Ok(())
}
```

`PluginRegistry` is a small handle exposed by `crates/ork-plugin-api` (a new crate that re-exports the public traits — `Agent`, `GenericGatewayAdapter`, `ToolExecutor`, `ArtifactStore`, `LlmProvider`, `EmbedHandler` — plus the `register_*` methods). Plugins depend on `ork-plugin-api` and only on it; they never depend on `ork-core` or `ork-api` directly. This shields plugins from internal churn.

### `ork plugin add`

The CLI ([`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs)) gains:

```
ork plugin add <crate>           # add to user's plugin workspace, regenerate ork-server crate
ork plugin remove <crate>
ork plugin list
ork plugin search <term>         # query the official catalog
ork plugin build                 # build the augmented ork-server
```

Mechanics:

1. The user's project layout (created by `ork init`) includes a `plugins/` directory containing a workspace member `plugins/local-server/` whose `Cargo.toml` lists ork core crates + each installed plugin crate as dependencies.
2. `plugins/local-server/src/main.rs` is **generated**: it imports each plugin's `register` function and calls it at boot.
3. `ork plugin add foo = "0.1"` writes a dependency line, regenerates `main.rs`, and runs `cargo build`.
4. The resulting `target/release/ork-server` is the deployed binary.

Plugins distributed via crates.io, git URLs, or local paths all work because the underlying mechanism is `cargo`.

### Official plugin catalog

ork hosts an `ork-plugin-catalog` GitHub repo. `ork plugin search` queries the GitHub API for crates listed in `catalog.json`. Each entry contains:

```json
{
  "name": "ork-gateway-slack",
  "type": "gateway",
  "homepage": "https://github.com/example/ork-gateway-slack",
  "summary": "Slack gateway for ork — workspace-scoped chat ingress",
  "ork_version": ">=0.5,<0.6"
}
```

Mirrors SAM's [official_registry](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/cli/commands/plugin_cmd/official_registry.py) pattern.

### Versioning and ABI

Plugins compile against a specific `ork-plugin-api` semver. Breaking changes to the public traits bump `ork-plugin-api` major version. Plugins are pinned in their `ork-plugin.toml` `ork_version` field; `ork plugin add` refuses to install a plugin whose `ork_version` is incompatible with the running ork.

Because everything is statically compiled, there is no runtime ABI; the cargo resolver enforces compatibility at build time.

### Plugin types and registration hooks

| Plugin type | Register method | Trait it provides | ADR |
| ----------- | --------------- | ----------------- | --- |
| `agent` | `reg.register_agent_factory(name, factory)` | `Agent` | [`0002`](0002-agent-port.md) |
| `gateway` | `reg.register_gateway_adapter(name, adapter)` | `GenericGatewayAdapter` | [`0013`](0013-generic-gateway-abstraction.md) |
| `tool` | `reg.register_tool_namespace(prefix, executor)` | `ToolExecutor` | [`0011`](0011-native-llm-tool-calling.md) |
| `embed` | `reg.register_embed_handler(type, handler)` | `EmbedHandler` | [`0015`](0015-dynamic-embeds.md) |
| `artifact_store` | `reg.register_artifact_store(scheme, store)` | `ArtifactStore` | [`0016`](0016-artifact-storage.md) |
| `llm_provider` | `reg.register_llm_provider(id, provider)` | `LlmProvider` | [`0012`](0012-multi-llm-providers.md) |

Multiple registrations from one plugin are allowed (a plugin may expose both a tool and a gateway).

### Configuration surface

A plugin's runtime config lives under `[plugins.<name>.config]` in [`config/default.toml`](../../config/default.toml). Validated against `ork-plugin.config.schema.json` at boot. Sensitive values may use the existing `_env` indirection convention from `[[remote_agents]]` (ADR [`0007`](0007-remote-a2a-agent-client.md)).

### Plugin sandboxing and trust

Tier-1 plugins run in-process — they have full access. We **explicitly do not** sandbox them; plugin trust is the operator's responsibility (analogous to picking which Cargo deps to use). Tier-2 (MCP/A2A out-of-process) is the path for untrusted code.

For plugins from the official catalog, we publish a signed checksum manifest. `ork plugin verify` checks that installed crate hashes match.

### `ork init` integration

`ork init` (added by ADR [`0023`](0023-migration-and-rollout.md)'s rollout work) scaffolds the `plugins/` directory and `local-server/` crate, mirroring `sam init`'s project layout.

## Consequences

### Positive

- Third parties can ship plugins without a fork.
- Type safety preserved — plugins are just Cargo crates with a registration entry point.
- Ops surface stays small — operators ship one binary that they built locally.
- Out-of-process extension (MCP, A2A) covers the closed-source / multi-language case without bespoke IPC.

### Negative / costs

- Building a custom `ork-server` is a real build step — not as instant as `pip install`. Mitigated by `ork plugin add` automating it.
- Plugin authors must track `ork-plugin-api` versions; major bumps invalidate compiled plugins.
- The catalog repo is operational debt to maintain.

### Neutral / follow-ups

- WASM-based plugins remain a possible future ADR if the build-step tax becomes painful or if we need true sandboxing.
- ADR [`0023`](0023-migration-and-rollout.md) sequences this after the core mesh is in place; plugins are not on the critical path for A2A.

## Alternatives considered

- **Dynamic libraries (`libloading`).** Rejected: Rust ABI is not stable across compiler versions; we'd ship an awful UX of "rebuild your plugin every time ork updates" without any of the type-safety upside.
- **WASM/Component Model plugins.** Tempting, but: (a) the WIT bindings to ork's full API would be massive, (b) async + traits in WASM are still rough, (c) it adds a runtime layer to debug. Deferred — explicit follow-up ADR if/when needed.
- **Python plugins via PyO3.** Rejected: drags Python into the ork process and recreates SAM's stack inside ork.
- **Out-of-process plugins via gRPC / protobuf.** Rejected for in-tree plugins: the MCP+A2A surfaces already cover the out-of-process case, with standard protocols.
- **Force everyone to extend via MCP/A2A only.** Rejected: there are legitimate in-process needs (custom `ArtifactStore` backends, custom embed handlers) where IPC overhead dominates.

## Affected ork modules

- New crate: `crates/ork-plugin-api/` — public re-exports + `PluginRegistry`.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) — `ork plugin` subcommands; templates for `local-server/` scaffolding.
- New repo: `ork-plugin-catalog` (maintained outside this repo).
- [`config/default.toml`](../../config/default.toml) — `[plugins.<name>.config]` convention.
- Templates: extend `workflow-templates/` with `templates/plugin-skeleton/` (a generated Cargo crate).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `sam plugin add` | [`cli/commands/plugin_cmd/add_cmd.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/cli/commands/plugin_cmd/add_cmd.py) | `ork plugin add` |
| Plugin metadata in `pyproject.toml` | SAM convention `[tool.<name>.metadata]` | `ork-plugin.toml` |
| Official catalog | [`cli/commands/plugin_cmd/official_registry.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/cli/commands/plugin_cmd/official_registry.py) | `ork-plugin-catalog` repo |
| Plugin types | "agent", "gateway", "tool", … | Same enum on `[plugin] type` |
| Project init scaffolding | `sam init`, [`config_portal/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/config_portal) | `ork init` (ADR [`0023`](0023-migration-and-rollout.md)); GUI portal deferred |

## Open questions

- Do we offer a hosted plugin registry (private crates) for enterprise plugins? Defer; out of scope for the open core.
- Should `ork plugin add` also register the plugin's gateway/agent in DevPortal automatically? Yes when DevPortal credentials are configured; the plugin manifest carries enough metadata.
- GUI portal analog of SAM's `config_portal/` for plugin/agent install — defer to a follow-up ADR; the CLI flow is sufficient day one.

## References

- SAM plugin commands: <https://github.com/SolaceLabs/solace-agent-mesh/tree/main/cli/commands/plugin_cmd>
- WASM Component Model (for the deferred option): <https://github.com/WebAssembly/component-model>
