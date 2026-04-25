# 0021 — RBAC scopes for agents, tools, artifacts

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0002, 0006, 0010, 0011, 0013, 0016, 0017, 0019, 0020

## Context

ADR [`0020`](0020-tenant-security-and-trust.md) put the trust *frame* in place — JWT shape, mTLS, RLS, KMS — but does not specify **what authorisation decisions** the system actually makes. Today there are essentially none: any authenticated tenant can call any workflow API, any agent can call any tool, any tool can read/write anything in its tenant scope. This is fine in single-team environments and unworkable in any multi-team or multi-org deployment.

SAM uses scope strings for fine-grained access decisions across agents, tools, and artifact paths (see SAM's `shared/auth/middleware.py` and the per-skill scope checks in agent code). We adopt the same pattern, with vocabulary tailored to ork's modules.

## Decision

ork **adopts a structured scope vocabulary** evaluated at three layers — middleware (HTTP), `Agent` boundary, and `ToolExecutor` boundary — using a small `ScopeChecker` shared across the codebase.

### Vocabulary

Scope strings follow the shape `<resource>:<id>:<action>` with optional wildcards (`*`). Each scope is documented and registered in `crates/ork-security/src/scopes.rs`:

| Scope shape | Example | Meaning | Checked at |
| ----------- | ------- | ------- | ---------- |
| `agent:<id>:invoke` | `agent:planner:invoke` | Send a message to the agent | `Agent::send` / `routes/a2a.rs` |
| `agent:<id>:delegate` | `agent:vendor.scanner:delegate` | Delegate to the agent (peer call or `delegate_to`) | `agent_call` tool, `delegate_to` step |
| `agent:<id>:cancel` | `agent:planner:cancel` | Cancel a running task on the agent | `tasks/cancel` |
| `agent:*:invoke` | wildcard | Any agent invocation | … |
| `tool:<name>:invoke` | `tool:agent_call:invoke` | Call a built-in or integration tool | `ToolExecutor::execute` |
| `tool:mcp:<server>.<name>:invoke` | `tool:mcp:atlassian.search_jira:invoke` | Call a specific MCP tool | `McpClient::execute` |
| `tool:mcp:<server>.*:invoke` | wildcard | Any tool from one MCP server | … |
| `artifact:<scope>:<action>` | `artifact:context-abc:read`, `artifact:tenant:write` | Artifact read / write / delete (`scope` = `tenant` or `context-<id>`) | `ArtifactStore::{get,put,delete}` (ADR [`0016`](0016-artifact-storage.md)) |
| `model:<provider>:<model>:invoke` | `model:openai:gpt-4o:invoke` | Use a specific LLM model | `LlmRouter::resolve` (ADR [`0012`](0012-multi-llm-providers.md)) |
| `gateway:<id>:invoke` | `gateway:slack-acme:invoke` | Source events from a gateway | `Gateway` middleware (ADR [`0013`](0013-generic-gateway-abstraction.md)) |
| `tenant:admin` | — | Tenant CRUD | `routes/tenants.rs` |
| `tenant:self` | — | Read/update own tenant | `routes/tenants.rs` |
| `schedule:read` / `schedule:write` | — | Schedule CRUD | `routes/schedules.rs` (ADR [`0019`](0019-scheduled-tasks.md)) |
| `webui:access` | — | Use the Web UI gateway | `crates/ork-webui` (ADR [`0017`](0017-webui-chat-client.md)) |
| `ops:read` | — | Non-spec admin views (e.g. `GET /a2a/agents/{id}/tasks`) | `routes/a2a.rs` |

### Wildcards and hierarchy

Scopes are evaluated with simple glob matching:

- `agent:*:invoke` matches `agent:planner:invoke` and `agent:vendor.scanner:invoke`.
- `agent:planner:*` matches `agent:planner:invoke` and `agent:planner:cancel`.
- `*:*:*` is forbidden (would silently grant everything); `*` alone is reserved for the root admin scope `tenant:root` (operator-only).

Resource ids in scopes are case-sensitive and must match exactly (or via wildcard).

### Decision points

1. **HTTP layer** — `auth_middleware` (ADR [`0020`](0020-tenant-security-and-trust.md)) populates `RequestCtx.scopes`. Each route uses the `require_scope!("...")` macro:

   ```rust
   pub async fn cancel_task(...) -> impl IntoResponse {
       require_scope!(req, "agent:{agent_id}:cancel");
       ...
   }
   ```

2. **`Agent::send` boundary** — The registry wraps every `Agent::send` / `send_stream` invocation in a `ScopeChecker::require("agent:{id}:invoke")` call. `agent_call` tool wraps with `agent:{target}:delegate` instead. Cross-tenant delegation requires both `agent:{target}:delegate` *and* `tenant:cross_delegate` (a separate scope), so accidental tenant chains are blocked by default.

3. **`ToolExecutor::execute` boundary** — `CompositeToolExecutor` checks `tool:<name>:invoke` (or `tool:mcp:<server>.<name>:invoke` for MCP tools). The check is once per call; the result is cached per request via `RequestCtx.scope_cache`.

4. **`ArtifactStore` boundary** — `crates/ork-storage`'s wrapper checks `artifact:<scope>:<action>`. The wrapper is mandatory; raw access to a backend bypasses the check (developer footgun documented prominently).

5. **`LlmRouter::resolve` boundary** — Optional but recommended in production: `model:<provider>:<model>:invoke`. Disabled by default to keep dev easy; enable via `[security.enforce_model_scopes] = true`.

6. **Gateway boundary** — Each gateway (ADR [`0013`](0013-generic-gateway-abstraction.md)) checks `gateway:<id>:invoke` for the resolved principal before forwarding the request to an agent.

### `ScopeChecker`

```rust
// crates/ork-security/src/scopes.rs
pub struct ScopeChecker;

impl ScopeChecker {
    pub fn allows(scopes: &[String], required: &str) -> bool { /* glob match */ }
    pub fn require(scopes: &[String], required: &str) -> Result<(), OrkError> { ... }

    /// Pre-validate a scope string at config time; returns Err for malformed shapes.
    pub fn validate_format(scope: &str) -> Result<(), String>;
}
```

The checker has **no policy state of its own**. All policy is in the JWT scopes. This deliberately keeps ork-the-runtime out of the IAM business; DevPortal and the configured OAuth2 server own role → scope mapping.

### DevPortal integration

DevPortal exposes an OAuth2 client-credentials flow per agent card (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)). Each card's `securitySchemes` declares the relevant scopes; consumers request them through the standard OAuth2 dance. DevPortal's UI shows which roles have which scopes for human review.

