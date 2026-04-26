# 0017 — Web UI / chat client gateway

- **Status:** Accepted
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0008, 0013, 0015, 0016, 0019, 0021

## Context

ork has no end-user-facing UI today. Operators interact with workflows via REST (run definitions, kick off runs, view results) and via the CLI ([`crates/ork-cli/`](../../crates/ork-cli/)). There is no chat surface, no streaming visualisation, no artifact browser.

SAM ships a full Web UI gateway ([`gateway/http_sse/frontend/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway/http_sse) + [`client/webui/frontend/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/client/webui/frontend)) that drives the project. Reaching SAM parity at the user-experience level requires the equivalent: a chat UI that consumes the A2A SSE stream from ADR [`0008`](0008-a2a-server-endpoints.md), renders artifacts (ADR [`0016`](0016-artifact-storage.md)), resolves embeds (ADR [`0015`](0015-dynamic-embeds.md)), and shows scheduled tasks (ADR [`0019`](0019-scheduled-tasks.md)).

The Web UI is meaningfully larger and more stateful than other gateways (auth flow, file uploads, projects, multi-conversation history), which is why ADR [`0013`](0013-generic-gateway-abstraction.md) explicitly carves it out from the generic gateway abstraction.

## Decision

ork **introduces a Web UI gateway** as two new components:

- `crates/ork-webui/` — backend `axum` service that implements the `Gateway` trait (ADR [`0013`](0013-generic-gateway-abstraction.md)) and exposes a small set of UI-specific HTTP routes plus the SSE bridge.
- `client/webui/frontend/` — React + Typescript + Vite + Tailwind SPA that consumes the A2A endpoints from ADR [`0008`](0008-a2a-server-endpoints.md) and the UI-specific routes from `ork-webui`.

The directory layout matches SAM's split (`gateway/http_sse` + `client/webui/frontend`) so contributors moving between the two projects feel at home.

### Backend scope (`crates/ork-webui/`)

```rust
impl Gateway for WebUiGateway {
    fn id(&self) -> &GatewayId { &self.id }
    fn card(&self) -> &GatewayCard { &self.card }
    async fn start(&self, deps: GatewayDeps) -> Result<(), OrkError> {
        let app = Router::new()
            .merge(static_routes())                       // serve the SPA bundle
            .merge(api_routes(deps.clone()))              // /webui/api/*
            .layer(middleware::from_fn(webui_auth));
        // Bind on a configurable port; Kong usually fronts this.
        axum::serve(listener, app).await?;
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), OrkError> { /* graceful */ Ok(()) }
}
```

UI-specific routes (under `/webui/api/`):

| Route | Method | Purpose |
| ----- | ------ | ------- |
| `/projects` | GET / POST / DELETE | CRUD for "projects" (a UI concept; a project = pinned `context_id` + label set) |
| `/conversations` | GET / POST | Per-project conversation list (each conversation = one A2A `context_id`) |
| `/conversations/{id}/messages` | POST | Send a message — proxies to A2A `message/stream` (ADR [`0008`](0008-a2a-server-endpoints.md)) |
| `/uploads` | POST | Multipart upload → `ArtifactStore::put` (ADR [`0016`](0016-artifact-storage.md)); returns `Part::File { Uri }` |
| `/agents` | GET | Cached `AgentRegistry::list_cards()` (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)) for picker UI |
| `/scheduled` | (deferred) | Blocked on ADR [`0019`](0019-scheduled-tasks.md) — not part of the initial Web UI surface until that ADR is implemented. |
| `/me` | GET | Identity from JWT: `user_id` (`sub`), `tenant_id`, and `scopes` as decoded by [`auth_middleware`](../../crates/ork-api/src/middleware.rs). Fine-grained RBAC (ADR [`0021`](0021-rbac-scopes.md)) is not enforced per-route until 0021 lands. |

The Web UI **does not** invent new task semantics — every chat message becomes an A2A `message/send` against a chosen agent. The UI's "project" is a `(tenant_id, context_id, label)` triple stored in a small `webui_projects` table; the heavy data lives in `a2a_tasks` / `a2a_messages` already (ADR [`0008`](0008-a2a-server-endpoints.md)).

