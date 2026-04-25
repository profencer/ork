# 0017 — Web UI / chat client gateway

- **Status:** Proposed
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
- `client/webui/frontend/` — React + Vite + Tailwind SPA that consumes the A2A endpoints from ADR [`0008`](0008-a2a-server-endpoints.md) and the UI-specific routes from `ork-webui`.

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
| `/scheduled` | GET / POST / PATCH / DELETE | CRUD for scheduled tasks (ADR [`0019`](0019-scheduled-tasks.md)) |
| `/me` | GET | Resolved identity + scopes (ADR [`0021`](0021-rbac-scopes.md)) |

The Web UI **does not** invent new task semantics — every chat message becomes an A2A `message/send` against a chosen agent. The UI's "project" is a `(tenant_id, context_id, label)` triple stored in a small `webui_projects` table; the heavy data lives in `a2a_tasks` / `a2a_messages` already (ADR [`0008`](0008-a2a-server-endpoints.md)).

### Auth

The frontend authenticates against an OIDC provider (default: DevPortal-issued OAuth2). The backend exchanges the OIDC token for an ork JWT via the existing [`auth_middleware`](../../crates/ork-api/src/middleware.rs) machinery (or proxies the token if Kong already validated it). The web bundle is served from a Kong-fronted route; CSRF is mitigated by SameSite=Lax cookies + state nonce.

### Frontend scope (`client/webui/frontend/`)

A Vite + React + TypeScript + Tailwind SPA. Major views:

| View | Description |
| ---- | ----------- |
| Chat | A2A `message/stream` consumer; renders text + tool calls + artifacts; supports cancel |
| Agent picker | Dropdown sourced from `/webui/api/agents` |
| Artifact browser | Per-conversation list with preview (PDF, image, JSON, markdown) |
| Project sidebar | List + create + delete projects |
| Scheduled tasks (read-only initially) | List + run-on-demand; PATCH/DELETE deferred |
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

### Gateway card

The Web UI publishes a `GatewayCard` on `ork.a2a.v1.discovery.gatewaycards` with `extensions[].uri = "https://ork.dev/a2a/extensions/gateway-role/webui"` so DevPortal exposes it as the canonical chat surface.

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

- New crate: `crates/ork-webui/` — `Gateway` impl, axum routes, OIDC handshake.
- New: `client/webui/frontend/` — React SPA (separate `package.json`).
- New: `migrations/005_webui_projects.sql` — `webui_projects` table.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — register the Web UI gateway via the gateway loader from ADR [`0013`](0013-generic-gateway-abstraction.md).
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs) — share `AppState` (engine, registries, stores) with `ork-webui`.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) — `ork webui dev` helper (runs frontend + backend with hot reload).
- [`config/default.toml`](../../config/default.toml) — `[webui]` block: bind, oidc, dev_proxy.

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