For internal service-to-service calls inside the mesh, ork mints short-lived JWTs (ADR [`0020`](0020-tenant-security-and-trust.md)'s `A2aRemoteAgent` outbound flow) with the **intersection** of caller scopes and the destination card's accepted scopes. This prevents scope amplification on hops.

### Per-tenant scope policy

Tenants may **restrict** the union of scopes their tokens can request via `TenantSettings.scope_allowlist: Option<Vec<String>>`. If set, DevPortal honours it when minting tokens. ork-api re-checks at request time as defence-in-depth.

### Audit

Every denied scope check emits a `tracing` event `audit.scope_denied` with `scope`, `principal`, `tenant_id`, `tid_chain`, `request_id`. Every grant of a sensitive scope (any `tenant:admin`, `agent:*:delegate` to a different tenant) emits `audit.sensitive_grant`. Both go to the audit stream defined in ADR [`0022`](0022-observability.md).

### Defaults

| Token kind | Default scopes |
| ---------- | -------------- |
| End-user, no admin | `tenant:self`, `webui:access`, `agent:*:invoke`, `tool:*:invoke` (within tenant), `artifact:tenant:read`, `artifact:tenant:write`, `model:default:default:invoke` |
| Service / agent (mesh-internal) | `agent:<self>:invoke` plus what the calling card grants (intersected) |
| Operator / admin | `tenant:admin`, `ops:read`, `schedule:write` plus the above |
| External A2A partner | minted per-card by DevPortal; default minimal: `agent:<that-agent>:invoke` only |

These defaults are configurable; the table is the seed list in [`config/default.toml`](../../config/default.toml).

## Consequences

### Positive

- Multi-team and multi-org deployments get principle-of-least-privilege out of the box.
- Cross-tenant delegation is blocked by default; explicit `tenant:cross_delegate` opt-in is auditable.
- DevPortal becomes the IAM admin surface; ork stays the runtime.
- Plugin / MCP authors think about scopes from day one (their tool names appear in scope strings).

### Negative / costs

- More middleware on the hot path. Each scope check is sub-microsecond glob matching; we batch with a per-request cache.
- New scopes need to be coined as new features land; we centralise the registry to avoid drift.
- Token minting becomes more involved (intersect destination-accepted scopes); mitigated by helper functions in `crates/ork-security`.

### Neutral / follow-ups

- A future ADR may add **negative scopes** (`!tool:mcp:foo:invoke`) for explicit denies.
- A future ADR may add **resource-level RBAC** beyond tenant scoping (e.g. project-scoped artifact access).
- ABAC (attribute-based) is explicitly out of scope; scope strings carry attribute-shaped ids (e.g. `artifact:context-<id>`) where attributes matter.

## Alternatives considered

- **Casbin / OPA policy engines.** Rejected for now: heavy dependency to add, useful only when policies become complex enough to warrant a DSL; we revisit once we hit that.
- **Role names instead of scopes.** Rejected: roles encode policy in the runtime; scopes encode policy in the token issuer. Keeping ork policy-free aligns with the DevPortal IAM model.
- **Skip RBAC; rely on tenant isolation only.** Rejected: doesn't cover within-tenant separation (admin vs analyst, prod vs dev agents).
- **Custom DSL for scopes.** Rejected: glob matching is enough and trivially auditable.

## Affected ork modules

- New: `crates/ork-security/src/scopes.rs` — `ScopeChecker`, scope vocabulary, `require_scope!` macro.
- [`crates/ork-api/src/middleware.rs`](../../crates/ork-api/src/middleware.rs) — populate `RequestCtx.scopes`; add helper macros.
- [`crates/ork-api/src/routes/`](../../crates/ork-api/src/routes/) — every route adds `require_scope!`.
- [`crates/ork-agents/src/registry.rs`](../../crates/ork-agents/src/registry.rs) — wrap `Agent::send` with the checker.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs) — wrap `CompositeToolExecutor::execute` with the checker.
- New `crates/ork-storage/` (per ADR [`0016`](0016-artifact-storage.md)) wraps `ArtifactStore` with the checker.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs) — `TenantSettings.scope_allowlist`.
- [`config/default.toml`](../../config/default.toml) — default scope sets per token-kind seed.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Auth middleware | [`shared/auth/middleware.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/shared/auth/middleware.py) | `auth_middleware` + `require_scope!` |
| Per-skill scope check | implicit in SAM agent code | `agent:{id}:invoke` and similar |
| Tool scope | YAML `scopes:` per tool | `tool:<name>:invoke` |
| Cross-mesh delegation policy | implicit | `tenant:cross_delegate` + chain-aware checks |

## Open questions

- Do we want to ship a bundled DevPortal role template (e.g. "ork-analyst", "ork-admin")? Yes; published as JSON in `docs/operations/devportal-roles.json` (out-of-scope for this ADR).
- Per-environment scope dialing (prod stricter than staging)? Decision: yes via `[security.scopes.<env>]` overrides in config.
- Cancellation across tenants — should `agent:<id>:cancel` apply to a task started by a different tenant? Decision: only with `tenant:cross_delegate` already in the chain.

## References

- A2A `securitySchemes` in `AgentCard`: <https://github.com/google/a2a>
- OAuth 2 scope conventions: RFC 6749 §3.3
- SAM auth middleware: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/shared/auth/middleware.py>
