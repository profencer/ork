# 0058 — Per-tenant `OrkApp` and tenant-scoped catalog overlays

- **Status:** Proposed
- **Date:** 2026-05-09
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0010, 0012, 0013, 0017, 0020, 0021, 0048, 0049, 0050, 0051, 0052, 0053, 0056, 0057
- **Supersedes:** —

## Context

ADR [`0049`](0049-orkapp-central-registry.md) made `OrkApp` the
single Rust value that holds an ork deployment's shape (agents,
workflows, tools, MCP servers, memory, scorers, server config). The
auto-generated REST + SSE surface ([`0056`](0056-auto-generated-rest-and-sse-surface.md)),
Studio ([`0055`](0055-studio-local-dev-ui.md)), and the dev-loop
CLI ([`0057`](0057-ork-cli-dev-build-start.md)) all read off that
single value.

ADR [`0049`](0049-orkapp-central-registry.md)'s `Open questions`
section explicitly defers per-tenant `OrkApp`:

> **Per-tenant `OrkApp`.** ADR [`0020`](0020-tenant-security-and-trust.md)
> implies an application-per-tenant or a tenant-aware app. The
> current shape is single-tenant; multi-tenant scoping should be
> handled inside registered components (each agent has tenant-aware
> behaviour via `AgentContext`), not by spawning N apps. Confirm
> during 0020 implementation.

