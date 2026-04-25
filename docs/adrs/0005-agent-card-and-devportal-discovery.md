# 0005 — Agent Card publishing and DevPortal-backed discovery

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 1
- **Relates to:** 0002, 0003, 0004, 0006, 0013

## Context

Today, ork has no concept of agent capability discovery: the four hardcoded roles in [`AgentRole`](../../crates/ork-core/src/models/agent.rs) are wired in code, and [`AgentRegistry::list_agents`](../../crates/ork-agents/src/registry.rs) just returns the local `AgentConfig` blobs. There is no way for:

- An external A2A client to ask "what agents do you host and what can they do?";
- A peer agent in the mesh to find out which other agents exist before delegating ([`0006`](0006-peer-delegation.md));
- The DevPortal to list ork's agents alongside the team's other APIs and Kafka topics.

SAM solves this by publishing `AgentCard` JSON to the Solace topic `{ns}/a2a/v1/discovery/agentcards` and subscribing to the wildcard `{ns}/a2a/v1/discovery/>` ([`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py)). Cards have a TTL/last-seen recorded in [`common/agent_registry.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/agent_registry.py). For HTTP-callable A2A agents, the spec also requires a well-known endpoint — SAM's HTTP proxy defaults to `/.well-known/agent-card.json` ([proxy config](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/config.py)).

ork needs to do all three: HTTP well-known card (for spec compliance), Kafka discovery topic (for in-mesh fan-out, ADR [`0004`](0004-hybrid-kong-kafka-transport.md)), and DevPortal sync (for human/admin browsing and external publishing).

## Decision

ork **publishes Agent Cards on three surfaces** with the following authority:

| Surface | Authority | Purpose | Refresh |
| ------- | --------- | ------- | ------- |
| HTTP `/.well-known/agent-card.json` | per-agent runtime | Spec-compliant A2A discovery for any HTTP client | On-demand (computed on request) |
| Kafka topic `ork.a2a.v1.discovery.agentcards` | per-agent runtime | In-mesh discovery & cache invalidation across ork-api processes | Heartbeat every `discovery_interval` (default 30s) + on card change |
| DevPortal catalog | DevPortal | Human/admin browsing; external visibility for cross-team discovery; OAuth scope assignment | Push on card change; reconcile on a schedule |

**Source of truth:** the running agent owns its card. DevPortal is downstream — it cannot edit a card; it can only catalog it. The Kafka topic is the wire that DevPortal subscribes to.

### HTTP well-known endpoint

Mounted by ADR [`0008`](0008-a2a-server-endpoints.md) at:

```
GET /.well-known/agent-card.json                          (returns this ork-api's "default" agent or 404)
GET /a2a/agents/{agent_id}/.well-known/agent-card.json    (per-agent)
```

Implementation: handlers call `Agent::card()` (defined in ADR [`0002`](0002-agent-port.md)) and serialise. No DB hit; the card is built once at agent construction and stored on the trait object.

We pick `/.well-known/agent-card.json` (not `agent.json`) to align with SAM's HTTP A2A proxy default and with the A2A spec's most recent guidance. Both file names are served (the older `agent.json` returns the same payload) for one minor version, then dropped.

### Kafka discovery

Topic: `ork.a2a.v1.discovery.agentcards` (compacted, key = `agent_id`, retention by key = "infinite" semantically; per-message TTL via header).

Each ork-api process runs a `DiscoveryPublisher` background task per local agent:

- On boot: publish card with `header: ork-discovery-event = born`.
- Every `discovery_interval` (default 30s): publish `header: ork-discovery-event = heartbeat`.
- On graceful shutdown: publish a tombstone (null value, key = `agent_id`) with `header: ork-discovery-event = died`.
- On card change (e.g. plugin loaded a new tool, ADR [`0014`](0014-plugin-system.md)): publish immediately with `header: ork-discovery-event = changed`.

Each ork-api process also runs a `DiscoverySubscriber` consumer that updates the in-process [`AgentRegistry`](../../crates/ork-agents/src/registry.rs):

```rust
pub struct AgentRegistry {
    local: HashMap<AgentId, Arc<dyn Agent>>,        // LocalAgent + plugin agents
    remote: TtlCache<AgentId, RemoteAgentEntry>,    // populated from discovery
}

struct RemoteAgentEntry {
    card: AgentCard,
    last_seen: Instant,
    ttl: Duration,                  // 3 × discovery_interval
    transport_hint: TransportHint,  // HTTP url and/or Kafka request topic
}
```

A card is **expired** from the remote cache after `3 × discovery_interval` without a heartbeat. On expiry, in-flight delegations either complete (the request topic exists; the agent might just be heartbeat-skipped) or fail with `TaskState::Rejected`.

Gateway cards travel on a parallel topic `ork.a2a.v1.discovery.gatewaycards` with the same shape; they are surfaced separately by DevPortal because gateways are ingress points, not callable agents.

### DevPortal sync

DevPortal runs a long-lived consumer on `ork.a2a.v1.discovery.agentcards` and `discovery.gatewaycards`. It maps each card into:

- An **API** entry (the HTTP `/a2a/agents/{agent_id}` JSON-RPC endpoint, with the OpenAPI/JSON-Schema generated from the A2A spec — same schema for every agent).
- A **Topic** entry (the Kafka request topic `agent.request.<agent_id>` from ADR [`0004`](0004-hybrid-kong-kafka-transport.md)).
- An **Event** entry (the status topic `agent.status.<task_id>` template).

DevPortal entries inherit the agent's tags, skill descriptions, and `securitySchemes` so consumers can request OAuth scopes (ADR [`0021`](0021-rbac-scopes.md)) directly from the catalog.

**Bootstrap on cold start:** an ork-api process that comes up with an empty registry (e.g. before the Kafka subscription has caught up) calls `DevPortal /catalog/agents` once to seed the remote cache, then trusts the Kafka stream from there.

### Card content

The card's `url` field points to the **Kong-published** HTTPS URL, not the internal ork-api address. Example:

```json
{
  "name": "ork-planner",
  "description": "Plans multi-step DevOps workflows.",
  "version": "0.4.1",
  "url": "https://api.example.com/a2a/agents/planner",
  "provider": { "organization": "ork", "url": "https://devportal.example.com" },
  "capabilities": { "streaming": true, "push_notifications": true, "state_transition_history": true },
  "default_input_modes": ["text/plain", "application/json"],
  "default_output_modes": ["text/markdown", "application/json"],
  "skills": [
    { "id": "plan_change", "name": "Plan a change", "description": "...", "tags": ["devops", "planning"], "examples": ["Plan a database migration"] }
  ],
  "security_schemes": { "kong_oauth2": { "type": "oauth2", "flows": { "clientCredentials": { "tokenUrl": "https://devportal.example.com/oauth/token", "scopes": { "agent:planner:invoke": "Invoke the planner" } } } } },
  "security": [ { "kong_oauth2": ["agent:planner:invoke"] } ],
  "extensions": [
    { "uri": "https://ork.dev/a2a/extensions/transport-hint", "params": { "kafka_request_topic": "ork.a2a.v1.agent.request.planner" } },
    { "uri": "https://ork.dev/a2a/extensions/tenant-required", "params": { "header": "X-Tenant-Id" } }
  ]
}
```

Two ork-specific extension URIs are reserved by this ADR:

- `https://ork.dev/a2a/extensions/transport-hint` — exposes the Kafka request topic for callers who can speak Kafka (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)).
- `https://ork.dev/a2a/extensions/tenant-required` — declares which header carries the tenant id (ADR [`0020`](0020-tenant-security-and-trust.md)).

