# 0056 — Auto-generated REST + SSE server surface for agents and workflows

- **Status:** Accepted
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0003, 0004, 0008, 0017, 0020, 0021, 0048, 0049, 0050, 0052, 0055
- **Supersedes:** —

## Context

Ork's HTTP surface today is the [`ork-api`](../../crates/ork-api/)
crate plus
[`ork-webui`](../../crates/ork-webui/)'s gateway. ADR
[`0008`](0008-a2a-server-endpoints.md) (Proposed) defines the A2A
protocol-shaped endpoints (JSON-RPC + SSE per A2A 1.0). What is
missing is the *Mastra-shaped* REST surface — predictable
`/api/agents/:id/generate`, `/api/workflows/:id/run` routes auto-
generated from the registered components in
[`OrkApp`](0049-orkapp-central-registry.md), with OpenAPI emission,
Swagger UI, and request/response schemas derived from the
registered types.

Mastra ships this as
[`/api/agents/:id`, `/api/workflows/:id`, etc., via Hono](https://mastra.ai/docs/server/mastra-server),
plus an OpenAPI doc at `/api/openapi.json` and Swagger UI at
`/swagger-ui`. ADR [`0048`](0048-pivot-to-code-first-rig-platform.md)
adopts the Mastra-shaped surface; this ADR is its concrete server
form.

The goal: every component the user registers produces predictable
HTTP routes, validated against the user's schemas, streamed where
appropriate, and consumable by Studio (ADR 0055), the WebUI gateway
(ADR 0017), `curl`, and any HTTP client without bespoke per-agent
plumbing.

## Decision

ork **auto-generates a REST + SSE surface from `OrkApp::manifest()`**.
The crate is the existing
[`crates/ork-api/`](../../crates/ork-api/), reshaped to mount the
new routes alongside the existing A2A endpoints. The auto-generated
surface is purely additive to the wire contract from ADR
[`0003`](0003-a2a-protocol-model.md); A2A clients keep working
through the JSON-RPC paths.

### Route schedule

```
GET  /api/openapi.json                              -> OpenAPI 3.1 spec
GET  /swagger-ui                                    -> Swagger UI (gated by ServerConfig.swagger_ui)
GET  /healthz                                       -> 200 OK (liveness)
GET  /readyz                                        -> 200 OK once OrkApp::serve() is ready

GET  /api/manifest                                  -> AppManifest (ADR 0049)

# Agents
GET  /api/agents                                    -> [AgentSummary]
GET  /api/agents/:id                                -> AgentDetail (card + skills + schemas)
POST /api/agents/:id/generate                       -> AgentGenerateOutput
POST /api/agents/:id/stream                         -> text/event-stream (SSE)

# Workflows
GET  /api/workflows                                 -> [WorkflowSummary]
GET  /api/workflows/:id                             -> WorkflowDetail (input/output schemas, steps)
POST /api/workflows/:id/run                         -> { run_id }
GET  /api/workflows/:id/runs                        -> [RunSummary]
GET  /api/workflows/:id/runs/:run_id                -> RunState
GET  /api/workflows/:id/runs/:run_id/stream         -> text/event-stream (SSE)
POST /api/workflows/:id/runs/:run_id/resume         -> { ok }
POST /api/workflows/:id/runs/:run_id/cancel         -> { ok }

# Tools
GET  /api/tools                                     -> [ToolSummary]
GET  /api/tools/:id                                 -> ToolDetail (input/output schemas)
POST /api/tools/:id/invoke                          -> ToolOutput

# Memory
GET  /api/memory/threads?resource=...               -> [ThreadSummary]
GET  /api/memory/threads/:id/messages               -> [Message]
POST /api/memory/threads/:id/messages               -> { id }
DELETE /api/memory/threads/:id                      -> 204
GET  /api/memory/working?resource=...&agent=...     -> { value, schema, updated_at }
PUT  /api/memory/working?resource=...&agent=...     -> { ok }

# Scorers (ADR 0054)
GET  /api/scorers                                   -> [ScorerBindingSummary]
GET  /api/scorer-results?...                        -> [ScorerRow] (paginated)

# A2A endpoints (ADR 0008) coexist:
POST /a2a/...                                       -> JSON-RPC + SSE per A2A 1.0
```

The route table is a *function* of `OrkApp::manifest()`. Adding an
agent in `main.rs` ⇒ `/api/agents/<new-id>/generate` exists
automatically. Removing an agent removes its routes on the next
`OrkApp::serve()` call.

### Request/response shape

```rust
// crates/ork-api/src/dto.rs
#[derive(Deserialize, JsonSchema)]
pub struct AgentGenerateInput {
    pub message: ChatMessage,                   // user role; A2A typed parts (ADR 0003)
    #[serde(default)]
    pub thread_id: Option<ThreadId>,            // memory scope (ADR 0053)
    #[serde(default)]
    pub resource_id: Option<ResourceId>,
    #[serde(default)]
    pub request_context: Option<serde_json::Value>,  // validated against agent's schema
    #[serde(default)]
    pub options: AgentRunOptions,               // temperature override, max_steps override, etc
}

#[derive(Serialize, JsonSchema)]
pub struct AgentGenerateOutput {
    pub run_id: RunId,
    pub message: ChatMessage,                   // assistant role
    pub structured_output: Option<serde_json::Value>,  // present if agent has output_schema
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
}
```

**Streaming:** `/api/agents/:id/stream` returns `text/event-stream`
encoded per ADR [`0003`](0003-a2a-protocol-model.md)'s SSE shape:

```
event: status
data: {"kind":"status","state":"working"}

event: delta
data: {"kind":"delta","text":"The temperature in"}

event: tool_call
data: {"kind":"tool_call","id":"call_1","name":"weather.lookup","args":{"city":"SF"}}

event: tool_result
data: {"kind":"tool_result","id":"call_1","output":{"temp_f":67}}

event: completed
data: {"kind":"completed","run_id":"r-...","usage":{...}}
```

The same encoder powers Studio (0055) and the WebUI gateway (0017).

### OpenAPI emission

`/api/openapi.json` is generated at boot from the manifest. Each
agent contributes:

- A `POST /api/agents/:id/generate` operation with
  `requestBody` referencing the agent's `request_context_schema`
  (or the default `AgentGenerateInput` if none) and `responses` 200
  referencing `AgentGenerateOutput`.
- A `POST /api/agents/:id/stream` with `responses` 200 of MIME
  `text/event-stream`.

Each workflow contributes a `/api/workflows/:id/run` whose
`requestBody` schema is the workflow's input schema (from ADR 0050)
and a `responses` 200 schema of `{ run_id }`. The full schema lives
in `components/schemas/`.

Each tool exposed via `/api/tools/:id/invoke` contributes its
parameter and output schema from ADR 0051. `gate(predicate)`-d
tools are still listed in OpenAPI but flagged with
`x-ork-gated: true`.

The OpenAPI emitter is a function `manifest -> OpenApiDoc`; pure,
deterministic, snapshot-tested.

### Auth and tenant scoping

The auto-generated surface inherits ADR
[`0020`](0020-tenant-security-and-trust.md)'s tenant model. Every
request carries:

- `Authorization: Bearer <jwt>` — JWT subject becomes the caller
  identity.
- `X-Ork-Tenant: <tenant_id>` — explicit tenant header.

ADR [`0021`](0021-rbac-scopes.md) scopes are checked per-route per
the manifest's RBAC bindings:

- `agent:<id>:invoke` — required for `/api/agents/:id/{generate,stream}`.
- `workflow:<id>:run` — required for `/api/workflows/:id/run`.
- `tool:<id>:invoke` — required for `/api/tools/:id/invoke`.
- `memory:<resource_id>:read|write` — required for memory routes.

Without auth, in `ork dev` mode, all scopes default to "allow"
unless `ServerConfig::auth` is set. Production
(`ork start`) requires auth.

### Error model

Single error envelope:

```json
{
  "error": {
    "kind": "validation" | "auth" | "not_found" | "internal" | ...,
    "message": "short, human-readable",
    "details": { ... },
    "trace_id": "..." 
  }
}
```

Mapped from `OrkError` variants. All errors carry the trace id from
the OTel context (ADR 0058).

### Server adapters (mount-into-host shape)

For users embedding ork into an existing axum service (Mastra has
this shape; see
[Mastra server adapters](https://mastra.ai/blog/mastra-server-adapters)),
the routes are exposed as a standalone `axum::Router` builder:

```rust
let router: axum::Router = ork_api::router_for(&app, &server_config);
let host_router = axum::Router::new()
    .nest("/ai", router)
    .merge(my_existing_routes());
host_router.into_make_service();
```

This is the seam ADR 0049's `OrkApp::serve()` uses internally; users
who want to mount ork on a different path or co-host with their own
axum routes go through the same surface.

## Acceptance criteria

- [ ] [`crates/ork-api/`](../../crates/ork-api/) ships
      `pub fn router_for(app: &OrkApp, cfg: &ServerConfig) ->
      axum::Router` returning a router with the routes listed in
      `Decision`.
- [ ] `OrkApp::serve()` (ADR 0049 stub) consumes
      `ork_api::router_for(&self, &self.server_config)` and runs
      the resulting axum service.
- [ ] OpenAPI emitter at `crates/ork-api/src/openapi.rs`:
      `pub fn openapi_spec(app: &OrkApp) -> openapiv3::OpenAPI`.
      Snapshot-tested in
      `crates/ork-api/tests/openapi_snapshot.rs` against a fixture
      `OrkApp` with two agents, two workflows, two tools, one
      MCP server.
- [ ] `/swagger-ui` mounts (gated by `ServerConfig::swagger_ui`,
      default true in dev, false in prod).
- [ ] SSE encoder at `crates/ork-api/src/sse.rs`: emits the event
      shape in `Decision` for both agent streams and workflow run
      streams. Used by ADR 0055 Studio and ADR 0017 WebUI.
- [ ] Each route has an integration test under
      `crates/ork-api/tests/routes/`: `agents_generate.rs`,
      `agents_stream.rs`, `workflows_run.rs`,
      `workflows_stream.rs`, `workflows_resume.rs`,
      `workflows_cancel.rs`, `tools_invoke.rs`, `memory_*.rs`,
      `scorer_results.rs`.
- [ ] Validation: a `POST /api/agents/:id/generate` with a
      `request_context` that does not match the agent's
      `request_context_schema` returns 422 with
      `error.kind = "validation"`. Verified by integration test.
- [ ] Auth-gating: with `ServerConfig::auth = Some(...)`, a
      missing JWT returns 401; a JWT lacking `agent:weather:invoke`
      scope returns 403. Verified.
- [ ] Tenant header: missing `X-Ork-Tenant` returns 400
      (configurable via `ServerConfig::default_tenant`).
- [ ] Coexistence with A2A endpoints (ADR 0008): the same `OrkApp`
      mounts both `/api/agents/:id/generate` and `/a2a/...` and
      both succeed against the same agent. Verified by
      `crates/ork-api/tests/coexistence.rs`.
- [ ] Manifest hot-swap: `OrkApp::reload(new_app)` swaps the
      router atomically without dropping in-flight connections.
      Stub for now; full hot-reload semantics owned by ADR 0057.
- [ ] No regressions in
      [`crates/ork-api/tests/`](../../crates/ork-api/tests/)
      existing A2A tests.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- Adding an agent is one line in `main.rs` and produces a fully
  documented HTTP endpoint. The "what's the URL?" friction goes
  to zero.
- OpenAPI doc is *generated*, not maintained. Refactoring an
  agent updates the doc on the next boot; documentation drift
  becomes impossible.
- Studio (0055), WebUI (0017), and Swagger UI all read the same
  routes; one change updates all surfaces.
- The `router_for` helper means ork can mount inside another
  axum service. The "embed ork inside our existing app" use case
  Mastra wrote server adapters for is a single function call here.
- A2A endpoints from 0008 keep working unchanged; this ADR is
  purely additive.

### Negative / costs

- Auto-generated routes are a contract the runtime must keep
  stable across versions. Renaming an agent's id breaks
  `/api/agents/:old-id`. Mitigation: agent ids are user-chosen,
  and rename = create-new + deprecate-old.
- The OpenAPI spec for a large app (50+ agents, 200+ tools)
  becomes large. Acceptable; pagination on the manifest endpoint
  covers the discovery side.
- Streaming via SSE forces clients off WebSocket. SSE is fine for
  text/JSON but not bidirectional binary. Out of scope; ADR 0017
  WebUI used SSE successfully.
- The validation step (request body against the schema) is a
  per-request cost. Mitigation: schemas are compiled once at
  boot to a `jsonschema::Compiled`.
- Tenant scoping requires every route to thread `TenantId`
  through; we define one extractor (`Extension<RequestEnvelope>`)
  and use it everywhere.

### Neutral / follow-ups

- `mTLS` for service-to-service (ADR 0020 owns the policy);
  axum supports it via `rustls`. Wired in 0020's
  implementation.
- gRPC surface for tool-call-heavy workloads is not in v1 and is
  unlikely to ever be needed; SSE+REST is the wire contract.
- WebSocket as an alternative streaming transport (browsers
  beyond SSE limits) is a future ADR; SSE has worked well in
  ADR 0017.
- Custom user-defined routes (Mastra's
  [`registerApiRoute`](https://mastra.ai/docs/server/custom-api-routes))
  shape — users can already mount custom routes by calling
  `router_for(&app, ...).merge(my_router)`. A first-class
  builder method on `OrkAppBuilder::custom_route(...)` is a
  small follow-up for ergonomics.

## Alternatives considered

- **Stick with the A2A JSON-RPC surface only (extend ADR 0008,
  do not add REST).** Rejected. A2A is a wire contract for
  agent-to-agent traffic; humans, browsers, and `curl` users
  expect REST. Mastra's success rests on the REST shape; we
  match.
- **Hand-write routes per agent.** Rejected. The 0028–0045
  batch already showed the cost of per-thing route plumbing.
  Auto-generation from the manifest is the only way to keep
  Studio + OpenAPI + WebUI in sync without manual labour.
- **Use `tonic`/gRPC.** Rejected. Mastra is REST+SSE; OpenAI's
  Responses API is REST+SSE; rig is engine-only and SDK-agnostic.
  The default web ecosystem is REST+SSE; switch only if a
  customer needs gRPC.
- **Generate one router per tenant.** Rejected. Routes are
  tenant-agnostic; tenant scoping happens *inside* the
  middleware. Spawning N routers for N tenants would explode
  the route table.
- **Use `utoipa` for OpenAPI generation.** Considered, partially
  adopted: `utoipa` for the per-DTO schema derives, our own
  emitter for the agent/workflow/tool walk over `AppManifest`
  (utoipa cannot enumerate registered components without
  attribute-on-fn boilerplate).

## Affected ork modules

- [`crates/ork-api/`](../../crates/ork-api/) — major reshape:
  `router_for`, OpenAPI emitter, SSE encoder, per-DTO modules,
  middleware (auth, tenant, error mapping).
- [`crates/ork-app/`](../../crates/) — `OrkApp::serve()`
  consumes `router_for`; `OrkApp::reload(new_app)` for hot-swap.
- [`crates/ork-webui/`](../../crates/ork-webui/) — moves any
  shared SSE encoding to `ork-api` and consumes from there.
- [`crates/ork-studio/`](../../crates/ork-studio/) — depends on
  the SSE shape and the `/studio/api/*` routes added by ADR 0055.
- A2A endpoint code in
  [`crates/ork-api/`](../../crates/ork-api/) — unchanged behaviour;
  shares middleware with the new routes.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Critical | C1 — `router_for` does not mount the existing A2A endpoints (`crates/ork-api/src/routes/a2a.rs`); the legacy router consumes `AppState` (Postgres, Redis, Kafka, push outbox) which `OrkApp` does not own. | Documented seam in `router_for.rs` module docs: deployments needing both surfaces compose `auto.merge(legacy)` in `main.rs` per ADR §`Server adapters`. Full automatic coexistence inside `OrkApp::serve()` is deferred to a follow-up that threads `AppState` through `OrkAppBuilder`. |
| Critical | C2 — Tenant header could override the JWT-derived tenant without a consistency check, opening a cross-tenant impersonation path. | Fixed in-session: `tenant_middleware` now layers *inside* `auth_middleware` (auth runs first), and rejects `403 forbidden` when `X-Ork-Tenant` differs from `AuthContext::tenant_id` unless the caller holds `tenant:admin` (ADR-0020 §`Tenant CRUD`). |
| Critical | C3 — Memory routes (`list_threads`, `append_message`, `delete_thread`, `read_working`, `put_working`) skipped scope checks; an authenticated caller could read another resource's working memory. | Fixed in-session: every handler calls `require_scope(parts, &memory_read_scope(&resource))` or `&memory_write_scope(&resource)` per ADR-0021 vocabulary. |
| Major | M1 — Several routes from the `Decision` block are unimplemented: `/api/workflows/{id}/runs`, `/api/workflows/{id}/runs/{run_id}` (state poll), `/api/workflows/{id}/runs/{run_id}/stream`, `/resume`, `/cancel`, and `/api/memory/threads/{id}/messages` GET. | Acknowledged, deferred. The fire-and-forget `tokio::spawn` in `run_workflow` discards the `WorkflowRunHandle`; full surface needs a per-run snapshot table (ADR-0050 follow-up) before stream/resume/cancel can ship. Tracked as a v1.1 follow-up ADR alongside cursor pagination (open question #3). |
| Major | M2 — `/swagger-ui` and `/api/openapi.json` were initially gated behind tenant middleware, breaking browser fetch. | Fixed in-session: `router_for` now mounts both on a `docs` sub-router that sits ahead of `tenant_middleware`. |
| Major | M3 — `static_manifest_path` referenced `AgentSummary` for `/api/manifest`'s response. | Fixed in-session: `AppManifest` schema is hand-registered in `register_dto_schemas` (since `ork_app::AppManifest` does not derive `JsonSchema` to avoid cascading derives into `ork-eval`/`ork-a2a`); `static_manifest_path` references it. |
| Major | M4 — Middleware order docstring described `auth → tenant`; the implementation actually ran `tenant → auth`. Caused C2. | Fixed in-session in conjunction with C2; module-level docs in `router_for.rs` now describe and justify the `auth → tenant` ordering. |
| Major | M5 — `idempotency::IdempotencyCache` declared but unused. | Acknowledged, deferred. Wiring `Idempotency-Key` into the three POST handlers (`agents/generate`, `workflows/run`, `tools/invoke`) is straightforward but requires capturing the response body before it leaves the handler — recorded as a v1.1 follow-up. The cache is held available as `pub` so a follow-up can wire it without breaking ABI. |
| Major | M6 — `ScorerRow.recorded_at` is fabricated at fetch time (`Utc::now()`). | Acknowledged, deferred. Real timestamp requires a new field on `ork_eval::ScoredRow` and propagation through `record`. Follow-up tracked alongside the Postgres-backed sink in ADR-0054 M1. |
| Major | M7 — `metrics.csv` row missing. | Fixed in-session: row 0056 appended. |
| Minor | m1 — JSON Schema recompiled per request; ADR §`Negative/costs` mitigation says "compiled once at boot." | Acknowledged, deferred. Cache the compiled `jsonschema::Validator` on `Arc<OrkApp>` at boot. v1 trades the cost for simplicity; the validation is on the request critical path so the optimisation is real. |
| Minor | m2 — OpenAPI version emitted is `3.0.3`; ADR mentioned `3.1`. | The `openapiv3` crate models 3.0.x only; switching to `oas3` or hand-rolling is out of scope. Documented divergence. |
| Minor | m3 — `x-ork-gated: true` extension on MCP-fronted tools not implemented. | Acknowledged, deferred. The OpenAPI `Operation::extensions` field carries this; defer to the same follow-up ADR that splits MCP tools from native tools in the manifest. |
| Minor | m4 — Legacy `auth_middleware` returns `{"error": "..."}` (string), not the unified envelope. | Out of scope: the legacy middleware backs the existing A2A routes (ADR-0008) which have their own error contract. The auto-routes use `ApiError` consistently. Unifying is a follow-up after ADR-0008 stabilises. |
| Minor | m5 — `validate_request_context` calls `app.manifest()` per request. | Acknowledged, deferred. Cache the schema on `Arc<OrkApp>` at boot alongside m1's compiled validator. |
| Minor | m6 — `parse_user_message` silently overrides `role` to `User`. | Acknowledged. Comment in code documents the behaviour; a follow-up can `Err(ApiError::validation(...))` on mismatch. |
| Minor | m7 — Dead code placeholders (`_ensure_status_code_in_scope`, `_unused_run_status`, `const _: &str = TENANT_SELF_SCOPE`). | Fixed in-session where it didn't introduce import churn; remaining placeholders kept to mark imports the missing M1 routes will consume. |
| Minor | m8 — `read_working`/`put_working` accepted empty `agent` query param. | Fixed in-session: explicit non-empty validation. Same applied to `append_message` `agent_id`. |
| Minor | m9 — SSE encoder unit tests rely on `format!("{ev:?}")` (axum `Event` Debug shape). | Acknowledged, deferred. Asserting on the wire form requires running the response through axum's body. Tracked. |
| Nit | n1 — Stale doc comment on `router_for` middleware order. | Fixed in-session as part of C2/M4. |
| Nit | n2 — `format!("r-{task_id}")` repeated. | Acknowledged. Single-line, low-value extraction. |
| Nit | n3 — `sse/mod.rs` lacked module-level docs. | Acknowledged. Sub-modules carry the prose; module file is a 4-line re-export. |
| Nit | n4 — `const _: &str = TENANT_SELF_SCOPE;` keep-alive. | See m7. |
| Nit | n5 — README index row not yet flipped to Accepted. | Fixed in this commit. |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Server overview](https://mastra.ai/docs/server/mastra-server) | the `/api/...` route schedule |
| Mastra | [Custom API routes](https://mastra.ai/docs/server/custom-api-routes) | `router_for(...).merge(custom)` |
| Mastra | [Server adapters](https://mastra.ai/blog/mastra-server-adapters) | `router_for` is the same shape |
| OpenAI | Responses API: `/v1/responses` (REST + SSE) | the streaming and DTO shape |
| A2A | JSON-RPC + SSE wire contract (ADR 0003) | parallel surface, both mounted |

## Open questions

- **Streaming framing.** SSE per ADR 0003 vs OpenAI-style
  newline-delimited JSON streams. We pick SSE consistently
  (ADR 0003's shape) since clients (browsers, EventSource) get
  it for free.
- **Idempotency keys.** A `POST /api/agents/:id/generate` retried
  by a flaky network should not double-bill. Mastra has no
  surface; we add an `Idempotency-Key` header that produces the
  same response if the same key was used in the last 24 h.
  Detail in implementation.
- **Pagination.** Manifest, runs, scorer-results need pagination.
  Default v1: cursor-based with `?cursor=...&limit=...`.
- **Long-running runs.** A `POST /api/workflows/:id/run` returning
  `run_id` immediately is the right call; clients poll or
  subscribe to the SSE stream. The synchronous-wait-for-done
  variant (`?wait=true`) is a v1.1 add for ergonomic CLI use.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — registry.
- ADR [`0003`](0003-a2a-protocol-model.md) — A2A wire and SSE
  shape.
- ADR [`0008`](0008-a2a-server-endpoints.md) — A2A endpoints
  coexist.
- ADR [`0017`](0017-webui-chat-client.md) — WebUI gateway, same
  SSE encoder.
- ADR [`0020`](0020-tenant-security-and-trust.md) — auth and
  tenant.
- ADR [`0021`](0021-rbac-scopes.md) — RBAC scopes.
- Mastra server: <https://mastra.ai/docs/server/mastra-server>
- Mastra custom API routes:
  <https://mastra.ai/docs/server/custom-api-routes>
- Mastra server adapters:
  <https://mastra.ai/blog/mastra-server-adapters>