ADR [`0020`](0020-tenant-security-and-trust.md) is now Implemented
(Phases A–D) and shipped tenant isolation at the *data* layer (RLS,
per-tenant DEKs, mesh JWT propagation), and ADR
[`0021`](0021-rbac-scopes.md) shipped scope-based authorisation —
but neither answered the registry question. The result is the
posture documented in the [`0049`](0049-orkapp-central-registry.md)
note: every tenant sees the same catalog; tenant variation only
happens *inside* a component (e.g.
[`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs)'s
`TenantSettings` carries per-tenant LLM credentials, MCP creds, and
the `scope_allowlist` from
[`0021`](0021-rbac-scopes.md)). That is sufficient for **operator-deployed
catalogs** with uniform components; it is **not** sufficient for
self-serve SaaS where tenants need:

- a **different catalog** (tenant A gets agents `planner` and
  `researcher`; tenant B gets `support-triage` and
  `compliance-review`);
- a **different LLM routing posture** (tenant A pinned to
  `openai/gpt-4o`, tenant B to a self-hosted llama via
  [`0012`](0012-multi-llm-providers.md));
- **different MCP servers** wired in (tenant A's Jira, tenant B's
  Linear) without rebuilding the binary;
- **different memory/storage backends** ([`0053`](0053-memory-working-and-semantic.md))
  per-tenant for compliance reasons (e.g. EU-resident storage for
  some, US for others);
- **per-tenant scope allowlists** that ADR
  [`0021`](0021-rbac-scopes.md) defined as a `TenantSettings` field
  but did not yet enforce as a *catalog* filter at request time.

The current code path can express none of these as a config
artefact; they require a redeploy with hand-edited `main.rs`.

This ADR closes the gap by making the resolution
`tenant_id → Arc<OrkApp>` a **first-class port** that the REST
surface ([`0056`](0056-auto-generated-rest-and-sse-surface.md)) and
the CLI ([`0057`](0057-ork-cli-dev-build-start.md)) consult on every
request, with a default that preserves the single-tenant shape from
[`0049`](0049-orkapp-central-registry.md).

## Decision

ork **introduces `TenantAppResolver`**, the port that maps a
`TenantId` to the `Arc<OrkApp>` that should serve that tenant's
requests. The auto-generated REST + SSE surface and the
programmatic `OrkApp::run_*` helpers consult the resolver before
they dispatch.

The change is **purely additive**:

- The existing
  [`OrkApp::serve()`](../../crates/ork-app/src/lib.rs) entry
  point keeps working — it is shorthand for
  `serve_with_resolver(SingleTenantResolver(self.clone()))`.
- The hexagonal contract from [`0049`](0049-orkapp-central-registry.md)
  is preserved — `crates/ork-app/` still depends only on
  `ork-core` + `ork-common`. File-IO and SQL adapters for
  tenant-config sources live in a new sibling crate
  `crates/ork-tenants/`.
- The wire shape is unchanged: A2A and REST URLs are the same;
  what changes is *which app* serves a given request.
- Hard isolation between tenants in the same process remains the
  domain of RLS ([`0020`](0020-tenant-security-and-trust.md)) and
  scopes ([`0021`](0021-rbac-scopes.md)). This ADR does **not**
  introduce a Rust-level sandbox.

### `TenantAppResolver` port

New trait in `crates/ork-app/src/multi_tenant.rs`:

```rust
use std::sync::Arc;
use ork_common::{OrkError, TenantId};
use crate::OrkApp;

/// Resolves the `OrkApp` view to use for a given tenant. The same
/// `Arc<OrkApp>` may be returned for many tenants (shared catalog)
/// or a distinct value per tenant (per-tenant overlay).
#[async_trait::async_trait]
pub trait TenantAppResolver: Send + Sync {
    /// Returns the app view for `tenant_id`. Implementations are
    /// allowed to build lazily and cache.
    async fn resolve(&self, tenant_id: &TenantId)
        -> Result<Arc<OrkApp>, OrkError>;

    /// Tenants known to this resolver. May be empty for resolvers
    /// that mint apps lazily.
    async fn known_tenants(&self) -> Vec<TenantId> { Vec::new() }

    /// Drop any cached app for `tenant_id`. The next `resolve`
    /// rebuilds it. Used by the admin reload endpoint.
    async fn invalidate(&self, _tenant_id: &TenantId)
        -> Result<(), OrkError> { Ok(()) }
}
```

Three concrete resolvers ship in `crates/ork-app/`:

| Resolver | Purpose | Where the app comes from |
| -------- | ------- | ------------------------ |
| `SingleTenantResolver` | Back-compat with [`0049`](0049-orkapp-central-registry.md). One `Arc<OrkApp>` for every tenant. | The user's `main.rs` builds one app and wraps it. |
| `StaticTenantResolver` | "ISV with N customer apps known at build time" — each customer has a hand-coded `OrkApp`. | A `HashMap<TenantId, Arc<OrkApp>>` populated in `main.rs`. |
| `CatalogTenantResolver` | Self-serve SaaS. A catalog of component **factories** + a `TenantConfigSource`. Apps are built lazily, cached, evicted on config change. | `AppCatalog` (factories) + `dyn TenantConfigSource` (loads per-tenant overlay). |

### `OrkApp::serve_with_resolver`

`crates/ork-app/src/lib.rs` grows one method:

```rust
impl OrkApp {
    /// Start the auto-generated REST/SSE server (ADR 0056) using a
    /// resolver to pick the per-request app. The existing
    /// `OrkApp::serve()` is now shorthand for
    /// `serve_with_resolver(Arc::new(SingleTenantResolver::new(self.clone())))`.
    pub async fn serve_with_resolver(
        resolver: Arc<dyn TenantAppResolver>,
        cfg: ServerConfig,
    ) -> Result<ServeHandle, OrkError>;
}
```

The single-tenant `serve()` from [`0049`](0049-orkapp-central-registry.md)
keeps its current signature; under the hood it calls
`serve_with_resolver` with a `SingleTenantResolver`.

`OrkApp::run_agent` and `OrkApp::run_workflow` from
[`0049`](0049-orkapp-central-registry.md) take an explicit
`AgentContext` already; the new equivalents accept a
`TenantAppResolver` and resolve internally:

```rust
impl OrkApp {
    pub async fn run_agent_for_tenant(
        resolver: &dyn TenantAppResolver,
        tenant_id: &TenantId,
        agent_id: &str,
        ctx: AgentContext,
        prompt: ChatMessage,
    ) -> Result<AgentEventStream, OrkError>;

    pub async fn run_workflow_for_tenant(
        resolver: &dyn TenantAppResolver,
        tenant_id: &TenantId,
        workflow_id: &str,
        ctx: AgentContext,
        input: serde_json::Value,
    ) -> Result<WorkflowRunHandle, OrkError>;
}
```

### `CatalogTenantResolver` and the factory model

```rust
// crates/ork-app/src/multi_tenant.rs

pub struct CatalogTenantResolver {
    catalog: AppCatalog,
    config_source: Arc<dyn TenantConfigSource>,
    cache: tokio::sync::Mutex<lru::LruCache<TenantId, Arc<OrkApp>>>,
    cache_capacity: usize,
}

pub struct AppCatalog {
    pub agents:    HashMap<String, Arc<dyn AgentFactory>>,
    pub workflows: HashMap<String, Arc<dyn WorkflowFactory>>,
    pub tools:     HashMap<String, Arc<dyn ToolFactory>>,
    pub memory:    Option<Arc<dyn MemoryFactory>>,
    pub vectors:   Option<Arc<dyn VectorStoreFactory>>,
    pub mcp:       HashMap<String, Arc<dyn McpServerFactory>>,
    pub scorers:   Vec<ScorerSpec>,
    pub server:    ServerConfig,
}

#[async_trait::async_trait]
pub trait AgentFactory: Send + Sync {
    async fn build(&self, ctx: &TenantBuildContext<'_>)
        -> Result<Arc<dyn Agent>, OrkError>;
}
// (WorkflowFactory, ToolFactory, MemoryFactory, VectorStoreFactory,
//  McpServerFactory all share the same shape.)

pub struct TenantBuildContext<'a> {
    pub tenant_id: &'a TenantId,
    pub config:    &'a TenantAppConfig,
    pub secrets:   &'a dyn SecretResolver,   // tenant DEK-backed (ADR 0020)
    pub llm:       &'a dyn LlmRouterPort,    // ADR 0012
}
```

### `TenantAppConfig` — the per-tenant overlay

```rust
// crates/ork-tenants/src/config.rs (schema lives in ork-tenants because
// the Postgres adapter does too — ork-app stays infra-clean)

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "v")]
pub enum TenantAppConfig {
    /// Versioned from day one. Future variants live alongside `V1`;
    /// the resolver rejects unknown variants with a Configuration error.
    V1(TenantAppConfigV1),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TenantAppConfigV1 {
    /// Subset of catalog ids visible to this tenant. `None` = full catalog.
    pub agents:    Option<Vec<String>>,
    pub workflows: Option<Vec<String>>,
    pub tools:     Option<Vec<String>>,
    pub mcp:       Option<Vec<String>>,

    /// Per-component config (free-form, validated by each factory).
    pub component_settings: serde_json::Map<String, serde_json::Value>,

    /// Routing override (which provider/model to prefer); ADR 0012.
    pub llm: Option<LlmRoutingOverride>,

    /// Memory backend choice (ADR 0053). `None` = catalog default.
    pub memory: Option<MemoryConfig>,

    /// Scope allowlist; defence-in-depth filter on JWT scopes.
    /// Tied back to ADR 0021's `TenantSettings.scope_allowlist`.
    pub scope_allowlist: Option<Vec<String>>,
}
```

### `TenantConfigSource` and change propagation

```rust
// crates/ork-tenants/src/lib.rs

#[async_trait::async_trait]
pub trait TenantConfigSource: Send + Sync {
    async fn load(&self, tenant_id: &TenantId)
        -> Result<TenantAppConfig, OrkError>;

