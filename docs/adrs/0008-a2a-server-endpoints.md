# 0008 — A2A server endpoints in `ork-api`

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 2
- **Relates to:** 0002, 0003, 0004, 0005, 0006, 0007, 0009, 0017, 0020, 0022

## Context

ork's HTTP surface today is limited to ork-internal CRUD: tenants, workflows, runs, webhooks ([`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs)). There is no way for an external client (browser, vendor agent, partner mesh) to invoke an ork agent using the A2A protocol; the only path is "POST a workflow run" which is ork-specific and returns a `WorkflowRun`, not an A2A `Task`.

Now that the type model (ADR [`0003`](0003-a2a-protocol-model.md)), the `Agent` port (ADR [`0002`](0002-agent-port.md)), the transport split (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)), the discovery story (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)), and the remote client (ADR [`0007`](0007-remote-a2a-agent-client.md)) are decided, we can mount the server side. SAM does the equivalent in [`gateway/http_sse/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway/http_sse) — though heavily intertwined with its Web UI gateway. We split the two: this ADR mounts the **pure A2A protocol endpoints**; ADR [`0017`](0017-webui-chat-client.md) mounts the Web UI on top of them.

## Decision

ork **mounts a new A2A route module** at `crates/ork-api/src/routes/a2a.rs`, with the following normative endpoint set:

```
GET  /.well-known/agent-card.json                                — default agent card (configurable)
GET  /a2a/agents/{agent_id}/.well-known/agent-card.json         — per-agent card

POST /a2a/agents/{agent_id}                                      — JSON-RPC (methods below)
GET  /a2a/agents/{agent_id}/stream/{task_id}                     — SSE replay/live for a task

GET  /a2a/agents                                                 — non-spec convenience: list cards
GET  /a2a/tasks/{task_id}                                        — non-spec convenience: lookup task by id (cross-agent)
```

The JSON-RPC methods accepted on `POST /a2a/agents/{agent_id}` are exactly the A2A 1.0 method enum from ADR [`0003`](0003-a2a-protocol-model.md):

| Method | Semantics | Persistence |
| ------ | --------- | ----------- |
| `message/send` | One-shot send; returns `Task` (final or with `state=working` if asynchronous) | Insert `a2a_tasks` row, link to (possibly new) `WorkflowRun` |
| `message/stream` | Streaming send; response body is `text/event-stream` of `JsonRpcResponse<TaskEvent>` chunks | Same as above; events also published to Kafka `agent.status.<task_id>` (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)) for SSE bridge replay |
| `tasks/get` | Lookup current task state | Read from `a2a_tasks` + reconstruct `history` from `a2a_messages` |
| `tasks/cancel` | Cancel running task | Calls `Agent::cancel`; on success updates state to `canceled` |
| `tasks/pushNotificationConfig/set` | Register a callback URL | Insert into `a2a_push_configs` (ADR [`0009`](0009-push-notifications.md)) |
| `tasks/pushNotificationConfig/get` | Retrieve current config | Select from `a2a_push_configs` |

The handler for `POST /a2a/agents/{agent_id}` is shape-uniform: it parses the JSON-RPC envelope, dispatches on `method`, calls the appropriate `Agent` method via the registry, persists, and returns the JSON-RPC response. **The Agent trait is the only place that knows about the agent's actual implementation** — local, remote, or plugin (ADRs [`0002`](0002-agent-port.md), [`0007`](0007-remote-a2a-agent-client.md), [`0014`](0014-plugin-system.md)).

### Routing layer

[`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs) is updated:

```rust
pub fn create_router(state: AppState) -> Router {
    let public_routes = Router::new()
        .merge(health::routes())
        .merge(webhooks::routes(state.clone()))
        .merge(a2a::well_known_routes(state.clone()));   // NEW: cards are public

    let protected_routes = Router::new()
        .merge(tenants::routes(state.clone()))
        .merge(workflows::routes(state.clone()))
        .merge(a2a::protected_routes(state.clone()))     // NEW: send/stream/cancel
        .layer(middleware::from_fn(auth_middleware));

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
}
```

The in-process [`rate_limit_middleware`](../../crates/ork-api/src/middleware.rs) is removed in this PR — Kong owns rate limiting per ADR [`0004`](0004-hybrid-kong-kafka-transport.md). [`auth_middleware`](../../crates/ork-api/src/middleware.rs) continues to validate the JWT (now Kong-issued) and populate `RequestCtx { tenant_id, sub, scopes }`.