### Auth

**Phase 1 (this ADR’s implementation pass):** the SPA and `/webui/api/*` routes use the same **`Authorization: Bearer <ork JWT>`** model as the rest of `ork-api`. [`auth_middleware`](../../crates/ork-api/src/middleware.rs) decodes the JWT (`sub`, `tenant_id`, `scopes`, `exp`); the UI may collect the token via a local dev flow (e.g. paste token) and store it for API calls. Kong may still front the route; the Web UI does not add a second auth stack in-process.

**Deferred to a follow-up ADR:** full **OIDC authorization-code flow** (IdP redirect, `code` + PKCE, callback, session cookies, token refresh) and “exchange IdP token for ork JWT” are out of scope here; capture them when DevPortal / Kong integration is ready.

The web bundle is served from a Kong-fronted route in production. When OIDC + cookies are adopted later, **CSRF** is mitigated by SameSite cookies + state nonce; bearer-only Phase 1 does not require that layer.

### Frontend scope (`client/webui/frontend/`)

A Vite + React + TypeScript + Tailwind SPA. Major views:

| View | Description |
| ---- | ----------- |
| Chat | A2A `message/stream` consumer; renders text + tool calls + artifacts; supports cancel |
| Agent picker | Dropdown sourced from `/webui/api/agents` |
| Artifact browser | Per-conversation list with preview (PDF, image, JSON, markdown) |
| Project sidebar | List + create + delete projects |
| Scheduled tasks | **Deferred** until ADR [`0019`](0019-scheduled-tasks.md) exists in code; the UI table row is reserved. |
| Settings | Tenant + LLM provider info; API key rotation lives in admin CLI |

UI rendering of dynamic embeds (ADR [`0015`](0015-dynamic-embeds.md)) reuses the late-phase resolver server-side; the SPA receives already-resolved `Part`s and renders them by `kind`.

The frontend is a single bundle served via the backend's static-routes layer; CDN deployment is a deferred optimization.

### Build pipeline

- Frontend: `pnpm install && pnpm build` produces `client/webui/frontend/dist/`.
- Backend: `crates/ork-webui` includes `dist/` via `include_dir!` macro at compile time so the resulting binary is self-contained.
- Local dev: frontend on Vite dev server (`pnpm dev`), backend reverse-proxies to it when `WEBUI_DEV_PROXY=http://localhost:5173` is set.

### Out of scope for this ADR

- A drag-and-drop **workflow builder** (visual DAG editor) — defer to a future ADR; for now the UI is a chat client that can drive workflow agents.
- Full **admin** features (tenant CRUD, plugin install GUI) — CLI-driven; ADR [`0014`](0014-plugin-system.md) covers plugins.
- Per-message **edit/regenerate** UX — defer.
- **`/webui/api/scheduled`** and any **Scheduled tasks** UI — blocked on ADR [`0019`](0019-scheduled-tasks.md) (land server surface first).
- **Full OIDC / OAuth2 browser code-flow** in `ork-webui` — follow-up ADR; Phase 1 uses bearer JWT as today’s [`auth_middleware`](../../crates/ork-api/src/middleware.rs).

### Gateway card

The Web UI publishes a `GatewayCard` on `ork.a2a.v1.discovery.gatewaycards` with `extensions[].uri = "https://ork.dev/a2a/extensions/gateway-role/webui"` so DevPortal exposes it as the canonical chat surface.

### Registry plug-in (no `ork-gateways` → `ork-webui` dependency)

The `webui` gateway type is registered from [`crates/ork-api/src/gateways.rs`](../../crates/ork-api/src/gateways.rs) by calling **`GatewayRegistry::add_factory("webui", …)`** after `GatewayRegistry::with_builtins()`, so [`crates/ork-gateways/`](../../crates/ork-gateways/) does not depend on the `ork-webui` crate (hexagonal: API composition owns optional gateways).

## Acceptance criteria