    /// Stream of "the config for this tenant changed" events. The
    /// resolver subscribes once at boot and invalidates its cache
    /// per event. Sources that cannot stream return an empty stream
    /// and rely on the admin reload endpoint.
    fn change_stream(&self)
        -> futures::stream::BoxStream<'static, TenantConfigChange>;
}

pub struct TenantConfigChange {
    pub tenant_id: TenantId,
    pub kind: TenantConfigChangeKind,   // Updated | Removed
}
```

Adapters in `crates/ork-tenants/`:

- `FileTenantConfigSource` — reads `tenant_configs/<tenant_id>.toml`,
  uses `notify` for change events. Useful for small operator
  fleets and dev.
- `PostgresTenantConfigSource` — reads
  `tenants.app_config` JSONB column (new in this ADR's migration);
  change stream backed by Postgres `LISTEN`/`NOTIFY` on
  `tenant_app_config_changed`. RLS-respecting via the same
  `app.current_tenant_id` GUC contract from
  [`0020`](0020-tenant-security-and-trust.md).

### REST surface integration ([`0056`](0056-auto-generated-rest-and-sse-surface.md))

The router in
[`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs)
gains an Axum `Extension<Arc<dyn TenantAppResolver>>`. Each handler
that today receives `&OrkApp` resolves it from the request's
`RequestCtx.tenant_id` (already populated by ADR
[`0020`](0020-tenant-security-and-trust.md)'s `auth_middleware`):

```rust
async fn resolve_app(
    State(state): State<AppState>,
    ctx: &RequestCtx,
) -> Result<Arc<OrkApp>, OrkError> {
    state.tenant_resolver
        .resolve(&ctx.tenant_id)
        .await
}
```

Two new admin routes (gated by ADR
[`0021`](0021-rbac-scopes.md)'s `tenant:admin` scope — no new scope
is coined):

```
GET  /api/admin/tenants/:id/manifest        -> AppManifest for that tenant's view
POST /api/admin/tenants/:id/reload          -> Drop cache; force rebuild on next request
```

Existing routes unchanged in URL shape, but their *result* depends
on the caller's tenant:

- `GET /api/manifest` returns the **current tenant's** manifest,
  not a global catalog.
- `GET /api/agents`, `GET /api/workflows`, `GET /api/tools` list
  only what the resolved app exposes.
- Per-id routes (`/api/agents/:id/generate`, etc.) return
  `404 Not Found` (with audit event `audit.tenant_lookup_miss`)
  when the requested id is not in the tenant's view, even if it
  exists in another tenant's catalog.

### CLI integration ([`0057`](0057-ork-cli-dev-build-start.md))

Two additions to the CLI surface; no new top-level subcommand:

```
ork inspect --tenant <id> [--config <path>]
    Resolves the named tenant via the configured resolver and prints
    its AppManifest. With --config, applies the overlay locally
    (without DB) for dev.

ork lint
    Adds a `multi-tenant catalog` check: every catalog factory
    builds successfully against an empty TenantAppConfig (the
    default-overlay smoke test), and every example tenant config
    in `tenant_configs/` resolves to a valid manifest.
```

`ork dev` is unchanged in default behaviour (single-tenant); a
`--tenant <id>` flag picks an example config from
`tenant_configs/<id>.toml` for the running session.

### Storage and migrations

One Postgres migration adds the per-tenant config column. Existing
tenants get the default empty overlay, which means "see the full
catalog" — back-compat preserved.

```sql
-- migrations/013_tenant_app_config.sql
ALTER TABLE tenants
    ADD COLUMN app_config JSONB NOT NULL DEFAULT '{"v":"V1"}'::jsonb;

CREATE OR REPLACE FUNCTION notify_tenant_app_config_changed()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify(
        'tenant_app_config_changed',
        json_build_object('tenant_id', NEW.id)::text
    );
    RETURN NEW;
END $$;

CREATE TRIGGER tenants_app_config_notify
    AFTER UPDATE OF app_config ON tenants
    FOR EACH ROW EXECUTE FUNCTION notify_tenant_app_config_changed();
```

The `tenants` table is already RLS-disabled per
[`migrations/010_rls_policies.sql`](../../migrations/010_rls_policies.sql)
(see ADR [`0020`](0020-tenant-security-and-trust.md)'s Phase A
findings); admin tokens with `tenant:admin` are the only writers,
which the existing `routes/tenants.rs` enforces.

### Observability ([`0022`](0022-observability.md) successor)

Every resolution emits a `tracing` span with attributes
`tenant_id`, `cache_hit: bool`, `latency_ms`. Cache miss + factory
build emits a child span per built component so cold-start cost is
attributable.

Audit events:

- `audit.tenant_resolved` on successful resolution (info).
- `audit.tenant_lookup_miss` on tenant id not in config source (warn).
- `audit.tenant_catalog_filter` when an id is requested but absent
  from the tenant's overlay (warn).

## Acceptance criteria

- [ ] Trait `TenantAppResolver` defined at
      `crates/ork-app/src/multi_tenant.rs` with the signature shown
      in `Decision`. `crates/ork-app/Cargo.toml` adds no new infra
      dependencies (no `axum`, `sqlx`, `reqwest`, `rmcp`,
      `rskafka`); CI grep enforces this.
- [ ] `SingleTenantResolver`, `StaticTenantResolver`, and
      `CatalogTenantResolver` defined in the same module with
      `Arc<dyn TenantAppResolver>` factory constructors.
- [ ] `OrkApp::serve_with_resolver` and the existing
      `OrkApp::serve` (now shorthand) defined in
      `crates/ork-app/src/lib.rs`. The shorthand wraps
      `SingleTenantResolver` and is verified to be a no-op
      regression vs ADR [`0049`](0049-orkapp-central-registry.md).
- [ ] `OrkApp::run_agent_for_tenant` and
      `OrkApp::run_workflow_for_tenant` honour
      `AgentContext::cancel`, mirroring [`0049`](0049-orkapp-central-registry.md)'s
      `run_agent` / `run_workflow`.
- [ ] New crate `crates/ork-tenants/` exists with:
      `TenantAppConfig` (`V1` variant only), `TenantConfigSource`
      trait, `FileTenantConfigSource`, `PostgresTenantConfigSource`,
      and the `TenantConfigChange` event.
- [ ] `crates/ork-tenants/Cargo.toml` is the only crate that imports
      `notify` and `sqlx` for this feature; `crates/ork-app/`
      depends on `ork-tenants` only via the trait (no concrete
      adapter import).
- [ ] Migration `migrations/013_tenant_app_config.sql` adds the
      `app_config JSONB NOT NULL DEFAULT '{"v":"V1"}'` column and
      the `tenant_app_config_changed` `LISTEN`/`NOTIFY` trigger.
- [ ] [`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs)
      mounts the resolver as an Axum `Extension` and exposes a
      `resolve_app(state, ctx)` helper used by every route that
      previously read `&OrkApp`.
- [ ] New routes
      `GET /api/admin/tenants/:id/manifest` and
      `POST /api/admin/tenants/:id/reload` defined in
      [`crates/ork-api/src/routes/admin.rs`](../../crates/ork-api/src/routes/),
      gated by `require_scope!("tenant:admin")`.
- [ ] `GET /api/manifest`, `GET /api/agents`, `GET /api/workflows`,
      `GET /api/tools` filter to the resolved tenant's view.
      Per-id routes return `404 Not Found` for ids not in the
      tenant's overlay; the path emits an
      `audit.tenant_catalog_filter` event.
- [ ] `TenantAppConfigV1.scope_allowlist`, when set, is intersected
      with the JWT's `scopes` at request time, closing the deferred
      defence-in-depth from ADR [`0021`](0021-rbac-scopes.md)'s
      Reviewer findings (`TenantSettings.scope_allowlist defence-in-depth
      intersection in auth_middleware is not implemented`).
- [ ] `ork inspect --tenant <id>` and the `multi-tenant catalog`
      check in `ork lint` are wired in
      [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs).
- [ ] Integration test
      `crates/ork-app/tests/multi_tenant_smoke.rs` covers:
      (a) two tenants with disjoint agent overlays — each tenant's
      `/api/agents` lists only its own; (b) cross-tenant id leakage
      check (tenant B requesting tenant A's agent id returns 404
      with the audit event); (c) admin `reload` triggers a rebuild
      on next request; (d) cache eviction on
      `TenantConfigChangeKind::Updated`.
- [ ] Integration test
      `crates/ork-tenants/tests/postgres_source_smoke.rs` builds a
      `PostgresTenantConfigSource` against `testcontainers`,
      asserts `LISTEN`/`NOTIFY` round-trip, and asserts that
      `app_config` mutation via SQL produces a `TenantConfigChange`
      event.
- [ ] Integration test
      `crates/ork-app/tests/scope_allowlist_intersect.rs` asserts
      that a JWT scope outside the tenant's allowlist is dropped
      from `RequestCtx.scopes` before any route handler runs.
- [ ] No file under `crates/ork-app/` imports `axum`, `sqlx`,
      `reqwest`, `rmcp`, `rskafka`, or `notify` (CI grep).
- [ ] `OrkApp::serve()` (no resolver) regression test
      `crates/ork-app/tests/serve_smoke.rs::single_tenant_serve_compat`
      asserts back-compat — the same fixture from
      [`0049`](0049-orkapp-central-registry.md) still passes.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- **Self-serve SaaS becomes mechanically possible.** Onboarding
  a new tenant is "insert a row into `tenants.app_config`"; the
  next request rebuilds and caches the tenant's app. No redeploy,
  no `main.rs` edit, no recompile.
- **Per-tenant catalogs.** Tenants can see different agents,
  workflows, tools, MCP servers — the operator publishes a
  catalog of *factories*, each tenant chooses the subset.
- **Per-tenant component config.** LLM provider routing, memory
  backend, MCP credentials all flow through a single config object
  validated by each factory. The existing
  [`TenantSettings`](../../crates/ork-core/src/models/tenant.rs)
  fields (LLM creds, MCP creds, scope allowlist) become inputs to
  the factory model rather than per-call branches inside agents.
- **Closes a deferred ADR-0021 finding.** The
  `TenantSettings.scope_allowlist` intersection in
  `auth_middleware` was acknowledged-deferred in [`0021`](0021-rbac-scopes.md);
  this ADR makes it load-bearing.
- **Back-compat preserved.** `OrkApp::serve()` with no resolver
  keeps working. Existing single-tenant demos and tests are
  unchanged.
- **Hexagonal boundary preserved.** `crates/ork-app/` adds the
  trait and the in-memory resolvers. The Postgres / file adapters
  live in `crates/ork-tenants/`, sibling to `crates/ork-persistence/`.

### Negative / costs

- **N tenants share a process; not a sandbox.** A bug in a shared
  tool is still a bug for every tenant that has it in their
  catalog. Operators running regulated tenants must deploy them
  process-isolated by routing — same `OrkApp` shape, N-up behind
  Kong. The deployment-tier isolation knob is preserved but not
  automated by this ADR.
- **Cold-start tail latency.** First request for a tenant builds
  the app (factories run, MCP servers register, agents wire up).
  Latency is bounded by factory cost; for a typical agent + 2
  tools + 1 MCP server + memory backend, expect ~50–200ms.
  Mitigations: (a) bounded LRU keeps warm tenants fast,
  (b) `CatalogTenantResolver::warm(&[TenantId])` for explicit
  pre-warm, (c) per-resolution span exposes the build cost.
- **Cache eviction is a correctness boundary.** If the change
  stream loses an event, tenants see stale catalog. Mitigations:
  (a) `POST /api/admin/tenants/:id/reload` is the manual escape
  hatch, (b) optional periodic cache-flush schedule for paranoia,
  (c) Postgres `LISTEN`/`NOTIFY` is at-least-once on a healthy
  connection — operators monitor connection health (ADR
  [`0022`](0022-observability.md) successor).
- **More config surface area.** `TenantAppConfig` is a new schema
  to version, validate, and document. Mitigation: `#[serde(tag = "v")]`
  enum versioning from day one; unknown variants fail the resolver
  loud.
- **Factory contract is new.** Every catalog component author
  writes a `*Factory` impl in addition to the component itself.
  Mitigation: `from_fn`-style helpers for trivial factories
  (`AgentFactory::from_fn(|ctx| async move { ... })`); the common
  shapes from ADR [`0052`](0052-code-first-agent-dsl.md)'s
  `CodeAgent` and ADR [`0051`](0051-code-first-tool-dsl.md)'s tool
  builder ship `IntoFactory` blanket impls.
- **The "ServeHandle commits to graceful shutdown" cost from
  ADR [`0049`](0049-orkapp-central-registry.md) extends to per-tenant
  apps.** Shutdown must drain *every* tenant app's in-flight tasks;
  the handle aggregates per-tenant drain futures. Bounded by
  `ServerConfig.shutdown_timeout`.

### Neutral / follow-ups

- **Tenant-supplied code (Rust, WASM, Lua) is explicitly
  out of scope.** Tenants choose subsets of operator-supplied
  factories and supply config; they do *not* upload code. ADR
  [`0024`](0024-wasm-plugin-system.md) was superseded by
  [`0048`](0048-pivot-to-code-first-rig-platform.md) and is not
  resurrected here. A separate ADR may revisit if the SaaS shape
  needs it.
- **Per-tenant Postgres schema / Kafka topic prefixes** remain
  open questions on ADRs [`0004`](0004-hybrid-kong-kafka-transport.md)
  and [`0020`](0020-tenant-security-and-trust.md). This ADR
  layers on top of those — RLS + scopes + per-tenant overlay
  give logical isolation today; physical isolation is a future
  ADR.
- **Studio multi-tenant view** (admin: pick a tenant, see its
  manifest and traces) is a follow-up on ADR
  [`0055`](0055-studio-local-dev-ui.md). The
  `/api/admin/tenants/:id/manifest` endpoint is the wire shape
  Studio will consume; the UI is out of scope here.
- **`LlmRouter` per tenant.** Today the router is shared (one
  process, one router); this ADR adds per-tenant
  `LlmRoutingOverride` that picks model + credentials from the
  shared router. A future ADR may split the router itself per
  tenant if quota / billing isolation demands it.
- **Per-tenant rate limits** stay at the Kong layer, configured
  via DevPortal. This ADR does not introduce in-process per-tenant
  rate limits.
- **Hot reload at the resolver level** is the natural place for
  ADR [`0057`](0057-ork-cli-dev-build-start.md)'s `ork dev` to
  hook tenant-config changes during development.

## Alternatives considered

- **Tenant-aware components only (the [`0049`](0049-orkapp-central-registry.md)
  default).** Rejected. Forces every tenant to share the same
  agent/workflow/tool catalog; tenants only differ via
  `TenantSettings`. Sufficient for "one product, many tenants",
  insufficient for "different products to different tenants".
  This ADR keeps that shape as the back-compat default
  (`SingleTenantResolver`) but makes the catalog-overlay model
  the recommended SaaS shape.
- **Process-per-tenant orchestration as the default.** Rejected as
  default. Best isolation, but ops cost and idle-tenant memory
  footprint dominate at scale (10³+ tenants, sparse access).
  Preserved as a deployment knob: an operator can run N
  `SingleTenantResolver` processes, one per tenant, behind a
  tenant-aware reverse proxy.
- **Code-only multi-tenancy (compile-time
  `let acme = build_acme_app()` per customer).** Rejected as
  default; preserved as `StaticTenantResolver` for ISVs whose
  customer count is small and known at build time. Onboarding a
  new customer should not require a redeploy.
- **Database row per tenant containing a full app definition (no
  catalog/overlay split).** Rejected. Duplicates the catalog
  across rows; bloats config; makes "ship a new agent to every
  tenant" an N-row migration. The catalog-of-factories +
  overlay-per-tenant model keeps the catalog DRY and the overlay
  small.
- **Type-erased `dyn Any` registry per tenant.** Rejected; the
  same `Arc<dyn Agent>` / `Arc<dyn ToolDef>` shape from ADR
  [`0049`](0049-orkapp-central-registry.md) works for per-tenant
  apps too. No need for a parallel typed surface.
- **One global `OrkApp` with a `tenant_id`-keyed routing layer
  inside each component (no resolver port).** Rejected. Pushes
  tenant-awareness into every component author's code; the
  resolver port localises it to one place and keeps the
  component contract single-tenant. Components that genuinely
  need cross-tenant logic (admin tools) can opt in via
  `AgentContext`.

## Affected ork modules

- [`crates/ork-app/`](../../crates/ork-app/) — new module
  `multi_tenant.rs` (trait + three resolvers); `lib.rs` gets
  `serve_with_resolver`, `run_agent_for_tenant`,
  `run_workflow_for_tenant`. No new infra dependencies.
- New: `crates/ork-tenants/` — `TenantAppConfig` schema,
  `TenantConfigSource` trait, `FileTenantConfigSource`,
  `PostgresTenantConfigSource`. This is the seam where
  file-watching and SQL adapters cross the hexagonal boundary,
  parallel to [`crates/ork-persistence/`](../../crates/ork-persistence/).
- [`crates/ork-api/src/routes/`](../../crates/ork-api/src/routes/) —
  per-handler `resolve_app` helper; new
  [`admin.rs`](../../crates/ork-api/src/routes/) module for
  `/api/admin/tenants/:id/{manifest,reload}`; `/api/manifest`
  and the catalog list routes filter by tenant view.
- [`crates/ork-api/src/middleware.rs`](../../crates/ork-api/src/middleware.rs) —
  `auth_middleware` intersects `RequestCtx.scopes` with the
  tenant's `scope_allowlist` after the resolver attaches its
  view (closes the deferred ADR
  [`0021`](0021-rbac-scopes.md) finding).
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) —
  `ork inspect --tenant <id>`; `ork lint` catalog check;
  `ork dev --tenant <id>`.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs) —
  `TenantSettings.app_config: TenantAppConfig` field added
  (existing per-tenant fields unchanged).