These mirror SAM's pattern of [extension URIs](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/utils.py) for `gateway-role` and `display-name`.

## Consequences

### Positive

- One card definition serves three surfaces; no duplication.
- DevPortal becomes the human entry point without becoming a single point of failure for the data plane (it consumes from Kafka; the mesh keeps working if DevPortal is down).
- TTL-based eviction handles agent crashes without operator intervention, exactly like SAM.
- The card's `extensions` field gives us a forward-compatible way to expose ork-specific capabilities (Kafka topic, tenant header) without breaking spec-strict A2A clients.

### Negative / costs

- Three publication surfaces means three failure modes; ADR [`0022`](0022-observability.md) defines the discovery freshness alarm.
- DevPortal must subscribe to Kafka, which adds a coupling between platform tooling and the event mesh. Acceptable because that coupling already exists for non-agent topics.
- Heartbeats add steady-state Kafka traffic proportional to agent count; at 30s interval and 1 KB cards, 1000 agents = ~33 KB/s of background traffic. Trivial for Kafka.

### Neutral / follow-ups

- ADR [`0006`](0006-peer-delegation.md) consumes `AgentRegistry::list_cards()` to populate the LLM's tool descriptions for delegation.
- ADR [`0013`](0013-generic-gateway-abstraction.md) reuses this discovery mechanism for gateway cards.
- A future ADR may introduce signed cards (JWS over the card payload) for trust beyond Kafka SASL identity (ADR [`0020`](0020-tenant-security-and-trust.md)).

