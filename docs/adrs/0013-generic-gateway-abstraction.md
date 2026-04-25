# 0013 — Generic Gateway abstraction

- **Status:** Implemented
- **Date:** 2026-04-24
- **Phase:** 3
- **Relates to:** 0002, 0003, 0004, 0008, 0014, 0015, 0017, 0021

## Context

ork's only ingress today is the JSON HTTP API in [`crates/ork-api/src/routes/`](../../crates/ork-api/src/routes/). After ADR [`0008`](0008-a2a-server-endpoints.md) it also exposes A2A endpoints. But to reach SAM parity ork must support the same family of "front doors" SAM has:

- Web UI chat (HTTP+SSE)
- REST API for arbitrary clients
- Slack
- Microsoft Teams (enterprise)
- Webhook receivers (incoming events from CI/CD, CRM, ticketing)
- Event-mesh ingress (Kafka topics from non-A2A producers)
- MCP gateway (expose ork agents as MCP tools to other LLM clients)

SAM solves this with a **`GatewayAdapter`** + **`GenericGatewayComponent`** pattern ([`gateway/generic/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/gateway/generic/component.py), [`gateway/adapter/base.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/gateway/adapter/base.py)) plus a beefier purpose-built [`gateway/http_sse`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway/http_sse) for the Web UI. Each gateway publishes a "gateway card" on discovery, terminates its own auth, transforms inbound requests into A2A `Message`s, and forwards events back out.

Without an analogous abstraction, every new ingress in ork becomes a custom `axum` Router + ad-hoc message conversion; we end up with N inconsistent integrations.

## Decision

ork **introduces a `Gateway` abstraction** plus a `GenericGatewayAdapter` trait that lets thin gateways (Slack, Teams, webhook, MCP-as-gateway) be implemented as small adapters without touching the core. The Web UI (ADR [`0017`](0017-webui-chat-client.md)) is treated separately because of its scale and statefulness.

### Trait

```rust
// crates/ork-core/src/ports/gateway.rs

#[async_trait::async_trait]
pub trait Gateway: Send + Sync {
    fn id(&self) -> &GatewayId;
    fn card(&self) -> &GatewayCard;          // analog of AgentCard for ingress
    async fn start(&self, deps: GatewayDeps) -> Result<(), OrkError>;
    async fn shutdown(&self) -> Result<(), OrkError>;
}

pub struct GatewayDeps {
    pub agent_registry: Arc<AgentRegistry>,
    pub a2a_repo: Arc<dyn A2aTaskRepository>,
    pub embed_resolver: Arc<EmbedResolver>,   // ADR 0015
    pub artifact_store: Arc<dyn ArtifactStore>,// ADR 0016
    pub kafka: Arc<KafkaProducer>,             // ADR 0004
    pub auth_resolver: Arc<dyn GatewayAuthResolver>,
    pub tracing: TracerHandle,                 // ADR 0022
}
```

A `Gateway` has full freedom to mount HTTP routes, run Kafka consumers, or hold long-lived WebSocket connections. It owns its own concurrency. It calls into ork through the trait surfaces it needs (`Agent::send_stream`, `ArtifactStore::*`, etc.) — it does **not** import private engine internals.

### `GenericGatewayAdapter` — the thin path

For 80% of gateways, the work is "translate request shape ↔ A2A `Message`". A `GenericGatewayAdapter` lets implementors skip the Gateway plumbing:

```rust
#[async_trait]
pub trait GenericGatewayAdapter: Send + Sync {
    fn id(&self) -> &GatewayId;
    fn card(&self) -> &GatewayCard;

    /// Optional axum Router contributed by the adapter (Slack/Teams/REST/webhook).
    fn http_routes(&self, deps: GatewayDeps) -> Option<Router>;

    /// Optional Kafka subscription contributed by the adapter (event-mesh ingress).
    fn kafka_subscriptions(&self) -> Vec<String>;

    /// Convert an inbound request into an A2A message + target agent.
    async fn translate_inbound(&self, raw: InboundRaw, ctx: GatewayCtx)
        -> Result<TranslatedRequest, OrkError>;

    /// Convert an outbound A2A event/message into the gateway's wire format.
    async fn translate_outbound(&self, ev: AgentEvent, ctx: GatewayCtx)
        -> Result<Vec<OutboundChunk>, OrkError>;
}

pub struct GenericGateway<A: GenericGatewayAdapter> {
    adapter: Arc<A>,
}
#[async_trait]
impl<A: GenericGatewayAdapter> Gateway for GenericGateway<A> { ... }
```

This is the analog of SAM's `GenericGatewayComponent` ([`gateway/generic/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/gateway/generic/component.py)).

### Gateway cards