- New: `migrations/013_tenant_app_config.sql` — `app_config`
  column + `tenant_app_config_changed` trigger.
- [`config/default.toml`](../../config/default.toml) — new
  `[tenants]` section: `resolver = "single" | "static" | "catalog"`,
  `[tenants.catalog] config_source = "file" | "postgres"`,
  `cache.capacity`, `cache.ttl_secs`.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra Cloud | per-project deploys; one TS project per "app" | `StaticTenantResolver` (compile-time N apps) |
| Temporal | namespaces (per-namespace config inside one cluster) | `CatalogTenantResolver` with per-tenant overlay |
| Kong | per-route plugin overlays on a shared service | `TenantAppConfigV1.component_settings` |
| Auth0 | tenant = isolated config + shared runtime | the overall shape of this ADR |
| Solace Agent Mesh | implicit single-mesh, per-tenant config inside SAC | replaced by an explicit resolver port |

## Open questions

- **Cache eviction policy beyond LRU.** Time-based TTL? Per-tenant
  memory cap? Default in this ADR: bounded LRU + manual reload +
  config-change events. Time-based TTL is a config knob
  (`cache.ttl_secs`) that defaults to "no TTL". Revisit after
  measuring real cold-start cost.
- **Studio admin view.** Should Studio render a tenant picker for
  operators? Default: out of scope here; the
  `/api/admin/tenants/:id/manifest` endpoint is sufficient for a
  follow-up Studio ADR.