### SSE bridge

`GET /a2a/agents/{agent_id}/stream/{task_id}` is the single SSE surface. It does **not** call `Agent::send_stream` itself (the request was started by `message/stream`); it subscribes to:

1. Kafka topic `ork.a2a.v1.agent.status.<task_id>` for live events.
2. Redis cache for any events the client missed (replay window: last 60 seconds, configurable).
3. Postgres `a2a_messages` for full-history replay if the client provides `Last-Event-Id` older than the cache window.

This three-tier replay matches A2A's "SSE may reconnect; clients can also fall back to `tasks/get`" guidance.

### Persistence schema

A single migration `migrations/002_a2a_tasks.sql` adds:

```sql
CREATE TABLE a2a_tasks (
    id              UUID PRIMARY KEY,
    context_id      UUID NOT NULL,
    tenant_id       UUID NOT NULL REFERENCES tenants(id),
    agent_id        TEXT NOT NULL,
    workflow_run_id UUID REFERENCES workflow_runs(id),     -- nullable: A2A tasks may not be backed by a workflow
    parent_task_id  UUID REFERENCES a2a_tasks(id),         -- ADR 0006
    state           TEXT NOT NULL,                         -- TaskState enum
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at    TIMESTAMPTZ
);

CREATE INDEX a2a_tasks_workflow_run_id_idx ON a2a_tasks(workflow_run_id);
CREATE INDEX a2a_tasks_context_id_idx ON a2a_tasks(context_id);
CREATE INDEX a2a_tasks_parent_task_id_idx ON a2a_tasks(parent_task_id);
ALTER TABLE a2a_tasks ENABLE ROW LEVEL SECURITY;
CREATE POLICY a2a_tasks_tenant_isolation ON a2a_tasks
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);

CREATE TABLE a2a_messages (
    id          UUID PRIMARY KEY,
    task_id     UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
    role        TEXT NOT NULL,            -- "user" | "agent"
    parts       JSONB NOT NULL,           -- A2A Parts, ADR 0003
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    seq         BIGSERIAL,                -- ordering within task
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX a2a_messages_task_id_seq_idx ON a2a_messages(task_id, seq);
ALTER TABLE a2a_messages ENABLE ROW LEVEL SECURITY;
CREATE POLICY a2a_messages_tenant_isolation ON a2a_messages
    USING (task_id IN (SELECT id FROM a2a_tasks WHERE tenant_id = current_setting('app.current_tenant_id')::UUID));
```

A new repository trait `A2aTaskRepository` lives in `crates/ork-core/src/ports/a2a_repository.rs` with the Postgres impl in `crates/ork-persistence/src/postgres/a2a_repo.rs`, mirroring the existing pattern of [`PgWorkflowRepository`](../../crates/ork-persistence/src/postgres/workflow_repo.rs).

### Task ↔ WorkflowRun mapping

A single A2A `Task` corresponds to one or zero `WorkflowRun`s:

