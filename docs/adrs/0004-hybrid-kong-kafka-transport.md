# 0004 — Hybrid A2A transport: Kong/HTTP+SSE for sync, Kafka for async

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 1
- **Relates to:** 0003, 0005, 0006, 0008, 0009, 0017, 0020

## Context

SAM puts the entire A2A protocol on Solace topics: requests, responses, status updates, discovery, and push notifications all flow through the broker, with `replyTo` and `a2aStatusTopic` user properties for correlation (see [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) and [`core_a2a/service.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/core_a2a/service.py)). That gives SAM a single uniform plane.

ork's constraints are different:

- **No Solace.** The team is not adopting Solace; the platform standard for sync HTTP is **Kong** and the platform standard for async messaging is **Kafka**. Both are surfaced through the team's **DevPortal**.
- **A2A spec compliance** is required so external clients (browsers, vendor agents, partners) can call ork agents the same way they call Google ADK or any other A2A implementation. The spec wire format is JSON-RPC over HTTPS with SSE for streaming.
- We still need cheap, scalable async fan-out for: discovery heartbeats, mid-task status updates published to many subscribers, push-notification delivery, and fire-and-forget peer delegation.

We need to decide: HTTP-only, Kafka-only, or hybrid.

## Decision

ork **adopts a hybrid transport**:

- **All synchronous request/response and SSE streaming** go through **HTTPS** fronted by **Kong**. This is the spec-compliant A2A surface and the only thing external A2A clients need to speak.
- **All async event-mesh traffic** (discovery, status fan-out, push notifications, fire-and-forget delegation) goes over **Kafka** topics.
- **DevPortal** is the catalog/discovery surface that aggregates both Kong-published HTTP routes and Kafka-published topics into one source of truth (see ADR [`0005`](0005-agent-card-and-devportal-discovery.md)).
- **Redis** (already configured in [`config/default.toml`](../../config/default.toml) but currently unused) is the short-lived correlation/replay cache: SSE reconnect cursor, request-id deduplication, in-flight push delivery state. Kafka is the durable record.

### Sync plane: Kong + HTTP/SSE

Per-agent endpoints (paths defined in detail in ADR [`0008`](0008-a2a-server-endpoints.md)):

```
GET  /a2a/agents/{agent_id}/.well-known/agent-card.json
POST /a2a/agents/{agent_id}                    (JSON-RPC; methods per ADR 0003)
GET  /a2a/agents/{agent_id}/stream/{task_id}   (SSE replay)
```

Kong responsibilities:

- TLS termination + mTLS for cross-org A2A (mapped to A2A `securitySchemes`).
- OAuth2 / JWT validation (DevPortal-issued tokens; ork's [`auth_middleware`](../../crates/ork-api/src/middleware.rs) trusts pre-validated headers `X-Tenant-Id`, `X-Subject`, `X-Scopes`).
- Per-route rate limits (replaces the ineffective per-request rate limiter currently in [`crates/ork-api/src/middleware.rs`](../../crates/ork-api/src/middleware.rs)).
- Request size enforcement (matches A2A `defaultInputModes` size hints).
- Header-based routing of `/a2a/agents/{agent_id}` to the right ork-api instance based on agent locality (Kong upstream selectors).

### Async plane: Kafka topic layout

Topics live under a configurable namespace `ork.a2a.v1.*`. The naming mirrors SAM's `{namespace}/a2a/v1/...` (see [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py)) but with Kafka semantics (dots, no wildcards):

| Topic | Purpose | Key | Producer | Consumer |
| ----- | ------- | --- | -------- | -------- |
| `ork.a2a.v1.discovery.agentcards` | Agent card heartbeats | `agent_id` | Each agent on startup + every `discovery_interval` (30s default) | `AgentRegistry` watchers in every ork-api process; DevPortal sync (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)) |
| `ork.a2a.v1.discovery.gatewaycards` | Gateway card heartbeats | `gateway_id` | Each gateway (ADR [`0013`](0013-generic-gateway-abstraction.md)) | DevPortal, ork-api |
| `ork.a2a.v1.agent.request.<agent_id>` | Fire-and-forget delegation | `task_id` | `agent_call` tool (ADR [`0006`](0006-peer-delegation.md)) when `await: false` | `LocalAgent` running that `agent_id` |
| `ork.a2a.v1.agent.status.<task_id>` | Mid-task status events for one task | `task_id` | The agent executing the task | SSE bridges in ork-api ([`0008`](0008-a2a-server-endpoints.md)) and parent agents waiting on a delegated task |
| `ork.a2a.v1.agent.response.<client_id>` | Final task responses to fire-and-forget callers | `client_id` | The executing agent on completion | Caller agent / gateway |
| `ork.a2a.v1.push.outbox` | Push-notification delivery jobs | `task_id` | Agents on terminal `TaskState` transition | Push delivery worker (ADR [`0009`](0009-push-notifications.md)) |
| `ork.a2a.v1.trust.cards` | Trust attestations bound to broker identity (analog to SAM's trust topic) | `agent_id` | Onboarding flow | `Agent` middleware verifying remote calls (ADR [`0020`](0020-tenant-security-and-trust.md)) |

Per-task topics like `agent.status.<task_id>` use Kafka **compacted** topics with TTL ≤ 1 hour for the status case and 24 hours for `response.<client_id>`; the durable store of record is Postgres (`a2a_tasks`/`a2a_messages`, ADR [`0008`](0008-a2a-server-endpoints.md)).

### Message envelope on Kafka

Every Kafka A2A message carries the same JSON-RPC envelope used on the sync plane (ADR [`0003`](0003-a2a-protocol-model.md)) plus Kafka headers analogous to SAM's user properties:

| Kafka header | Equivalent SAM user property | Meaning |
| ------------ | ---------------------------- | ------- |
| `ork-a2a-version` | n/a | Wire-format version (`1.0`) |
| `ork-task-id` | `taskId` | A2A task id |
| `ork-context-id` | `contextId` | A2A conversation context id |
| `ork-reply-topic` | `replyTo` | Topic to publish the response to |
| `ork-status-topic` | `a2aStatusTopic` | Topic for status updates during streaming |
| `ork-tenant-id` | tenant in payload | Tenant scoping (ADR [`0020`](0020-tenant-security-and-trust.md)) |
| `ork-trace-id` | `traceparent` | W3C trace propagation |
| `ork-content-type` | `application/json` | Always JSON-RPC |

### Routing rules — when does a call go HTTP vs Kafka?

These rules live in the `Agent` resolution layer and are applied transparently by the registry:

1. **External A2A client → ork agent:** always HTTP through Kong. Kafka is internal.
2. **Local agent → local agent (same process):** direct in-process `Agent::send`. No transport.
3. **Local agent → local agent (different ork-api instance), `await: true`:** HTTP through Kong (still cheaper than building a Kafka request/reply correlator for sync flows).
4. **Local agent → local agent (different ork-api instance), `await: false`:** Kafka (`agent.request.<agent_id>`).
5. **Status update during a streaming task:** Kafka (`agent.status.<task_id>`); SSE clients subscribe via the bridge in ADR [`0008`](0008-a2a-server-endpoints.md).
6. **Discovery / heartbeats:** always Kafka.
7. **Push notification delivery:** Kafka outbox → HTTP POST to subscriber URL (ADR [`0009`](0009-push-notifications.md)).
8. **ork → external A2A agent (third party):** always HTTP (we don't share Kafka with the outside).

## Consequences

### Positive

- External A2A clients see ork as a vanilla A2A endpoint behind Kong; no broker proprietary transport required.
- The same operations that scale poorly over HTTP (discovery broadcast, mid-task status fan-out to many SSE listeners) move to Kafka where they belong.
- DevPortal becomes the single browse-and-discover surface, which matches the team's existing tooling story.
- Redis already in config gets a real job (correlation cache) instead of being dead weight.

### Negative / costs

- Two transports means two failure modes. Operators have to monitor both Kong and Kafka health for the mesh to be fully functional. ADR [`0022`](0022-observability.md) defines the dashboards.
- The "should this go HTTP or Kafka?" decision is wired into the registry; getting the rules wrong yields surprising latency/durability behaviour. The seven rules above are normative.
- SSE clients reconnecting need a replay cursor; we use Redis for the in-flight cache and Kafka offsets for the source of truth.
- Cross-process local-to-local sync goes HTTP → an extra hop vs SAM's broker. Acceptable at typical latencies; if it becomes hot, we can introduce a Kafka request/reply pattern later.

### Neutral / follow-ups

- ADR [`0005`](0005-agent-card-and-devportal-discovery.md) defines how DevPortal pulls and publishes from these topics.
- ADR [`0008`](0008-a2a-server-endpoints.md) defines the SSE bridge between Kafka status topics and HTTP clients.
- ADR [`0020`](0020-tenant-security-and-trust.md) defines the Kafka SASL/OAUTHBEARER and Kong mTLS posture.
- The `crates/ork-eventing` (new) crate owns Kafka producer/consumer wiring; depends on [`rskafka`](https://crates.io/crates/rskafka) (pure-Rust async Kafka client). We deliberately avoid `rdkafka` so the workspace stays cargo-only and does not require `librdkafka` at build time. The narrower feature surface (e.g. partition-0-only consume in the initial backend) is acceptable for Phase 1; multi-partition consumer work is a follow-up if/when load demands it.

## Alternatives considered

- **HTTP-only.** Rejected: forces every agent to long-poll or re-establish SSE for status fan-out and discovery; doesn't give us a durable event log for replay/push.
- **Kafka-only (SAM-style on Kafka).** Rejected: external A2A clients (browsers, vendor agents) cannot speak Kafka; we'd be reinventing HTTP gateways on top of every Kafka call. The spec wire format is HTTP, not topic-based.
- **NATS or RabbitMQ instead of Kafka.** Rejected: Kafka is the platform standard at the team and is already published through DevPortal; introducing a third broker is unjustified cost.
- **Per-agent dedicated SSE only, no Kafka status topic.** Rejected: blocks parent-task → child-task status streaming when they live in different processes (the common case for `delegate` in ADR [`0006`](0006-peer-delegation.md)).
- **Use Redis Streams instead of Kafka.** Rejected: Redis is fine as a short-lived cache but not as the durable mesh log; it doesn't match the team's standard for async cross-service messaging.

## Affected ork modules

- New crate: `crates/ork-eventing/` — Kafka producer/consumer, topic naming helpers (`crates/ork-a2a/src/topics.rs` declares the names; `ork-eventing` runs the I/O).
- New crate `crates/ork-cache/` (optional) wrapping Redis for SSE/replay; or extend [`crates/ork-common`](../../crates/ork-common/).
- [`config/default.toml`](../../config/default.toml) — add `[kafka]` section (brokers, security, namespace) and use the existing `[redis]` section.
- [`.env.example`](../../.env.example) — add `ORK__KAFKA__BROKERS`, `ORK__KAFKA__NAMESPACE`, `ORK__KAFKA__SECURITY_PROTOCOL`, `ORK__KAFKA__SASL_MECHANISM`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — wire Kafka producer/consumer at boot; remove or downgrade the in-process [`rate_limit_middleware`](../../crates/ork-api/src/middleware.rs) (Kong owns rate limiting now).
- New ops doc `docs/operations/kong-routes.md` and `docs/operations/kafka-topics.md` (out of scope for this ADR, follow-ups).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Solace topic namespace | [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) `get_a2a_base_topic` | Kafka namespace `ork.a2a.v1.*` |
| `replyTo`, `a2aStatusTopic` user properties | [`core_a2a/service.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/core_a2a/service.py) | Kafka headers `ork-reply-topic`, `ork-status-topic` |
| Discovery wildcard `discovery/>` | [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) `get_discovery_subscription_topic` | Kafka topics `discovery.agentcards`, `discovery.gatewaycards` |
| Per-agent request topic | `get_agent_request_topic` | `agent.request.<agent_id>` |
| Per-task status topic | `get_peer_agent_status_topic` | `agent.status.<task_id>` |
| Trust card topic | `.../trust/{component_type}/{component_id}` | `trust.cards` |

## Open questions

- Schema registry? **Yes** — JSON Schema (or Avro) for the JSON-RPC envelope and the Part union. Defer the registry choice to an ops follow-up.
- Kafka client choice: `rskafka` vs `rdkafka`? **Decided: `rskafka`** to avoid pulling `librdkafka` into the build. If we later need rdkafka-only features (transactional producers, idempotence, EOS) we revisit by adding a feature-flagged `rdkafka` backend behind the same `Producer`/`Consumer` traits owned by `crates/ork-eventing`.
- Per-tenant topic isolation vs. tenant-in-payload? **Default: tenant-in-payload** with Kafka ACLs scoping by namespace. A future ADR can introduce per-tenant topic prefixes if data residency demands it.
- Does the SSE replay window need to be persisted in Postgres for >24h durability? Defer until product asks for it.

## References

- [`future-a2a.md` §3–§4](../../future-a2a.md)
- A2A spec — JSON-RPC + SSE methods: <https://github.com/google/a2a>
- SAM topic helpers: [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py)
- Kong A2A reference patterns: <https://docs.konghq.com/>
- Kafka design — compacted topics + headers: <https://kafka.apache.org/documentation/>