- [x] `crates/ork-gateways/src/registry.rs` exposes `add_factory(name, factory)` and tests still pass.
- [x] `crates/ork-webui/` exists with `WebUiGateway: Gateway`, `WebUiGatewayFactory: GatewayFactory`, and `[[gateways]]` `type = "webui"` builds from TOML/JSON `config`.
- [x] `GET /webui/api/me` returns JSON `{ "user_id", "tenant_id", "scopes" }` from `AuthContext` and is only reachable with a valid JWT (same rules as `auth_middleware`).
- [x] `GET /webui/api/agents` returns `AgentRegistry::list_cards()` JSON with `Cache-Control: max-age=5`.
- [x] `GET`/`POST` `/webui/api/conversations` and `POST /webui/api/conversations/{id}/messages` stream or respond consistently with A2A `message/stream` behaviour (see implementation tests).
- [x] `migrations/008_webui_projects.sql` (or the chosen migration name after `007_artifacts.sql`) creates `webui_projects` and related sidecar if needed; Postgres repo implements project CRUD.
- [x] `GET`/`POST`/`DELETE` `/webui/api/projects` works per tenant; conversations filter by `project_id` where applicable.
- [x] `POST /webui/api/uploads` accepts multipart, writes via `ArtifactStore::put`, returns a `Part::File`-compatible `{ "uri" }` (or full part JSON) for the next message.
- [x] `client/webui/frontend/` builds with `pnpm build`; optional `embed-spa` feature bundles `dist/`; `WEBUI_DEV_PROXY` documented for `pnpm dev`.
- [x] `ork webui dev` in `ork-cli` runs Vite + API with dev proxy env as documented in `demo/README.md`.
- [x] `GatewayCard` for Web UI includes extension URI `https://ork.dev/a2a/extensions/gateway-role/webui` and discovery tests or smoke check passes.
- [x] `docs/adrs/README.md` index and `docs/adrs/metrics.csv` row updated when the ADR flips to Accepted.

## Reviewer findings

| Reviewer / date | Finding | Resolution |
| --------------- | ------- | ---------- |
| code-reviewer 2026-04-26 | `ork webui dev` did not load a `webui` gateway or set `a2a_public_base`, so `/webui/api/*` was missing. | `ORK_CONFIG_EXTRA` + `config/webui-dev.toml` merged in `ork-cli`; `ORK_A2A_PUBLIC_BASE` defaulted for the child `ork-server`. |
| code-reviewer 2026-04-26 | `WEBUI_DEV_PROXY` only passes HTTP GET; Vite HMR websockets are not proxied. | Documented in `demo/README.md` (use API origin for HTML; HMR may need the Vite port) — follow-up if we need full HMR through `ork-server`. |
| code-reviewer 2026-04-26 | Chat view used a hard-coded user message. | `App` now has a message textarea and sends `message.trim()` in JSON-RPC. |
| code-reviewer 2026-04-26 | Fixed 500ms sleep before `ork-server` (racey). | Replaced with TCP connect poll to the Vite port (≤ ~20s). |
| adversarial 2026-04-26 | Long-term: proxying all dev asset paths and WS upgrades is heavier than GET-only. | Kept GET passthrough; OIDC and scheduled UI remain in listed follow-up ADRs. |
| code-reviewer 2026-04-26 (post-accept) | `## Decision` sample shows a standalone `WebUiGateway` + `axum::serve`; implementation uses `NoopGateway` + `ork-api` route merge. | Doc drift only — behaviour matches ADR intent (same process, protected `/webui/api/*`). Update narrative in a follow-up doc edit or superseding note if it confuses readers. |
| code-reviewer 2026-04-26 (post-accept) | `ork-webui` has direct route smoke for `/me` only; conversations/messages/projects/uploads lack dedicated smokes. | Optional follow-up: add `*_smoke` tests per route cluster; not a contract breach. |

## Consequences

### Positive

- Real users can drive ork interactively without bespoke clients.
- The UI dogfoods the A2A endpoints (ADR [`0008`](0008-a2a-server-endpoints.md)) — every UI bug is also an A2A spec bug discoverable by `curl`.
- Artifact browser closes the loop on the artifact pipeline (ADR [`0016`](0016-artifact-storage.md)), making outputs discoverable.

### Negative / costs