- **Workflow-backed task:** the agent receiving the message decides to start a workflow (e.g. the `Planner` agent's logic produces a multi-step plan that becomes a workflow run). `a2a_tasks.workflow_run_id` is set; `Task.status` is derived from `WorkflowRunStatus`.
- **Single-shot task:** an agent that does not invoke a workflow. `workflow_run_id` is NULL; `Task.status` lives only in the `a2a_tasks.state` column.

This keeps the existing workflow surface (POST `/api/workflows/{id}/runs`) intact and avoids forcing every interaction to be a workflow.

### Auth and tenant scoping

`auth_middleware` parses the Kong-issued JWT and writes `RequestCtx`. `RequestCtx.tenant_id` is set on the DB session via `SET LOCAL app.current_tenant_id = $1` per request transaction (ADR [`0020`](0020-tenant-security-and-trust.md)) — finally activating the RLS already declared in [`migrations/001_initial.sql`](../../migrations/001_initial.sql).

Tenant resolution for incoming A2A calls:

1. JWT claim `tenant_id` (preferred).
2. Header `X-Tenant-Id` if the JWT has admin scope and explicitly impersonates.
3. Reject with HTTP 401 otherwise.

Cross-tenant calls are forbidden at this layer; ADR [`0020`](0020-tenant-security-and-trust.md) details the trust model.

### Webhooks reuse

[`crates/ork-api/src/routes/webhooks.rs`](../../crates/ork-api/src/routes/webhooks.rs) is the existing inbound HTTP machinery and is reused for push-notification **inbound delivery confirmation** (ADR [`0009`](0009-push-notifications.md)).

## Consequences

### Positive

- ork is now a vanilla A2A endpoint behind Kong: `curl https://api.example.com/a2a/agents/planner -d '{...}'` Just Works.
- The same `Agent` trait powers both inbound calls and outbound (ADR [`0007`](0007-remote-a2a-agent-client.md)) — round-trip self-call is a useful test fixture.
- The `Task` ↔ `WorkflowRun` mapping is explicit, so we don't lose ork's existing workflow audit trail.
- The SSE bridge subscribing to Kafka means `message/stream` scales horizontally: any ork-api instance can serve any task's stream.

### Negative / costs

- Adds two tables and one migration; tenant_id now must be set per-tx (mild perf cost vs unscoped).
- The replay window in Redis adds operational complexity (eviction tuning, monitoring).
- Removing the in-process rate limiter requires Kong to be in the loop in dev too, or a feature-flagged loopback limiter for local development.

### Neutral / follow-ups

- ADR [`0017`](0017-webui-chat-client.md) builds the Web UI as a consumer of these endpoints (it is **not** a separate transport).
- ADR [`0009`](0009-push-notifications.md) implements `pushNotificationConfig` semantics on top of the schema introduced here.
- ADR [`0022`](0022-observability.md) wires per-method tracing spans and a histogram of SSE event lag.

## Alternatives considered

- **Serve the A2A protocol on a separate port/binary.** Rejected: complicates Kong routing for no benefit; the JWT middleware and DB pool are shared.
- **Make `WorkflowRun` and `Task` literally the same row.** Rejected: A2A tasks aren't always workflows, and a workflow can fan out into multiple tasks.
- **Transport SSE through Server-Sent Events polyfill (long-poll).** Rejected: HTTP/1.1+ + Kong both natively support SSE; no need to polyfill.
- **Use gRPC instead of JSON-RPC.** Rejected: A2A spec is JSON-RPC; gRPC would block interop.

## Affected ork modules

- New: `crates/ork-api/src/routes/a2a.rs` — handlers (well-known, JSON-RPC dispatcher, SSE bridge).
- [`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs) — mount new module; remove `rate_limit_middleware`.
- [`crates/ork-api/src/middleware.rs`](../../crates/ork-api/src/middleware.rs) — `auth_middleware` writes `RequestCtx` and sets `app.current_tenant_id` per tx.
- [`crates/ork-api/src/state.rs`](../../crates/ork-api/src/state.rs) — add `kafka` and `redis` handles for the SSE bridge.
- New: `crates/ork-core/src/ports/a2a_repository.rs`.
- New: `crates/ork-persistence/src/postgres/a2a_repo.rs`.
- New: `migrations/002_a2a_tasks.sql`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) — `WorkflowRunStatus` extended (per ADR [`0003`](0003-a2a-protocol-model.md)).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| HTTP+SSE gateway | [`gateway/http_sse/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway/http_sse) | `routes/a2a.rs` (this ADR) + `crates/ork-webui` (ADR [`0017`](0017-webui-chat-client.md)) |
| Task/message persistence | SAM `tasks` DB | `a2a_tasks` + `a2a_messages` tables |
| SSE replay buffer | SAM `routers/sse.py` | Redis cache + Postgres replay |
| `tasks/get` / `tasks/cancel` handlers | SAM router code | JSON-RPC dispatch in `routes/a2a.rs` |
| Push notification config storage | SAM `routers/...push_notification` | `a2a_push_configs` table (ADR [`0009`](0009-push-notifications.md)) |

## Open questions

- Do we expose a non-spec `GET /a2a/agents/{agent_id}/tasks` for listing tasks per agent (useful for ops UI)? Probably yes, gated behind `ops:read` scope (ADR [`0021`](0021-rbac-scopes.md)).
- Should `tasks/get` return only the **currently** addressable agent's tasks, or any task in the tenant? Decision: any task in the tenant, since A2A clients should be able to look up tasks they started without knowing which agent currently owns them.

## References

- A2A spec — methods and SSE format: <https://github.com/google/a2a>
- SAM HTTP/SSE gateway: <https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway/http_sse>
- [`future-a2a.md` §3, §6](../../future-a2a.md)
- Existing migrations: [`migrations/001_initial.sql`](../../migrations/001_initial.sql)