Each gateway publishes a `GatewayCard` on Kafka topic `ork.a2a.v1.discovery.gatewaycards` (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)). DevPortal catalogs them so consumers can see "ork has a Slack gateway at workspace `acme`" without engineers asking.

### Bootstrapping

Gateways are loaded by `ork-api` (or a dedicated `ork-gateway` binary if scaled separately) from:

1. Static config — `[[gateways]]` blocks in [`config/default.toml`](../../config/default.toml).
2. Plugins — gateways may be supplied by ADR [`0014`](0014-plugin-system.md)'s plugin loader.

```toml
[[gateways]]
id = "slack-acme"
type = "slack"           # adapter id (built-in or plugin)
[gateways.slack-acme.config]
bot_token_env = "SLACK_BOT_TOKEN_ACME"
default_agent = "orchestrator"
```

The `type` field maps to a registered `GenericGatewayAdapter` factory. Adapters built into the core (REST, webhook, MCP-as-gateway) are always available; Slack/Teams/etc. ship as plugins.

### Built-in gateway adapters

| Adapter | Module | Purpose | Inbound | Outbound |
| ------- | ------ | ------- | ------- | -------- |
| `rest` | `crates/ork-gateways/src/rest.rs` | Generic REST → A2A bridge for clients that don't speak A2A | `POST /api/gateways/rest/{id}` JSON body → `Message` | JSON response, optional SSE |
| `webhook` | `crates/ork-gateways/src/webhook.rs` | Inbound webhooks from CI/CD, CRM, etc. — replaces today's hand-coded [`webhooks.rs`](../../crates/ork-api/src/routes/webhooks.rs) | `POST /api/gateways/webhook/{id}` with HMAC verification | Fire-and-forget; replies posted to push URL if configured |
| `event_mesh` | `crates/ork-gateways/src/event_mesh.rs` | Kafka topics from non-A2A producers | Subscribe to topic; map header rules → agent | Publish to outbound topic if configured |
| `mcp` | `crates/ork-gateways/src/mcp_gw.rs` | Expose ork agents as MCP tools to other LLM clients | MCP `tools/list`, `tools/call` over stdio or streamable-http | Convert A2A events into MCP responses |

### Plugin gateway adapters (ship as plugins per ADR [`0014`](0014-plugin-system.md))

| Adapter | Plugin | Purpose |
| ------- | ------ | ------- |
| `slack` | `ork-gateway-slack` | Slack workspace integration |
| `teams` | `ork-gateway-teams` | Microsoft Teams integration |

These mirror SAM's plugin gateways; we ship the plugin scaffolds in the [`workflow-templates/`](../../workflow-templates/) tree (renamed for plugins in ADR [`0014`](0014-plugin-system.md)).

### Auth across gateways

Each gateway terminates its own auth (Slack signing secrets, Teams Bot Framework JWTs, OAuth2 for REST), then resolves to an ork principal via `GatewayAuthResolver`:

```rust
#[async_trait]
pub trait GatewayAuthResolver: Send + Sync {
    async fn resolve(&self, claim: GatewayClaim) -> Result<RequestCtx, OrkError>;
}
```

Default impl maps gateway-specific identities to the configured tenant via a lookup table; advanced impls (DevPortal-backed) call DevPortal for OAuth2 token exchange. RBAC scopes (ADR [`0021`](0021-rbac-scopes.md)) are then evaluated against the resolved `RequestCtx`.

### Dynamic embeds

Gateways use the embed resolver (ADR [`0015`](0015-dynamic-embeds.md)) to substitute `«artifact_content:...»`, `«status_update:...»`, etc. in outbound messages before rendering them on Slack/Teams/etc. This matches SAM's gateway behaviour where `GenericGatewayComponent` resolves embeds before delivery.

## Consequences

### Positive

- One uniform front-door surface for every ingress; no more bespoke routes per integration.
- Gateways are plug-able without touching `ork-api` — the plugin system from ADR [`0014`](0014-plugin-system.md) ships ready-to-use gateways.
- DevPortal sees gateway cards and exposes them to platform consumers ("how do I send messages to ork from my Slack workspace?").
- Inbound webhooks get a real auth/HMAC story instead of the current public endpoint in [`webhooks.rs`](../../crates/ork-api/src/routes/webhooks.rs).

### Negative / costs

- Two abstractions layered (`Gateway`, `GenericGatewayAdapter`) — implementors need to know which to use. Documented in `0013` itself: use `GenericGatewayAdapter` unless you need long-lived state or non-axum I/O.
- The Web UI's heavy gateway path (ADR [`0017`](0017-webui-chat-client.md)) doesn't fit the generic adapter cleanly; we accept the asymmetry.
- HMAC/signing variants per provider are operational debt; we standardise on the providers' own libraries (`slack-morphism`, `azure-identity`).

