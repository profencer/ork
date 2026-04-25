# 0019 — Scheduled tasks

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0005, 0008, 0017, 0018, 0020, 0022

## Context

ork already has a [`WorkflowScheduler`](../../crates/ork-core/src/workflow/scheduler.rs): an in-memory cron registry with `register`, `unregister`, `get_due_workflows`, and a `run_loop`. The scheduler is **never started** by `ork-api` ([`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)), and there is no HTTP surface for tenants to manage schedules. The cron data lives only in process memory; restarts lose all schedules.

For SAM parity we need:

- Schedules **persist** across process restarts.
- Tenants can **CRUD** schedules via REST (and via the Web UI per ADR [`0017`](0017-webui-chat-client.md)).
- The scheduler is **leader-elected** in multi-process deployments to avoid duplicate firings.
- Schedules **integrate with A2A** — a scheduled run starts as a normal A2A `Task` so observability (ADR [`0022`](0022-observability.md)) and SSE (ADR [`0008`](0008-a2a-server-endpoints.md)) work uniformly.
- DevPortal (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)) lists the scheduled triggers as part of the agent's catalog metadata for discoverability.

SAM's scheduled tasks live in its admin UI; the underlying mechanism is similar (cron + persistence).

## Decision

ork **wires the existing `WorkflowScheduler` into `ork-api`**, adds Postgres-backed persistence, REST + Web UI surfaces, and implements leader election so only one process fires each schedule.

### Persistence

New table:

```sql
CREATE TABLE schedules (
    id              UUID PRIMARY KEY,
    tenant_id       UUID NOT NULL REFERENCES tenants(id),
    name            TEXT NOT NULL,
    target          JSONB NOT NULL,         -- workflow id OR direct agent message
    cron_expr       TEXT NOT NULL,
    timezone        TEXT NOT NULL DEFAULT 'UTC',
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    next_fire_at    TIMESTAMPTZ,            -- denormalised for fast lookup
    last_fire_at    TIMESTAMPTZ,
    last_run_id     UUID,
    last_status     TEXT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

CREATE INDEX schedules_next_fire_at_idx ON schedules(next_fire_at) WHERE enabled;
ALTER TABLE schedules ENABLE ROW LEVEL SECURITY;
CREATE POLICY schedules_tenant_isolation ON schedules
    USING (tenant_id = current_setting('app.current_tenant_id')::UUID);
```

`migrations/006_schedules.sql`. The `target` JSONB is a tagged union:

```jsonc
// Workflow target (current behaviour)
{ "type": "workflow", "workflow_id": "<uuid>", "input": { ... } }

// Direct A2A target (new)
{ "type": "a2a_message",
  "agent_id": "planner",
  "message": { "role": "user", "parts": [ { "kind": "text", "text": "Daily summary" } ] },
  "context_id": "<optional uuid>"
}
```

The A2A target lets a schedule directly invoke an agent without a wrapper workflow — useful for "every Monday at 9am, ping the standup agent".

### Scheduler service

A new `ScheduleService` in `crates/ork-core/src/workflow/schedule_service.rs`:

```rust
pub struct ScheduleService {
    repo: Arc<dyn ScheduleRepository>,        // new repo trait, Postgres impl in ork-persistence
    scheduler: Arc<WorkflowScheduler>,        // existing in-memory tracker
    runner: Arc<ScheduleRunner>,
    leader: Arc<dyn LeaderElector>,
}

impl ScheduleService {
    pub async fn list(&self, tenant_id: TenantId) -> Result<Vec<Schedule>, OrkError>;
    pub async fn create(&self, tenant_id: TenantId, req: CreateScheduleRequest) -> Result<Schedule, OrkError>;
    pub async fn update(&self, tenant_id: TenantId, id: ScheduleId, req: UpdateScheduleRequest) -> Result<Schedule, OrkError>;
    pub async fn delete(&self, tenant_id: TenantId, id: ScheduleId) -> Result<(), OrkError>;
    pub async fn fire_now(&self, tenant_id: TenantId, id: ScheduleId) -> Result<RunHandle, OrkError>;
    pub async fn start_loop(self: Arc<Self>);   // background tick
}
```

The `start_loop` method is awaited from [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)'s startup sequence — fixing the long-standing bug that the scheduler is dead code today.

### Leader election

Multi-process deployments must avoid duplicate firings. We adopt a Postgres advisory-lock leader:

```rust
pub trait LeaderElector: Send + Sync {
    async fn try_acquire(&self, lease_secs: u64) -> Result<bool, OrkError>;
    async fn release(&self) -> Result<(), OrkError>;
}

pub struct PgAdvisoryLockLeader { pool: PgPool, lock_id: i64 }
```

On each tick (every 30s, matching the existing `WorkflowScheduler::run_loop`), the process attempts `pg_try_advisory_lock(<schedule_lock_id>)`. The holder fires due schedules; the others skip. Heartbeat the lock by holding the connection. Loss of lock = stop firing on next tick.

Alternative impls (Redis-based, Kubernetes Lease) can register through the same trait when needed.

### Firing semantics

For each due schedule:

1. Mark `next_fire_at = NULL` to prevent reentry, then compute the new `next_fire_at` from the cron expression evaluated in `timezone`.
2. Resolve the target:
   - **Workflow target** → call `WorkflowService::start_run` (existing path).
   - **A2A message target** → enqueue an A2A `message/send` against the agent registry; this internally creates an `a2a_tasks` row (ADR [`0008`](0008-a2a-server-endpoints.md)).
3. Update `last_fire_at`, `last_run_id`, `last_status = "started"`.
4. On terminal completion (subscribe to the run/task event stream), update `last_status` to `succeeded | failed | canceled`.

Failed firings do not remove the schedule; the next tick re-evaluates `next_fire_at`.

### REST surface

Mounted under `/api/schedules` (protected by [`auth_middleware`](../../crates/ork-api/src/middleware.rs)):

```
GET    /api/schedules                   — list (tenant-scoped via RLS)
POST   /api/schedules                   — create
GET    /api/schedules/{id}              — read
PATCH  /api/schedules/{id}              — update (cron, target, enabled, name, metadata)
DELETE /api/schedules/{id}              — delete
POST   /api/schedules/{id}:run          — fire now (manual trigger)
GET    /api/schedules/{id}/runs         — paginated history
```

Web UI (ADR [`0017`](0017-webui-chat-client.md)) consumes these for the scheduled-tasks view.

### Cron syntax and timezones

We continue using the [`cron`](https://crates.io/crates/cron) crate (already a dependency in [`crates/ork-core/src/workflow/scheduler.rs`](../../crates/ork-core/src/workflow/scheduler.rs)). The `timezone` field uses IANA names (`UTC`, `Europe/Berlin`); we add `chrono-tz` for evaluation. Validation on `create`: parse the cron expression and reject malformed ones with a 400.

### DevPortal exposure

Each schedule whose target is an A2A agent contributes an entry to the agent's `AgentCard.metadata.scheduled_triggers` array (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)). DevPortal renders these so consumers can see "Planner agent runs daily at 09:00 UTC" without REST querying.

### Backfill / catch-up

If the leader is down for longer than the cron interval, missed schedules **are not retroactively fired** (default policy, matching SAM). A `catchup: bool` field on `Schedule` lets callers opt into firing once on resume; the default is `false`.

### Quotas

Per-tenant cap: `tenant.max_schedules` (default 100, configurable). Rate-limit on `:run` manual firings via Kong (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)).

## Consequences

### Positive

- The dormant scheduler becomes a first-class feature.
- Schedules persist across restarts; deployments can scale horizontally without duplicate firings.
- A2A agents can be scheduled directly without wrapping in a single-step workflow.
- DevPortal exposure makes scheduled triggers discoverable.

### Negative / costs

- Adds a Postgres advisory-lock dependency for the leader; visible in `pg_locks` and worth documenting for ops.
- Cron evaluation across timezones is a known foot-gun; we surface clear validation errors.
- Catch-up policy choice (off by default) will surprise some users; documented prominently.

### Neutral / follow-ups

- A future ADR may add an event-trigger (Kafka topic) alongside cron — currently we have webhook triggers via [`crates/ork-api/src/routes/webhooks.rs`](../../crates/ork-api/src/routes/webhooks.rs) and ADR [`0013`](0013-generic-gateway-abstraction.md)'s `webhook` gateway adapter; cron + webhook covers the common cases.
- Sub-minute schedules are not supported by the underlying `cron` crate; raise a parse error on attempt.
- ADR [`0022`](0022-observability.md) defines metrics for schedule lag.

## Alternatives considered

- **Use a dedicated scheduler service (Apache Airflow, Temporal).** Rejected: ork already owns the workflow primitives; adding a second scheduler is operational debt.
- **In-memory only (no persistence).** Rejected: violates the "schedules survive restarts" requirement.
- **Distributed lock via etcd / Consul.** Rejected: brings new infra; Postgres is already in the stack.
- **Per-process tick with deduplication via the schedule row's `next_fire_at` UPDATE-RETURNING.** Tempting; works but has more failure modes than advisory lock under contention. Defer as a future optimisation.

## Affected ork modules

- [`crates/ork-core/src/workflow/scheduler.rs`](../../crates/ork-core/src/workflow/scheduler.rs) — keep as the in-memory tracker; wrap in `ScheduleService`.
- New: `crates/ork-core/src/workflow/schedule_service.rs`.
- New: `crates/ork-core/src/ports/schedule_repository.rs`.
- New: `crates/ork-persistence/src/postgres/schedule_repo.rs` and `pg_advisory_lock.rs`.
- New: `crates/ork-api/src/routes/schedules.rs`.
- [`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs) — mount `schedules` under protected routes.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — start `ScheduleService::start_loop`.
- New SQL: `migrations/006_schedules.sql`.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) — `ork schedule list/create/run` subcommands.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Scheduled tasks UI | SAM web UI | Web UI scheduled-tasks view (ADR [`0017`](0017-webui-chat-client.md)) |
| Cron-driven workflow firing | SAM scheduler | `WorkflowScheduler` + `ScheduleService` |
| Agent direct schedule | YAML schedules in SAM agent definitions | `target.type = "a2a_message"` |
| Trigger discovery | implicit | DevPortal `scheduled_triggers` extension |

## Open questions

- Should `:run` firings respect `enabled = false` (require enabling first) or override? Decision: override; manual firings ignore `enabled`.
- Per-schedule retry policy on failure? Defer; rely on workflow / agent-level retries.
- Display-name for schedules in DevPortal — already handled by the `name` field.

## References

- `cron` crate: <https://crates.io/crates/cron>
- `chrono-tz`: <https://crates.io/crates/chrono-tz>
- Postgres advisory locks: <https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS>
