# 0007 — Remote agent client (`A2aRemoteAgent`)

- **Status:** Accepted
- **Date:** 2026-04-24
- **Phase:** 2
- **Relates to:** 0002, 0003, 0004, 0005, 0006, 0008, 0020

## Context

The `Agent` port from ADR [`0002`](0002-agent-port.md) gives ork a uniform abstraction for "an agent". So far we only have the in-process `LocalAgent` impl. To realise the full mesh we need a `dyn Agent` that talks to **remote** A2A endpoints — peers in another ork-api process, peers in another ork mesh, third-party A2A agents, vendor models. Without this, [`AgentRegistry::resolve(target)`](../../crates/ork-agents/src/registry.rs) returns nothing for any non-local agent and the delegation semantics from ADR [`0006`](0006-peer-delegation.md) cannot reach across processes.

SAM's analog is the [A2A proxy component](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/component.py) plus the [proxy config](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/config.py): a Solace-side adapter that forwards a peer A2A endpoint into the mesh as if it were a local SAM agent.

## Decision

ork **introduces `A2aRemoteAgent`**, an `Agent` impl that speaks A2A 1.0 JSON-RPC (HTTPS) and SSE to remote endpoints. It lives in a new module `crates/ork-integrations/src/a2a_client.rs` and depends on the type set from `crates/ork-a2a` (ADR [`0003`](0003-a2a-protocol-model.md)).

```rust
pub struct A2aRemoteAgent {
    id: AgentId,
    card: AgentCard,
    base_url: Url,                    // from card.url; Kong-fronted in cross-org case
    auth: A2aAuth,                    // bearer | api_key | oauth2_cc | oauth2_ac | mtls
    http: reqwest::Client,            // shared, with connection pool + timeouts
    transport_pref: TransportPref,    // Http | KafkaIfAvailable
    kafka: Option<Arc<KafkaProducer>>,// for transport-hint extension (ADR 0005)
}

#[async_trait]
impl Agent for A2aRemoteAgent {
    fn id(&self) -> &AgentId { &self.id }
    fn card(&self) -> &AgentCard { &self.card }

    async fn send(&self, ctx: AgentContext, msg: AgentMessage) -> Result<AgentMessage, OrkError> {
        let req = self.build_jsonrpc("message/send", MessageSendParams { message: msg });
        let resp: JsonRpcResponse<MessageSendResult> = self.post(req, &ctx).await?;
        resp.into_message()
    }

    async fn send_stream(&self, ctx: AgentContext, msg: AgentMessage)
        -> Result<BoxStream<'static, Result<AgentEvent, OrkError>>, OrkError>
    {
        let req = self.build_jsonrpc("message/stream", MessageSendParams { message: msg });
        let body = self.post_sse(req, &ctx).await?;
        Ok(parse_a2a_sse(body))   // emits TaskStatusUpdateEvent / TaskArtifactUpdateEvent / Message
    }

    async fn cancel(&self, ctx: AgentContext, task_id: &TaskId) -> Result<(), OrkError> {
        let req = self.build_jsonrpc("tasks/cancel", TaskIdParams { id: task_id.clone() });
        let _: JsonRpcResponse<Task> = self.post(req, &ctx).await?;
        Ok(())
    }
}
```

### Construction

`A2aRemoteAgent` is built from an `AgentCard` plus an `A2aClientConfig`:

```rust
pub struct A2aClientConfig {
    pub auth: A2aAuth,
    pub timeout: Duration,             // default 30s
    pub stream_idle_timeout: Duration, // default 5min
    pub retry: RetryPolicy,            // exponential backoff with jitter
    pub user_agent: String,            // "ork/<version>"
}

pub enum A2aAuth {
    None,
    StaticBearer(SecretString),
    StaticApiKey { header: String, value: SecretString },
    OAuth2ClientCredentials { token_url: Url, client_id: String, client_secret: SecretString, scopes: Vec<String> },
    OAuth2AuthorizationCode { token_provider: Arc<dyn TokenProvider> },
    Mtls { cert_path: PathBuf, key_path: PathBuf },
}
```