### Neutral / follow-ups

- Today's [`crates/ork-api/src/routes/webhooks.rs`](../../crates/ork-api/src/routes/webhooks.rs) is migrated to the `webhook` gateway adapter; the legacy route stays for one minor version.
- The MCP gateway adapter is a useful symmetry: ork-as-MCP-server lets non-A2A LLM clients (Cursor, Claude Desktop) drive ork agents as tools.
- A future ADR may add a "gateway pool" pattern (multiple instances of the same gateway type for tenant isolation).

## Alternatives considered

- **Per-gateway crates with no shared trait.** Rejected: every gateway re-implements auth, embed resolution, agent resolution; massive code duplication.
- **Use SAM's exact `GenericGatewayComponent` shape.** Rejected: it's tied to SAC's app/component model. We extract the useful pieces (adapter pattern, embed resolution, gateway cards) without the SAC scaffolding.
- **Build only the Web UI gateway, defer Slack/Teams.** Rejected: Slack/Teams are in the parity scope and become trivial once the abstraction exists.
- **Treat MCP-as-gateway as part of the MCP ADR.** Rejected: the MCP-server side is an ingress (gateway) concern, not a tool-plane concern.

## Affected ork modules

- New crate: `crates/ork-core/src/ports/gateway.rs` — `Gateway`, `GenericGatewayAdapter`, `GatewayCard`, `GatewayDeps`.
- New crate: `crates/ork-gateways/` — built-in adapters: `rest.rs`, `webhook.rs`, `event_mesh.rs`, `mcp_gw.rs`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — load `[[gateways]]`, call `Gateway::start`.
- [`crates/ork-api/src/routes/mod.rs`](../../crates/ork-api/src/routes/mod.rs) — mount routes contributed by adapters under `/api/gateways/{id}/...`.
- [`crates/ork-api/src/routes/webhooks.rs`](../../crates/ork-api/src/routes/webhooks.rs) — deprecated; mapping to `webhook` gateway.
- [`config/default.toml`](../../config/default.toml) — `[[gateways]]` examples.
- ADR [`0014`](0014-plugin-system.md) defines how `ork-gateway-slack` and `ork-gateway-teams` plug in.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `BaseGatewayComponent` | [`gateway/base/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/gateway/base/component.py) | `Gateway` trait |
| `GenericGatewayComponent` | [`gateway/generic/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/gateway/generic/component.py) | `GenericGateway<A>` + `GenericGatewayAdapter` |
| `GatewayAdapter` | [`gateway/adapter/base.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/gateway/adapter/base.py) | `GenericGatewayAdapter` |
| Webhook gateway example | [`examples/gateways/webhook_gateway_example.yaml`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/examples/gateways/webhook_gateway_example.yaml) | `webhook` adapter |
| Event mesh gateway | [`examples/gateways/event_mesh_gateway_example.yaml`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/examples/gateways/event_mesh_gateway_example.yaml) | `event_mesh` adapter |
| MCP gateway | [`examples/gateways/mcp_gateway_example.yaml`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/examples/gateways/mcp_gateway_example.yaml) | `mcp_gw` adapter |
| Slack/Teams plugin gateways | core-plugins repos | `ork-gateway-slack`, `ork-gateway-teams` plugins (ADR [`0014`](0014-plugin-system.md)) |
| Gateway card discovery | [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) | Kafka `discovery.gatewaycards` (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)) |

## Open questions

- Do we want to support a single ork-api process running 50+ gateways, or do gateways scale by separate processes? Decision: support both; gateways are independent enough to scale separately when needed.
- Per-gateway tenant scoping — Slack workspace ↔ tenant mapping. Decision: declared in gateway config; `GatewayAuthResolver` is the extension point.

## Reviewer findings (code-reviewer, implementation pass)

- **Shutdown (addressed):** `discovery_cancel` and tombstone sleep now run before per-gateway `shutdown()` in `ork-api` so shared tokens stop background work before adapter hooks. Server error paths that skip cleanup remain an operational caveat.
- **Pipeline `202` (acknowledged + logging):** Legacy and gateway `workflow_trigger` modes intentionally return `202 Accepted` as fire-and-forget; `run_pipeline_webhook` now `warn!`s on `list_tenants` / tenant match / `list_definitions` failures so operators see silent no-ops in logs. Changing HTTP status would be a follow-up / separate ADR.
- **Gateway discovery after `start` (addressed):** `GatewayDiscoveryPublisher` tasks are spawned only after a successful `Gateway::start`.

## References

- SAM gateway base: <https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/gateway>
- Slack Bolt for Rust (`slack-morphism`): <https://github.com/abdolence/slack-morphism-rust>
