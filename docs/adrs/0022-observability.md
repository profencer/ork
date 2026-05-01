# 0022 — Observability: tracing, monitors, task event log

- **Status:** Superseded by 0048
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0002, 0004, 0008, 0009, 0011, 0018, 0019, 0020, 0021

## Context

ork uses [`tracing`](https://crates.io/crates/tracing) for logging today (the [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) initialises a subscriber, [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) emits `info!` and `error!` events). What it lacks:

- **Distributed traces.** No OpenTelemetry exporter; no spans linking an inbound A2A request → workflow run → tool calls → outbound delegations.
- **Metrics.** No Prometheus / OTLP metrics; no SLI dashboards; no per-tenant cost telemetry from ADR [`0012`](0012-multi-llm-providers.md).
- **Audit log.** ADR [`0020`](0020-tenant-security-and-trust.md) and ADR [`0021`](0021-rbac-scopes.md) emit `audit.*` tracing events but there is no durable, queryable destination.
- **Task event log for SSE replay.** ADR [`0008`](0008-a2a-server-endpoints.md)'s SSE bridge needs structured event history per task to satisfy `Last-Event-Id` reconnects.
- **Per-agent / per-tool monitors.** SAM has `AgentMonitor` and `ToolMonitor` hooks ([`agent/utils/monitors.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/utils/monitors.py) and similar) for behaviour assertions, prompt-injection detection, and rate enforcement; ork has nothing equivalent.

## Decision

ork **adopts a four-pillar observability model**: traces (OpenTelemetry), metrics (Prometheus + OTLP), structured task event log (Postgres), and audit stream (separate Postgres table + optional sink). Plus an `AgentMonitor` / `ToolMonitor` hook surface for behavioural checks.

### Pillar 1 — Distributed tracing

Use `tracing-opentelemetry` to emit OTLP spans. Spans cover:

| Span name | Source | Key attributes |
| --------- | ------ | -------------- |
| `http.request` | `axum` middleware | method, path, status, tenant_id, request_id |
| `a2a.method` | A2A route handler (ADR [`0008`](0008-a2a-server-endpoints.md)) | method, agent_id, task_id |
| `agent.send` | `Agent::send` wrapper in registry | agent_id, task_id, tenant_id, parent_task_id |
| `agent.send_stream.tick` | Per LLM completion in `LocalAgent` (ADR [`0011`](0011-native-llm-tool-calling.md)) | tokens_in, tokens_out, finish_reason |
| `tool.execute` | `ToolExecutor::execute` wrapper | tool_name, duration_ms, error |
| `mcp.call` | `McpClient::execute` | server_id, tool, transport |
| `workflow.node` | Engine node walker (ADR [`0018`](0018-dag-executor-enhancements.md)) | node.id, node.kind, branch_idx, iteration |
| `kafka.produce` / `kafka.consume` | `crates/ork-eventing` | topic, key, partition |
| `pg.tx` | DB tx wrapper | tenant_id |

W3C `traceparent` propagates across:

- Inbound HTTP (Kong forwards the header).
- Outbound `A2aRemoteAgent` calls — set on JSON-RPC requests.
- Kafka headers — `ork-trace-id` per ADR [`0004`](0004-hybrid-kong-kafka-transport.md).
- Push notification delivery — set on outbound webhooks (ADR [`0009`](0009-push-notifications.md)).

Sampling: tail-based via the OTLP collector; head-based fallback at `[observability.tracing.sample_rate] = 0.1` in config. Per-tenant overrides supported.

### Pillar 2 — Metrics

Exposed at `GET /metrics` (Prometheus text format) and pushed via OTLP when `[observability.metrics.otlp]` is set.

Standard metrics, all labelled by `tenant_id` where applicable:

| Metric | Type | Purpose |
| ------ | ---- | ------- |
| `ork_a2a_requests_total{method,agent,status}` | counter | A2A traffic |
| `ork_a2a_request_duration_seconds{method,agent}` | histogram | latency |
| `ork_agent_send_duration_seconds{agent}` | histogram | end-to-end agent latency |
| `ork_tool_execute_duration_seconds{tool,outcome}` | histogram | tool latency |
| `ork_tool_execute_total{tool,outcome}` | counter | tool counts |
| `ork_llm_tokens_total{provider,model,direction}` | counter | token usage |
| `ork_llm_cost_usd_total{tenant,provider,model}` | counter | cost telemetry from ADR [`0012`](0012-multi-llm-providers.md) |
| `ork_workflow_runs_active` | gauge | concurrency snapshot |
| `ork_workflow_step_total{kind,outcome}` | counter | step throughput |
| `ork_kafka_publish_lag_seconds{topic}` | histogram | producer freshness |
| `ork_sse_clients{agent}` | gauge | live SSE listeners |
| `ork_sse_event_lag_seconds{topic}` | histogram | event delay vs Kafka |
| `ork_push_outbox_dead_letter_total` | counter | failed push deliveries (ADR [`0009`](0009-push-notifications.md)) |
| `ork_schedule_lag_seconds` | histogram | how late schedules fire (ADR [`0019`](0019-scheduled-tasks.md)) |
| `ork_discovery_remote_agents{state}` | gauge | discovery cache state |
| `ork_artifact_storage_bytes_total{scheme,tenant}` | counter | artifact growth |
| `ork_audit_denied_total{scope}` | counter | RBAC denials (ADR [`0021`](0021-rbac-scopes.md)) |

Recording rules and alert thresholds live in `docs/operations/observability/` (out of scope for this ADR; created as a follow-up).

### Pillar 3 — Task event log

ADR [`0008`](0008-a2a-server-endpoints.md)'s SSE bridge needs persistent per-task events for replay beyond the Redis cache window. We add:

```sql
CREATE TABLE a2a_task_events (
    id          UUID PRIMARY KEY,
    task_id     UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
    seq         BIGSERIAL,
    kind        TEXT NOT NULL,            -- "status_update" | "artifact_update" | "message" | "tool_call" | "tool_result"
    payload     JSONB NOT NULL,           -- A2A event JSON
    trace_id    TEXT,                     -- W3C traceparent that produced it
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX a2a_task_events_task_id_seq_idx ON a2a_task_events(task_id, seq);
ALTER TABLE a2a_task_events ENABLE ROW LEVEL SECURITY;
CREATE POLICY a2a_task_events_tenant_isolation ON a2a_task_events
    USING (task_id IN (SELECT id FROM a2a_tasks WHERE tenant_id = current_setting('app.current_tenant_id')::UUID));
```

Migration `migrations/008_task_events.sql`. Events are written by `LocalAgent::send_stream` (ADR [`0011`](0011-native-llm-tool-calling.md)) and by the engine node walker (ADR [`0018`](0018-dag-executor-enhancements.md)). Retention policy: 30 days by default, configurable per-tenant.

The SSE bridge's three-tier replay (Kafka live → Redis recent → Postgres historic) from ADR [`0008`](0008-a2a-server-endpoints.md) reads its third tier from this table.

### Pillar 4 — Audit stream

Separate from the task event log because audit retention and access controls differ:

```sql
CREATE TABLE audit_events (
    id           UUID PRIMARY KEY,
    tenant_id    UUID,
    actor        TEXT NOT NULL,                 -- sub from JWT
    action       TEXT NOT NULL,                 -- "scope_denied" | "sensitive_grant" | "tenant_create" | ...
    resource     TEXT,                          -- agent id / tool / artifact / scope name
    result       TEXT NOT NULL,                 -- "allow" | "deny" | "error"
    request_id   TEXT,
    trace_id     TEXT,
    metadata     JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_tenant_actor_idx ON audit_events(tenant_id, actor, created_at);
```

Migration `migrations/009_audit.sql`. Append-only (revoke `UPDATE`/`DELETE` from the `ork_app` Postgres role). Retention: 1 year default; longer when regulated. Optional sink to a SIEM via OTLP logs exporter or Kafka topic `ork.audit.v1.events` for organisations with central security tooling.

The `tracing` events `audit.scope_denied`, `audit.sensitive_grant`, `audit.tenant_*`, `audit.key_rotated` are intercepted by an `AuditLayer` and persisted; they also continue to flow to the standard tracing pipeline.

### Pillar 5 — Agent and Tool monitors

A new hook surface in `crates/ork-core/src/observability/monitors.rs` lets cross-cutting checks run at the `Agent` and `ToolExecutor` boundaries without modifying call sites:

```rust
#[async_trait::async_trait]
pub trait AgentMonitor: Send + Sync {
    async fn before_send(&self, ctx: &AgentContext, msg: &AgentMessage) -> Result<(), MonitorReject>;
    async fn after_send(&self, ctx: &AgentContext, result: Result<&AgentMessage, &OrkError>);
    async fn on_event(&self, ctx: &AgentContext, ev: &AgentEvent);
}

#[async_trait::async_trait]
pub trait ToolMonitor: Send + Sync {
    async fn before_execute(&self, tenant_id: TenantId, name: &str, input: &Value) -> Result<(), MonitorReject>;
    async fn after_execute(&self, tenant_id: TenantId, name: &str, result: Result<&Value, &OrkError>);
}

pub struct MonitorReject {
    pub reason: String,
    pub mapped_state: TaskState,         // typically Failed | Rejected
}
```

Built-in monitors:

- `RateLimitMonitor` — per-tenant per-agent QPS caps (replaces the global rate limiter dropped in ADR [`0008`](0008-a2a-server-endpoints.md)).
- `BudgetMonitor` — per-tenant LLM cost ceiling (consumes `ork_llm_cost_usd_total`).
- `PromptInjectionMonitor` (optional) — runs a configurable detector on inbound messages.
- `OutputPiiMonitor` (optional) — scans outbound text for PII patterns.

Plugins can register additional monitors via the plugin API (ADR [`0014`](0014-plugin-system.md)).

### Configuration

```toml
[observability]
service_name = "ork-api"

[observability.tracing]
exporter = "otlp"            # "otlp" | "stdout" | "off"
endpoint = "http://otel-collector:4317"
sample_rate = 0.1

[observability.metrics]
prometheus_bind = "0.0.0.0:9090"
otlp = { endpoint = "http://otel-collector:4317" }

[observability.audit]
sink = "postgres"            # "postgres" | "postgres+kafka" | "postgres+otlp"
postgres_retention_days = 365
kafka_topic = "ork.audit.v1.events"

[observability.monitors]
rate_limit = { default_qps = 5, per_tenant_overrides = {} }
budget = { default_usd_per_day = 50.0 }
```

### Health and readiness

- `GET /health/live` — process up.
- `GET /health/ready` — DB reachable, Kafka reachable, leader-election state, plus monitors green.

Existing [`crates/ork-api/src/routes/health.rs`](../../crates/ork-api/src/routes/health.rs) is extended.

## Consequences

### Positive

- End-to-end traces from "user types in Web UI" to "outbound MCP tool call" are stitched together.
- Per-tenant cost is observable and budget-enforced.
- SSE reconnect is reliable across the full task lifetime.
- Audit trail is durable, append-only, and exportable to SIEM.
- Cross-cutting behaviour (rate limit, budgets, prompt-injection) lives in monitors that are individually testable and toggleable.

### Negative / costs

- More writes to Postgres (task events + audit). Mitigated by batched inserts and partitioned tables (per-month partitioning for `a2a_task_events`).
- OpenTelemetry collector becomes infra dependency for production. Acceptable and standard.
- Monitor overhead per call. Bounded: each monitor is non-blocking by default; expensive ones (PII scan) marked `slow` and optionally async (results land via tracing rather than blocking the response).

### Neutral / follow-ups

- A future ADR may add **tracing samplers** that sample more aggressively per-tenant or per-agent (e.g. always sample errors).
- Frontend tracing (Web UI client spans) is a follow-up.
- Cross-process leader election state from ADR [`0019`](0019-scheduled-tasks.md) gets a metric (`ork_scheduler_leader{instance}`).

## Alternatives considered

- **Use only Prometheus, no OTLP.** Rejected: traces require OTLP; audit + log shipping benefit from a unified collector.
- **Embed audit in the tracing pipeline only.** Rejected: tracing infra is best-effort and lossy; audit must be durable.
- **Use Kafka exclusively for the task event log.** Rejected: SSE replay benefits from indexed Postgres queries; Kafka is the live tier, Postgres is the historic tier.
- **Buy-vs-build a managed SaaS observability stack.** Outside this ADR's scope; ork emits standard OTLP, deployments choose the backend.

## Affected ork modules

- New: `crates/ork-core/src/observability/{mod.rs,monitors.rs}`.
- New: `crates/ork-api/src/observability.rs` — Prometheus exporter, OTLP setup.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — initialise tracing + metrics + monitors at boot; subscribe `AuditLayer` to the tracing dispatcher.
- [`crates/ork-api/src/middleware.rs`](../../crates/ork-api/src/middleware.rs) — request-id, traceparent propagation.
- [`crates/ork-api/src/routes/health.rs`](../../crates/ork-api/src/routes/health.rs) — extended `/health/ready`.
- [`crates/ork-agents/src/registry.rs`](../../crates/ork-agents/src/registry.rs) — call agent monitors around `Agent::send` / `send_stream`.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs) — call tool monitors around `ToolExecutor::execute`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) — span per node; persist events to `a2a_task_events`.
- New SQL: `migrations/008_task_events.sql`, `migrations/009_audit.sql`.
- [`config/default.toml`](../../config/default.toml) — `[observability]` block.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Logging + tracing config | SAC config + Python `logging` | `tracing` + OTLP exporter |
| Agent monitor hook | [`agent/utils/monitors.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/utils/monitors.py) | `AgentMonitor` trait |
| Tool monitor | implicit in SAM tool wrappers | `ToolMonitor` trait |
| Task event persistence | SAM tasks DB + replay | `a2a_task_events` table |
| Audit logging | SAM auth middleware logs | `audit_events` table + `AuditLayer` |

## Open questions

- Per-tenant Prometheus endpoints (one big endpoint with cardinality blow-up vs partitioned)? Decision: one endpoint with limited cardinality (no `task_id` label).
- Storage for traces (Tempo, Jaeger, Honeycomb)? Out of scope; deployment chooses.
- Should audit events be queryable via a `/api/audit` route? Yes for `tenant:admin`; deferred to a small follow-up PR.

## References

- OpenTelemetry: <https://opentelemetry.io/>
- `tracing-opentelemetry`: <https://crates.io/crates/tracing-opentelemetry>
- W3C Trace Context: <https://www.w3.org/TR/trace-context/>
- SAM monitors: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/utils/monitors.py>