## Alternatives considered

- **HTTP polling only.** Rejected: doesn't scale to many agents, doesn't handle "agent died" without aggressive polling, and forces every consumer to know every agent's URL.
- **DevPortal as the source of truth.** Rejected: control plane outages would silently break delegation. The agent itself owns its card.
- **Reuse the existing Postgres `tenants` table.** Rejected: tenant ≠ agent; tenants own agents but agent identity is not tenant-scoped.
- **Use Consul/etcd for discovery.** Rejected: adds a new piece of infra not on the team's standard list. Kafka + DevPortal already cover this.

## Affected ork modules

- New: `crates/ork-eventing/src/discovery.rs` — `DiscoveryPublisher` and `DiscoverySubscriber`.
- [`crates/ork-agents/src/registry.rs`](../../crates/ork-agents/src/registry.rs) — `local` + `remote` split, TTL cache, `list_cards()`.
- [`crates/ork-api/src/routes/a2a.rs`](../../crates/ork-api/src/routes/) (new in ADR [`0008`](0008-a2a-server-endpoints.md)) — well-known card handlers.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — start `DiscoveryPublisher` per local agent and a single `DiscoverySubscriber` per process.
- New: `crates/ork-devportal-sync/` (small crate or simple binary) — DevPortal-side consumer; lives in this repo for protocol alignment, deployable independently.
- [`config/default.toml`](../../config/default.toml) — `[discovery]` section: `interval_secs`, `ttl_multiplier`, `devportal_bootstrap_url`.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| Discovery topic for cards | [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) `get_agent_discovery_topic` | Kafka topic `ork.a2a.v1.discovery.agentcards` |
| TTL-cached registry | [`common/agent_registry.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/agent_registry.py) | `AgentRegistry::remote: TtlCache` |
| `/.well-known/agent-card.json` | [`agent/proxies/a2a/config.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/config.py) (default `well_known_path`) | HTTP route in `routes/a2a.rs` |
| Gateway-role extension | [`common/a2a/utils.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/utils.py) (`gateway-role`) | `https://ork.dev/a2a/extensions/transport-hint`, `tenant-required` |
| Display-name extension | [`peer_agent_tool.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/peer_agent_tool.py) | `AgentCard.name` (no extension needed) |

## Open questions

- Should ork ship a sentinel `ork-platform-discovery` agent whose only job is to expose the union catalog at `GET /a2a/agents` (a non-spec convenience endpoint)? Probably yes — defer until a consumer needs it.
- Do we sign cards with JWS so consumers can verify provenance offline? Defer to a security ADR after [`0020`](0020-tenant-security-and-trust.md).

## References

- A2A spec — Agent Card and discovery: <https://github.com/google/a2a>
- SAM `agent_registry.py`: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/agent_registry.py>
- SAM `protocol.py`: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py>
- [`future-a2a.md` §3, §6](../../future-a2a.md)