The auth variants intentionally mirror SAM's [`AuthenticationConfig`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/config.py) so we can copy known-good token caching logic.

### Registration paths

Three ways a `A2aRemoteAgent` can land in the registry:

1. **Static config** in [`config/default.toml`](../../config/default.toml):

   ```toml
   [[remote_agents]]
   id = "vendor.security_scanner"
   card_url = "https://scanner.vendor.example.com/.well-known/agent-card.json"
   auth = { type = "oauth2_client_credentials", token_url = "...", client_id_env = "...", client_secret_env = "..." }
   ```

   Loaded at boot, fetched once, then refreshed every `card_refresh_interval` (default 1h).

2. **Dynamic discovery via Kafka** (ADR [`0005`](0005-agent-card-and-devportal-discovery.md)). When the discovery subscriber sees a card whose `id` is not local and not already in the remote cache, it constructs an `A2aRemoteAgent` using **default auth from DevPortal** (the same OAuth2 client the local mesh uses internally). This is the common in-mesh path.

3. **Workflow-time bind**: a workflow YAML may inline a card URL:

   ```yaml
   - id: scan
     agent: { url: "https://scanner.vendor.example.com" }
     prompt_template: "Scan {{input.repo}}"
   ```

   The compiler ([`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)) resolves this to a transient `A2aRemoteAgent` registered for the run's lifetime only.

[`WorkflowStep.agent`](../../crates/ork-core/src/models/workflow.rs) becomes a [serde-untagged] enum to support both the bare-string and inline-card forms; the bare-string form is unchanged.

### Transport selection

Driven by the rules in ADR [`0004`](0004-hybrid-kong-kafka-transport.md), the client follows the agent's `AgentCard.extensions`:

- If the card carries the `https://ork.dev/a2a/extensions/transport-hint` extension with `kafka_request_topic` AND we have a Kafka producer AND the call is fire-and-forget (`await: false`): publish on the Kafka topic.
- Otherwise: HTTPS through Kong (or directly to the agent URL if it's outside our trust boundary).

### Streaming

`send_stream` uses an SSE parser that maps A2A `TaskStatusUpdateEvent`, `TaskArtifactUpdateEvent`, and final `Message` events into `AgentEvent`. Backpressure is handled by `reqwest`'s response body stream; the `BoxStream` we return is bounded by the channel size and drops oldest on overflow with a warn log (this matches A2A spec: clients can resync via `tasks/get`).

### Tenant scoping

`AgentContext.tenant_id` is propagated as the `X-Tenant-Id` header (or whatever header the card's `tenant-required` extension declares — ADR [`0005`](0005-agent-card-and-devportal-discovery.md)). For cross-org calls the remote agent may not honour this header; the local mesh nonetheless tags the outbound call with the originating tenant for billing and audit (ADR [`0022`](0022-observability.md)).

### Failure model

| Wire condition | Surfaced as |
| -------------- | ----------- |
| HTTP 4xx (non-429) | `OrkError::A2aClient(status, body)` → engine maps to `StepStatus::Failed` |
| HTTP 5xx | retried per `RetryPolicy`; surfaced as failure on retry exhaustion |
| HTTP 429 | retried with `Retry-After` honoured (capped at policy max) |
| TLS / connection error | retried as 5xx |
| JSON-RPC error (in 200 body) | `OrkError::A2aClient(rpc.error.code, rpc.error.message)` |
| SSE disconnect mid-stream | yield current `AgentEvent`s, then `Err(OrkError::A2aStreamLost)`; engine MAY follow up with `tasks/get` to recover state |
| Card fetch 404 | construction fails; agent not registered |

### Caching

Card fetches are cached in Redis (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)) keyed by URL with TTL = `card_refresh_interval`. OAuth2 tokens are cached in-process (per-client) with refresh at `expires_at - 60s`.

## Consequences

### Positive

- A workflow can target a remote agent without any new step kind; `agent: vendor.scanner` just works.
- The HTTP+SSE plane is the spec-compliant path, so we can reach any A2A-compliant peer (Google ADK, vendor agents, partner meshes) on day one.
- The Kafka transport hint lets in-mesh fire-and-forget delegation skip Kong without exposing it to outsiders.
- All five SAM auth variants are supported, easing integration with existing platform OAuth tokens.

### Negative / costs

- We own a JSON-RPC + SSE client. The wire format is stable per the A2A spec, but corner cases (chunked SSE, multiplexed events, server early-EOF) need real-world bake time. Plan: integration tests against the [A2A reference server](https://github.com/google/a2a) in CI.
- Token refresh logic is non-trivial; bugs can mask as 401 storms. Mitigation: surface `A2aRemoteAgent` health metrics in ADR [`0022`](0022-observability.md).
- Discovery-time auto-registration requires a default credential available to ork-api. ADR [`0020`](0020-tenant-security-and-trust.md) defines that credential model.

### Neutral / follow-ups

- ADR [`0008`](0008-a2a-server-endpoints.md) ensures the server we expose is round-trip compatible with this client (we should be able to call ourselves through it).
- ADR [`0009`](0009-push-notifications.md) extends `A2aRemoteAgent` to register push-notification configs upstream when the local caller asks for callback delivery.

## Alternatives considered

- **Build A2A on top of `octocrab`-style typed crates.** Rejected: no such crate covers the full A2A surface; we'd have to write it anyway.
- **Use a generic `tower` HTTP RPC abstraction.** Rejected: yields nothing over `reqwest` here and adds another layer to debug SSE issues through.
- **Skip the static-config path; require all remote agents come through discovery.** Rejected: cross-org / vendor agents are not on our Kafka discovery bus, so static config or workflow-time bind is required.
- **Treat remote agents as MCP servers.** Rejected: MCP is for tools, not for agents. The two protocols have different lifecycles (sync tool call vs. streaming task with state transitions).

## Affected ork modules

- New: [`crates/ork-integrations/src/a2a_client.rs`](../../crates/ork-integrations/src/) — `A2aRemoteAgent` impl.
- [`crates/ork-integrations/src/lib.rs`](../../crates/ork-integrations/src/) — re-export.
- [`crates/ork-agents/src/registry.rs`](../../crates/ork-agents/src/registry.rs) — accept `Arc<dyn Agent>` of any kind (already true after ADR [`0002`](0002-agent-port.md)).
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs) — accept inline-card form on `WorkflowStep.agent`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — load `[[remote_agents]]` from config, register them.
- [`config/default.toml`](../../config/default.toml) and [`.env.example`](../../.env.example) — `[[remote_agents]]` examples.
- New: integration tests against the A2A reference server (added to existing tests directory under `crates/ork-integrations/tests/`).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| A2A proxy component | [`agent/proxies/a2a/component.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/component.py) | `A2aRemoteAgent` |
| `AuthenticationConfig` | [`agent/proxies/a2a/config.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/proxies/a2a/config.py) | `A2aAuth` enum |
| Default `well_known_path` | same file | Card URL resolution path in `A2aRemoteAgent::from_card_url` |
| Token cache | proxy auth handler | In-process token cache keyed by client config |

## Open questions

- Do we want HTTP/2 or HTTP/1.1 for SSE? Decision: rely on `reqwest`'s default (HTTP/2 if negotiated, HTTP/1.1 fallback); both work for SSE.
- Should we expose `A2aRemoteAgent` directly to plugins (ADR [`0014`](0014-plugin-system.md))? Yes, via a `pub use ork_integrations::a2a_client::A2aRemoteAgent` re-export.

## References

- A2A reference implementation: <https://github.com/google/a2a>
- SAM proxy: <https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/agent/proxies/a2a>
- [`future-a2a.md` §4](../../future-a2a.md)