- **A2A federation across tenants in one binary.** When tenant A's
  agent delegates to "tenant B's agent" via the same process
  (instead of a remote A2A hop), do we route through the resolver
  or short-circuit? Default: route through the resolver; it adds
  one hashmap lookup but preserves auditability through the
  `tid_chain` mechanism from ADR
  [`0020`](0020-tenant-security-and-trust.md).
- **Per-tenant LLM router.** Today shared; per-tenant
  `LlmRoutingOverride` selects model + creds. Does that hold at
  10× scale, or do we need per-tenant routers for quota / billing
  isolation? Default: shared with overrides; revisit when a
  billing ADR lands.
- **Versioning of `TenantAppConfigV1`.** Already serde-tagged;
  bumping to `V2` is straightforward. Open: do we maintain
  forward-compat parsers for old `V1` configs after a `V2` ships,
  or do we require migration on read? Default: maintain `V1` for
  one major version; document the retire date in the bumping ADR.

## References

- ADR [`0049`](0049-orkapp-central-registry.md) — `OrkApp` central
  registry; this ADR layers on top.
- ADR [`0020`](0020-tenant-security-and-trust.md) — RLS + tenant
  JWT + per-tenant DEKs; secrets feed into `TenantBuildContext`.
- ADR [`0021`](0021-rbac-scopes.md) — scope vocabulary +
  `TenantSettings.scope_allowlist` (this ADR closes the deferred
  intersection finding).
- ADR [`0056`](0056-auto-generated-rest-and-sse-surface.md) — REST
  + SSE surface that consumes the resolver.
- ADR [`0057`](0057-ork-cli-dev-build-start.md) — CLI hooks for
  `ork inspect --tenant` and `ork lint`.
- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot
  framing; this ADR keeps the code-first shape.
- Mastra `Mastra` class:
  <https://mastra.ai/reference/core/mastra-class>
- Postgres `LISTEN`/`NOTIFY`:
  <https://www.postgresql.org/docs/current/sql-notify.html>
- Auth0 multi-tenant model:
  <https://auth0.com/docs/get-started/auth0-overview/create-tenants>
