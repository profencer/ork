# 0055 — Studio: local dev UI for chat, workflows, memory, traces, scorers

- **Status:** Proposed
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0017, 0048, 0049, 0050, 0052, 0053, 0054, 0056, 0057, 0058
- **Supersedes:** —

## Context

Mastra's Studio is the surface that closes the loop on "what is this
agent doing right now?" The
[CLI reference](https://mastra.ai/reference/cli/mastra) describes
two modes: `mastra dev` mounts Studio at `localhost:4111` next to
the auto-generated API; `mastra studio` runs Studio standalone
against a remote backend. Studio panels cover:

- Chat with each registered agent (streaming, tool-call inspection).
- Run a workflow with form-shaped inputs (derived from the
  `inputSchema`), watch the run stream live, replay past runs.
- Inspect memory: working memory contents, semantic-recall hits per
  thread, browse and delete threads.
- Trace explorer: span tree per run, tokens, costs, errors, with
  filters by agent / scorer / latency / cost.
- Scorer dashboard: live scores over time, per-scorer regressions,
  drill-into-failed-runs.
- Eval runner UI: pick a dataset, pick a target, run, see the
  report.

Ork has a Web UI gateway in
[`crates/ork-webui/`](../../crates/ork-webui/) (ADR
[`0017`](0017-webui-chat-client.md), Implemented). 0017 is the
*end-user* chat surface; Studio is the *developer dashboard*. They
read the same `OrkApp` (ADR
[`0049`](0049-orkapp-central-registry.md)) but serve different
audiences and live on different routes.

## Decision

ork **introduces Studio**, a local-only developer UI mounted by
`OrkApp::serve()` (ADR 0049) at `/studio` in development mode and
disabled by default in production. Studio is a Vite + React SPA
in [`crates/ork-studio/`](../../crates/) that reads exclusively
through the auto-generated REST/SSE surface (ADR
[`0056`](0056-auto-generated-rest-and-sse-surface.md)) plus a
small set of Studio-specific introspection routes under
`/studio/api/*`. It is **not** an alternative chat client; ADR
0017's gateway and end-user UI stay.

```bash
ork dev               # boots OrkApp + REST + SSE + Studio at :4111
open http://localhost:4111/studio
```

### Studio panels (v1)

| Panel | What it shows | Reads |
| ----- | ------------- | ----- |
| **Overview** | Manifest summary: agents, workflows, tools, MCP servers, memory backend | `GET /studio/api/manifest` |
| **Chat** | Per-agent chat with tool-call inspection, model swap, request-context form (from `request_context_schema`) | `POST /api/agents/:id/stream` |
| **Workflows** | Per-workflow form (input schema), run streaming, suspend/resume UI, past runs list | `POST /api/workflows/:id/run`, `GET /api/workflows/:id/runs` |
| **Memory** | Per-resource working memory + semantic-recall hits + thread list + delete-thread | `GET /studio/api/memory?resource=...` |
| **Traces** | Span tree per run with tokens/cost/latency, filter by agent/scorer | `GET /studio/api/traces?...` |
| **Scorers** | Live scorer dashboard: pass-rate over time, regressions, failed-run drill-in | `GET /studio/api/scorers?...` |
| **Evals** | Pick dataset + target, run, see `EvalReport`, compare to baseline | `POST /studio/api/evals/run` |
| **Logs** | Tail-f of structured logs filtered by tenant / agent / run | `GET /studio/api/logs?stream=1` (SSE) |

Each panel is a Studio-shipped feature; user code does not extend
Studio in v1 (extensibility is a follow-up ADR).

### Mount mechanics

Studio is built once at `cargo build --release -p ork-studio`. The
build output (an `index.html` + `assets/*` static bundle) is
embedded in the `ork-studio` crate via `rust-embed` and served by
ADR 0056's axum router under `/studio`. The `ServerConfig::studio`
field gates it:

```rust
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub tls: Option<TlsConfig>,
    pub auth: Option<AuthConfig>,
    pub studio: StudioConfig,
    pub openapi: bool,                     // /api/openapi.json
    pub swagger_ui: bool,                  // /swagger-ui
    pub resume_on_startup: bool,
}

pub enum StudioConfig {
    Disabled,
    Enabled,                               // default in `ork dev`
    EnabledWithAuth(StudioAuth),           // require local auth
}
```

Defaults: `Disabled` in production builds, `Enabled` in `ork dev`.
Studio explicitly refuses to start if the listener binds a non-
loopback interface and `StudioConfig::EnabledWithAuth` is not set
— a guard rail against accidentally exposing Studio to the
internet.

### Studio API (introspection-only)

A set of read-only routes under `/studio/api/*` powers the UI. They
are *not* part of the public ork API; they are Studio's view of
data ork already collects via the standard surfaces. Route shapes:

```
GET  /studio/api/manifest                   -> AppManifest (ADR 0049)
GET  /studio/api/memory?resource=...        -> { working, threads, recent_recall }
DELETE /studio/api/memory/threads/:id       -> 204
GET  /studio/api/traces?since=...           -> [TraceSummary]
GET  /studio/api/traces/:run_id             -> Trace (full span tree)
GET  /studio/api/scorers?agent=...&since=...-> [ScorerRow]
GET  /studio/api/scorers/aggregate?...      -> { pass_rate, p50, p95, regressions }
POST /studio/api/evals/run                  -> EvalReport (long-poll/SSE)
GET  /studio/api/logs?stream=1              -> SSE of LogEvent
```

These endpoints have a versioned envelope:

```json
{ "studio_api_version": 1, "data": { ... } }
```

Mismatched versions cause Studio to render a "your Studio bundle is
older than the server" banner and offer to reload.

### Tech stack

- **Frontend:** Vite + React + TypeScript. Tailwind for styling
  (matches the existing
  [`crates/ork-webui/`](../../crates/ork-webui/) stack — same
  toolchain, shared Vite config, separate `package.json`).
- **Streaming:** `EventSource` for SSE, `fetch` for everything
  else. No bespoke WebSocket protocol.
- **Charts:** `recharts` for scorer dashboards.
- **Trace viewer:** `react-flow` for span tree (already vetted by
  the team for the WebUI workflow visualiser; reuse).
- **Editor:** `monaco-editor` for the JSON-input form fields where
  a schema is too freeform.

The build is reproducible: `pnpm install --frozen-lockfile && pnpm
build` produces a deterministic bundle. CI runs the build per
commit so the embedded asset is up to date. Bundle size budget:
≤ 1 MiB gzip for v1; bundle-size CI fails the build above the
budget.

### Hot reload

In `ork dev` mode Studio's bundle is served by Vite's dev server
proxied through axum so HMR works. ADR
[`0057`](0057-ork-cli-dev-build-start.md) describes the dev-server
orchestration; this ADR commits Studio to the proxy shape.

### Authentication

Studio default in `ork dev` is no-auth on `127.0.0.1`. A non-loopback
listener forces `StudioConfig::EnabledWithAuth(...)`, which adds an
`Authorization: Bearer <studio_token>` requirement on
`/studio/api/*` and on the static asset routes. Token is generated
on `ork dev` boot and printed once to the console. ADR
[`0020`](0020-tenant-security-and-trust.md) covers the production
auth shape if Studio is ever exposed beyond local.

## Acceptance criteria

- [ ] New crate `crates/ork-studio/` containing the Vite + React
      bundle source under `web/`, the embedding code under
      `src/lib.rs` (using `rust-embed`), and the route handlers
      under `src/routes.rs`.
- [ ] `ork-studio` `Cargo.toml` declares `axum`, `tower-http`,
      `rust-embed`, `serde`, `tokio`. Allowed because Studio is a
      *gateway-shaped* crate, not a domain crate; it lives at the
      same boundary as `ork-api` and `ork-webui`. Per
      [`AGENTS.md`](../../AGENTS.md) §3 hexagonal rule:
      `ork-core`/`ork-agents`/`ork-workflow`/`ork-tool`/`ork-memory`/
      `ork-eval` cannot import these; Studio (gateway) can.
- [ ] `ServerConfig::studio` field added with the enum shown in
      `Decision`; default in `ork dev` is `Enabled`, in `ork
      start` is `Disabled`.
- [ ] Non-loopback bind + `Enabled` (not `EnabledWithAuth`) causes
      `OrkApp::serve()` to return
      `Err(OrkError::Configuration("studio refuses non-loopback
      bind without auth"))`. Verified by integration test.
- [ ] Routes implemented:
      `/studio/`, `/studio/assets/*` (embedded SPA);
      `/studio/api/manifest`, `/studio/api/memory`,
      `/studio/api/traces`, `/studio/api/scorers`,
      `/studio/api/evals/run`, `/studio/api/logs`.
- [ ] All eight panels listed in `Decision` ship with one
      Playwright/Vitest end-to-end test each under
      `crates/ork-studio/web/tests/`. The Chat panel test sends
      a message, receives a streamed response, and renders the
      tool-call list when the agent makes a call.
- [ ] Bundle size ≤ 1 MiB gzip for `pnpm build`; CI step
      `pnpm bundle-size:check` enforces.
- [ ] `studio_api_version` envelope is asserted by a contract
      test `crates/ork-studio/tests/api_envelope.rs`.
- [ ] `ork dev` Studio token generation: token is created on
      boot, printed exactly once to stdout, accepted as a
      `Bearer` token on `/studio/api/*` (no-auth on `127.0.0.1`
      in v1, see `Open questions`).
- [ ] Memory panel can delete a thread; integration test verifies
      the row is gone from `mem_messages` and `mem_embeddings`.
- [ ] Eval-run panel produces an `EvalReport` end-to-end against a
      local 3-example JSONL fixture; the report renders with
      passed / failed / regression counts.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- "Show me" loop closes for the developer. Today the inner-loop
  feedback when authoring an agent is `cargo test` plus log
  spelunking; with Studio the loop is "edit, save, click chat,
  watch trace." That is the dominant productivity delta when
  comparing Mastra ergonomics to ork ergonomics.
- The director's "is this real for our org?" question gains a
  visual answer: open Studio on a laptop, point at a screen,
  click a workflow, watch it run. ADR
  [`0048`](0048-pivot-to-code-first-rig-platform.md)'s pivot
  cashes in here.
- Scorer dashboard turns ADR 0054's `scorer_results` table into
  an at-a-glance signal. CI gates on the same data; Studio just
  visualises it.
- Reusing the existing
  [`crates/ork-webui/`](../../crates/ork-webui/) toolchain
  (Vite/React/Tailwind) keeps the team's frontend competence
  concentrated; no new framework lock-in.

### Negative / costs

- Frontend code is real cost. ~3000–5000 lines of TypeScript
  for v1. The bundle-size budget caps complexity; the
  per-panel tests prevent regression.
- Studio is **another deploy artefact**. The bundle has to ship
  with every release of `ork-studio`. Mitigation: it is
  embedded in the binary via `rust-embed`, so `ork start`
  ships one binary; no separate static-asset deployment.
- Hot reload is fragile. Vite's dev server in front of axum
  needs careful proxy config; ADR 0017 already paid this cost
  for WebUI and ADR 0057 inherits the working setup.
- The introspection API (`/studio/api/*`) is a versioned
  surface that Studio depends on. We commit to backward-
  compatible changes within `studio_api_version`; bumps are
  rare but documented.
- Tracing data (the Traces panel) requires ADR 0058
  (observability) to land first. Studio's panel can render an
  empty-state when no spans are stored, but the panel only gets
  useful once OTel ingestion lands.

### Neutral / follow-ups

- ADR 0058 (observability) ships the OTel ingestion that the
  Traces panel reads.
- Studio extensibility (user-supplied panels) is a v2 ADR; v1
  is fixed-set. Mastra Studio v1 was the same.
- Auth beyond a local bearer token (SSO, mTLS) is a future ADR;
  Studio is local-only in v1, and ADR 0020 owns the production
  story.
- `ork studio --remote https://prod.example.com` (Studio
  pointed at a remote ork instance) is a Mastra parity feature;
  needs ADR 0020 auth before it can ship safely.
- A `Studio --read-only` mode for support / sales-engineering
  use cases is a small additional flag; not in v1.

## Alternatives considered

- **Skip Studio; rely on Swagger UI + raw SSE.** Rejected. ADR
  0056 ships Swagger UI for free, but it is a poor inner-loop
  surface (no streaming, no trace tree, no memory inspector).
  The Mastra success thesis specifically rests on Studio
  ergonomics.
- **Build Studio into [`crates/ork-webui/`](../../crates/ork-webui/)
  rather than a sibling crate.** Rejected. The WebUI gateway
  has end-user-facing surface (chat client) with different auth
  requirements (production OAuth) and a different release
  cadence. Studio is dev-loop only; mixing them invites
  accidental exposure.
- **Use a heavier frontend framework (Next.js, SvelteKit).**
  Rejected. ork already uses Vite + React for WebUI; one stack
  reduces team cognitive load. Static SPA + SSE is enough.
- **Server-render Studio (HTMX, Liveview).** Rejected. The
  trace-viewer + chart panels need rich client-side state; SSR
  fights with SSE-driven streaming UIs. Vite + React is the
  shortest path.
- **Defer Studio to "later".** Rejected. ADR 0048's pivot leans
  on Studio as the visible win; without it the pivot is mostly
  invisible to end users. The risk of mis-shipping Studio is
  larger than the risk of cutting it.

## Affected ork modules

- New: [`crates/ork-studio/`](../../crates/) — embedded SPA +
  routes.
- [`crates/ork-app/`](../../crates/) — `ServerConfig::studio`
  field.
- [`crates/ork-api/`](../../crates/ork-api/) — mount routes via
  axum; Studio-API SSE shares the SSE encoder with ADR 0056.
- [`crates/ork-cli/`](../../crates/ork-cli/) — `ork dev` opens
  the browser to `/studio` after `serve()` resolves.
- [`crates/ork-webui/`](../../crates/ork-webui/) — unchanged;
  shared toolchain.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Studio in CLI reference](https://mastra.ai/reference/cli/mastra) | `/studio` mounted by `ork dev` |
| Mastra | live evaluation dashboard | Scorers panel |
| Mastra | playground chat | Chat panel |
| LangSmith | trace inspector | Traces panel |
| Solace Agent Mesh | no equivalent | n/a |

## Open questions

- **Auth on local loopback.** Default v1: no auth on
  `127.0.0.1`. A user running on a multi-tenant developer machine
  may want to require auth even locally. Add a config flag in v1.1.
- **Studio bundle versioning.** When the developer upgrades the
  ork CLI but the project's `Cargo.lock` pins an older
  `ork-studio`, Studio is older than the server. The
  `studio_api_version` envelope handles the rendering; the
  upgrade prompt is the UX answer.
- **Embedded vs fetched bundle.** Embedding via `rust-embed`
  bloats the binary (~1 MiB). Fetching from a CDN is
  unacceptable for on-prem deployments. Embedded wins.
- **Mobile.** v1 targets desktop browsers (≥ 1280 px wide). A
  responsive pass is v2.
- **Theme.** Light/dark theme support: included in v1 by toggle;
  defaults to system theme.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — `OrkApp` and
  `AppManifest`.
- ADR [`0050`](0050-code-first-workflow-dsl.md) — workflow event
  stream Studio renders.
- ADR [`0052`](0052-code-first-agent-dsl.md) — agent surface
  Studio chats with.
- ADR [`0053`](0053-memory-working-and-semantic.md) — memory data
  Studio inspects.
- ADR [`0054`](0054-live-scorers-and-eval-corpus.md) — scorer
  data Studio dashboards.
- ADR [`0017`](0017-webui-chat-client.md) — end-user WebUI
  (separate surface).
- ADR [`0020`](0020-tenant-security-and-trust.md) — auth model
  for production exposure.
- Mastra Studio (CLI ref): <https://mastra.ai/reference/cli/mastra>
- Mastra server overview:
  <https://mastra.ai/docs/server/mastra-server>