- New frontend stack (React + Vite + Tailwind) adds tooling we didn't have. Mitigated by the SPA being built once into the backend binary; runtime dependencies stay Rust-only.
- The backend gateway is bigger than the generic gateways and needs more upkeep (auth flows, project model).
- `include_dir!` macro inflates the backend binary by ~5-10 MB depending on bundle size. Acceptable.

### Neutral / follow-ups

- A Slack / Teams gateway (ADR [`0013`](0013-generic-gateway-abstraction.md)) often replaces the Web UI for organisations that already live in chat platforms.
- `GET /scheduled` / `POST /scheduled` exposure assumes ADR [`0019`](0019-scheduled-tasks.md) is landed.
- A full workflow visual editor is a follow-up ADR.

## Alternatives considered

- **Pure SSR (Leptos / Yew).** Rejected: the available React ecosystem (markdown rendering, code highlighting, file viewers) outpaces Rust-WASM for our scope.
- **Single-binary HTMX UI.** Tempting for simplicity. Rejected: streaming chat with rich tool-call rendering and artifact previews benefits from a real reactive framework.
- **Reuse SAM's Web UI directly.** Rejected: tied to SAM's Python backend types and Solace transport assumptions; rewriting against ork is faster than retrofitting.
- **Defer until everything else is done.** Rejected: the Web UI is the first thing users see; skipping it makes ork hard to evaluate.

## Affected ork modules

- New crate: `crates/ork-webui/` — `Gateway` impl, axum routes, static/dev proxy (Phase 1: bearer JWT; OIDC follow-up).
- New: `client/webui/frontend/` — React SPA (separate `package.json`).
- New: `migrations/008_webui_projects.sql` (next after [`007_artifacts.sql`](../../migrations/007_artifacts.sql)) — `webui_projects` table (+ optional `webui_conversations` if used).
- [`crates/ork-gateways/src/registry.rs`](../../crates/ork-gateways/src/registry.rs) — `add_factory` for optional gateway types.
- [`crates/ork-api/src/gateways.rs`](../../crates/ork-api/src/gateways.rs) — register `webui` factory and merge any protected vs public router split for `/webui/api/*`.
- [`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs) — merge Web UI API routes with `auth_middleware` where required.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — same process as A2A; Web UI is not a second OS-level listen unless config chooses a second bind (implementation detail).
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs) — `AppState` passed into Web UI handlers.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) — `ork webui dev` helper (runs frontend + backend with hot reload).
- [`config/default.toml`](../../config/default.toml) and [`demo/config/default.toml`](../../demo/config/default.toml) — `[[gateways]]` `type = "webui"` and `[gateways.config]` (bind, `dev_proxy`, etc.).
- [`config/webui-dev.toml`](../../config/webui-dev.toml) — optional dev overlay; merged when `ORK_CONFIG_EXTRA` is set (used by `ork webui dev`).
- [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs) — `AppConfig::load` honours `ORK_CONFIG_EXTRA` (path to a `.toml` file).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| HTTP+SSE gateway | [`gateway/http_sse/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway/http_sse) | `crates/ork-webui` + A2A routes from ADR [`0008`](0008-a2a-server-endpoints.md) |
| Frontend SPA | [`client/webui/frontend/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/client/webui/frontend) | `client/webui/frontend/` (same path) |
| Project / conversation model | SAM `routers/sessions.py`, `tasks/` | `webui_projects` table + reuse `a2a_tasks` |
| Streaming consumer | SAM React SSE hook | Identical pattern in `client/webui/frontend/src/hooks/useA2aStream.ts` |
| Embed rendering | SAM gateway resolves embeds before send | Late-phase resolver wraps SSE (ADR [`0015`](0015-dynamic-embeds.md)) |

## Open questions

- Multi-tenant Web UI in one process vs per-tenant? Decision: one process; tenant inferred from OIDC token.
- Public vs internal hostname? Decision: behind Kong; supports both depending on Kong route.
- Real-time collaborative chat (multiple humans on one conversation)? Defer.

## References

- A2A SSE: <https://github.com/google/a2a>
- SAM frontend: <https://github.com/SolaceLabs/solace-agent-mesh/tree/main/client/webui/frontend>
- React + Vite + Tailwind setup: <https://vitejs.dev/>
