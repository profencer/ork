# 0012 — OpenAI-compatible LLM provider catalog

- **Status:** Accepted
- **Date:** 2026-04-25
- **Deciders:** ork core
- **Phase:** 3
- **Relates to:** 0002, 0010, 0011, 0020
- **Supersedes:** —

## Context

ork ships exactly one LLM client implementation,
[`MinimaxProvider`](../../crates/ork-llm/src/minimax.rs), wired into
[`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) as the
single global LLM. Despite the name, it already speaks the OpenAI Chat
Completions wire format end-to-end (request shape, streaming SSE,
tool calls per ADR [`0011`](0011-native-llm-tool-calling.md)) and exposes
a configurable `base_url`. What it lacks is:

- The ability to talk to **more than one endpoint at a time** (planning
  agent on a strong model, summarisation agent on a cheap one).
- Anything beyond `Authorization: Bearer <key>` on the wire — the API
  key is a hardcoded header, not a generic auth surface.
- Per-tenant overrides — [`TenantSettings`](../../crates/ork-core/src/models/tenant.rs)
  has a single `llm_api_key_encrypted` field but tenants cannot pick
  *which* endpoint or override anything else.

The deployment shape that drives this ADR has the rest of the multi-LLM
problem already solved by infrastructure: a **GPUStack** cluster fronted
by **Kong** with auth/routing/quota plugins. Kong terminates whatever
auth scheme the upstream wants (Bearer, header API keys, mTLS) and
re-encodes outbound requests for OpenAI / Anthropic (via OpenAI-compat
shim) / Bedrock / vLLM / Ollama uniformly as the OpenAI Chat
Completions wire shape. From ork's point of view, **every** LLM
endpoint already looks like OpenAI; what differs between them is the
URL, the set of headers Kong wants, and the model id we pass through.

This ADR therefore deliberately rejects the "ship N native provider
clients in Rust" path the prior draft of 0012 took (Anthropic, Bedrock,
LiteLLM proxy as separate impls, cargo features, USD price table). All
of that is solved off-process by Kong + GPUStack, and re-implementing it
in ork would be expensive duplication that drifts from the gateway's
behaviour.

## Decision

ork **adopts a catalog of OpenAI-compatible LLM provider configurations**
addressed by id, with a single in-tree wire client (rewritten from
[`MinimaxProvider`](../../crates/ork-llm/src/minimax.rs)) that takes its
`base_url`, `default_model`, custom headers and per-model capabilities
from configuration rather than hardcoding any of them. Selection is by
**separate** `provider` + `model` fields on requests, agents and
workflow steps — no `<provider>/<model>` string parsing.

### Wire client — `OpenAiCompatibleProvider`

`crates/ork-llm/src/openai_compatible.rs` replaces
[`crates/ork-llm/src/minimax.rs`](../../crates/ork-llm/src/minimax.rs).
The streaming aggregator (`ToolCallAggregator`), SSE parser and
request/response serde stay byte-for-byte identical — only the
constructor surface changes:

```rust
pub struct OpenAiCompatibleProvider {
    id: String,
    client: reqwest::Client,
    base_url: String,
    default_model: Option<String>,
    headers: reqwest::header::HeaderMap,
    capabilities: HashMap<String, ModelCapabilities>,
    default_capabilities: ModelCapabilities,
}

impl OpenAiCompatibleProvider {
    pub fn from_config(cfg: ResolvedLlmProviderConfig) -> Result<Self, OrkError>;
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, OrkError>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<LlmChatStream, OrkError>;
    fn provider_name(&self) -> &str { &self.id }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.capabilities
            .get(model)
            .copied()
            .unwrap_or(self.default_capabilities)
    }
}
```

The hardcoded `Authorization: Bearer <api_key>` line in the current
client goes away. Every header is supplied by configuration; if Kong
wants `X-API-Key` or `X-Consumer-Username` or `apikey`, the operator
declares it. `MinimaxProvider` is **deleted**, not aliased — ork is
pre-1.0 with no production tenants and the misnomer is not worth
carrying.

Anthropic, Bedrock, LiteLLM and any other provider-specific Rust
client are explicitly **out of scope**. They are handled by Kong's
upstream route to a converter (an Anthropic-compatible Kong plugin, or
a vLLM/LiteLLM container behind Kong) and present to ork as just
another OpenAI-compatible URL.

### Provider catalog — operator + tenant, tenant wins on id collision

Operator catalog lives in `config/default.toml`:

```toml
[llm]
default_provider = "gpustack-fast"

[[llm.providers]]
id = "gpustack-fast"
base_url = "https://kong.example.com/llm/gpustack/fast/v1"
default_model = "qwen2.5-coder-32b"

[llm.providers.headers]
"X-API-Key" = { env = "GPUSTACK_FAST_KEY" }
"X-Consumer-Username" = { value = "ork" }

[[llm.providers.capabilities]]
model = "qwen2.5-coder-32b"
supports_tools = true
supports_streaming = true
max_context = 32768

[[llm.providers]]
id = "gpustack-strong"
base_url = "https://kong.example.com/llm/gpustack/strong/v1"
default_model = "claude-3-5-sonnet-latest"
[llm.providers.headers]
"X-API-Key" = { env = "GPUSTACK_STRONG_KEY" }
```

Each header value is one of `{ env = "VAR_NAME" }` (resolved at boot
from the process environment) or `{ value = "literal" }`. Modelled as
a serde `untagged` enum since the two variants have disjoint keys; no
discriminator field needed. The pattern of `*_env` indirection for
secrets is established by
[`crates/ork-common/src/mcp_config.rs`](../../crates/ork-common/src/mcp_config.rs)
and the `A2aAuthToml` enum in
[`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs).
No literal secret ever lives in the toml file.

Tenant catalog mirrors the same shape inside `TenantSettings`:

```rust
pub struct TenantSettings {
    // existing fields ...

    /// Tenant-scoped LLM provider catalog (ADR 0012). Entries with an
    /// `id` that collides with the operator catalog REPLACE the
    /// operator entry; non-colliding entries from both stacks merge.
    /// Mirrors the resolution rule used for `mcp_servers` (ADR 0010).
    #[serde(default)]
    pub llm_providers: Vec<TenantLlmProviderConfig>,

    /// Default `(provider_id, model)` for this tenant. Falls back to
    /// the operator's `[llm].default_provider` and the resolved
    /// provider's `default_model` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}
```

`TenantLlmProviderConfig` carries the same fields as the operator entry.
Header values ride in `TenantSettings.llm_providers` as **plaintext-at-rest
today**, sharing the same security property as the existing
`github_token_encrypted` / `gitlab_token_encrypted` fields
(see [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs)
and [`crates/ork-persistence/src/postgres/tenant_repo.rs`](../../crates/ork-persistence/src/postgres/tenant_repo.rs)
for the JSONB write path) — the `_encrypted` suffix on those legacy fields
is aspirational, not real. ADR
[`0020`](0020-tenant-security-and-trust.md) §`Tenant credentials` owns
AES-GCM-encrypting the whole tenant-secrets surface, including these new
fields, in one place. The legacy `llm_api_key_encrypted` field is
**removed** — there is no back-compat shim.

### Selection — separate `provider` + `model` fields

`ChatRequest` gains a `provider` field alongside the existing `model`:

```rust
pub struct ChatRequest {
    // existing fields ...
    pub provider: Option<String>,
    pub model: Option<String>,
}
```

The same pair lands on `AgentConfig` (which already carries
`model: Option<String>`; gains `provider: Option<String>`) and on
`WorkflowStep` (which gains both). Resolution order — first hit wins:

1. `WorkflowStep.provider` / `.model`
2. `AgentConfig.provider` / `.model`
3. `TenantSettings.default_provider` / `.default_model`
4. `[llm].default_provider`, then the resolved provider's `default_model`

Model strings are passed through verbatim to the provider's `model`
field on the OpenAI request — so HuggingFace-style ids that contain
`/` (e.g. `meta-llama/Meta-Llama-3.1-70B-Instruct`) work without
escaping. There is no `<provider>/<model>` parsing anywhere.

### Routing — `LlmRouter` is the global `LlmProvider`

`crates/ork-llm/src/router.rs` holds the catalog and itself implements
`LlmProvider`:

```rust
pub struct LlmRouter {
    /// Pre-resolved operator providers, keyed by id. Built once at boot
    /// from `LlmConfig::providers`; env-form headers are resolved
    /// eagerly so missing secrets fail the binary at boot.
    operator_providers: HashMap<String, Arc<OpenAiCompatibleProvider>>,
    /// Operator-side fallback selector mirrored from
    /// `LlmConfig::default_provider`.
    operator_default_provider: Option<String>,
    /// Tenant-side resolver; defaults to `NoopTenantLlmCatalog` when
    /// `ork-persistence` is not wired up (CLI binaries, unit tests).
    tenant_catalog: Arc<dyn TenantLlmCatalog>,
    /// Materialised tenant-override providers keyed on
    /// `(tenant_id, provider_id)`.
    tenant_provider_cache: RwLock<HashMap<CacheKey, Arc<OpenAiCompatibleProvider>>>,
}

#[async_trait]
impl LlmProvider for LlmRouter {
    async fn chat(&self, mut req: ChatRequest) -> Result<ChatResponse, OrkError> {
        let (provider, model) = self.resolve(&req).await?;
        req.model = model;
        provider.chat(req).await
    }
    // chat_stream is symmetric.

    fn provider_name(&self) -> &str { "router" }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // Sync entrypoint: cannot see the ResolveContext tenant, so
        // falls back to the operator default provider only.
        // Tenant-aware callers prefer the async `capabilities_for`.
        ModelCapabilities::default()
    }

    async fn capabilities_for(&self, request: &ChatRequest) -> ModelCapabilities {
        // Walks the same step → agent → tenant → operator chain
        // `chat_stream` does, so the caller gets the capabilities of
        // the provider that *would* actually answer the request.
        ModelCapabilities::default()
    }
}
```

`ResolveContext::current()` reads the per-task `tenant_id` from the
existing tokio task-local storage propagated via `AgentContext` (the
same context [`LocalAgent`](../../crates/ork-agents/src/local.rs) uses
today). The router merges the tenant catalog over the operator catalog
on every resolve — clients are constructed lazily and cached by `id`,
keyed on the **resolved** header set so a tenant override invalidates
only its own slot.

[`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) boots
`LlmRouter` in place of the bare `MinimaxProvider`; the
`Arc<dyn LlmProvider>` it hands to `LocalAgent` is the router. Engine
and agent code change zero lines — they keep talking to the trait.

### Capability negotiation

Capabilities are **declared in provider config** (per the toml example
above) rather than baked into provider impls, because the same wire
client now serves many physical backends. `[[llm.providers.capabilities]]`
entries map `model -> ModelCapabilities`; the provider's
`default_capabilities` (which itself defaults to "tools on, streaming
on, vision off, max_context unknown") covers any model not listed.
[`LocalAgent`](../../crates/ork-agents/src/local.rs) already calls
`capabilities()` before sending a tools catalog (ADR
[`0011`](0011-native-llm-tool-calling.md)) — that path keeps working
unchanged; the data behind it just gets richer.

## Acceptance criteria

- [ ] `crates/ork-llm/src/openai_compatible.rs` exists and exports
      `OpenAiCompatibleProvider` with the constructor and `LlmProvider`
      impl shown in `Decision`.
- [ ] `crates/ork-llm/src/minimax.rs` is deleted; the streaming
      aggregator and SSE parser move with the rewrite (no behaviour
      change for the OpenAI wire shape).
- [ ] `crates/ork-llm/src/router.rs` exists and exports `LlmRouter`
      implementing `LlmProvider`; `cargo test -p ork-llm router::` is
      green.
- [ ] `LlmConfig` in [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs)
      carries `default_provider: Option<String>` and
      `providers: Vec<LlmProviderConfig>`; the `provider`, `base_url`
      and `model` top-level fields are removed.
- [ ] `LlmProviderConfig` parses `headers` as a map of header-name to
      `{ env = "..." } | { value = "..." }` (serde `untagged` enum;
      the two variants have disjoint keys so no discriminator is
      needed). Header names are case-preserving.
- [ ] `LlmProviderConfig` parses an optional
      `[[llm.providers.capabilities]]` array of
      `{ model, supports_tools, supports_streaming, supports_vision, max_context }`.
- [ ] `TenantSettings` (`crates/ork-core/src/models/tenant.rs`) carries
      `llm_providers: Vec<TenantLlmProviderConfig>`,
      `default_provider: Option<String>`, `default_model: Option<String>`,
      and **does not** carry `llm_api_key_encrypted`.
- [ ] `ChatRequest`, `AgentConfig`, `WorkflowStep` each carry
      `provider: Option<String>`; `WorkflowStep` additionally carries
      `model: Option<String>` (`AgentConfig.model` already exists).
- [ ] [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs)
      constructs `LlmRouter` from `config.llm`, registers it as the
      global `Arc<dyn LlmProvider>`, and the env var `MINIMAX_API_KEY`
      is no longer read.
- [ ] `crates/ork-llm/tests/router_smoke.rs` covers: (a) request with
      no `provider` resolves to `default_provider`; (b) tenant
      `llm_providers[id]` overrides the operator entry of the same id;
      (c) precedence order (step → agent → tenant → operator) holds.
- [ ] `crates/ork-llm/tests/openai_compatible_headers.rs` asserts
      that custom headers from config are sent on `chat()` and
      `chat_stream()` requests (verified against a mock HTTP server).
- [ ] [`config/default.toml`](../../config/default.toml) `[llm]`
      section is rewritten to the catalog shape shown in `Decision`,
      with at least one example `[[llm.providers]]` entry commented out.
- [ ] [`README.md`](README.md) ADR index row updated to reflect the
      new title.
- [ ] [`metrics.csv`](metrics.csv) row appended after the ADR ships.

## Consequences

### Positive

- Tenants can target multiple physical model backends without ork
  shipping any provider-specific Rust code.
- Auth surface is open-ended: anything Kong (or any other reverse
  proxy) wants in headers is expressible without a code change. Kong
  consumer keys, mTLS termination + a header echo, OAuth-introspected
  bearer tokens, internal `X-Tenant-Id` propagation — all configuration.
- The wire impl is one file, one client. The aggregator and SSE parser
  that already work on the OpenAI shape (and were proven against
  Minimax / Kong-fronted GPUStack in development) stop being
  Minimax-coloured.
- Per-(provider, model) capability declarations let operators stop the
  agent loop from sending tool catalogs to local models that can't
  handle them, without a model-detection patch in the wire client.

### Negative / costs

- Operators carry the burden of declaring provider entries correctly:
  a wrong header name silently fails as `401`/`403` from upstream.
  The router logs the resolved provider id + redacted header **names**
  on every request to make this debuggable.
- The catalog is loaded at boot and on tenant-settings updates; there
  is no hot reload of operator-side `[[llm.providers]]` entries
  without a process restart. Acceptable for ops, called out so it is
  not a surprise later.
- Capabilities are declarative, not introspected from the upstream.
  An operator who upgrades a model behind Kong but forgets to update
  `[[llm.providers.capabilities]]` will get stale capability answers.
  The mitigation is documentation, not code.
- ork loses any cost-attribution telemetry it might otherwise have
  computed in-process. This is by design — Kong (or whatever fronts
  Kong) owns metering and chargeback. ADR
  [`0022`](0022-observability.md) records token usage from the
  provider response; converting that to USD is **not** ork's job.
- **Tenant header values are plaintext-at-rest in
  `tenants.settings.llm_providers` until ADR
  [`0020`](0020-tenant-security-and-trust.md) lands.** This matches
  the status quo for `github_token_encrypted` / `gitlab_token_encrypted`
  in the same JSONB blob — the `_encrypted` suffix on those fields is
  aspirational; the only real AES-GCM helper today lives in
  [`crates/ork-push/src/encryption.rs`](../../crates/ork-push/src/encryption.rs)
  and only wraps push signing keys. ADR 0020 owns hardening this
  surface uniformly; this ADR consumes whatever that ADR ships rather
  than rolling a one-off encryption scheme for LLM headers.

### Neutral / follow-ups
- A future ADR can add native non-OpenAI clients (`AnthropicProvider`,
  `BedrockProvider`) as additional `LlmProvider` impls registered in
  the router under their own `id`. This ADR neither precludes nor
  schedules that work.
- Vision / image input depends on `Part::File` plumbing (ADR
  [`0003`](0003-a2a-protocol-model.md)) and the artifact pipeline
  (ADR [`0016`](0016-artifact-storage.md)); orthogonal to provider
  selection.

## Alternatives considered

- **Ship native Anthropic + Bedrock + LiteLLM clients in Rust (the
  prior 0012 draft).** Rejected: Kong + GPUStack already does
  protocol conversion off-process. Re-implementing it in ork doubles
  the surface area, drifts from the gateway behaviour, and forces
  per-provider feature flags / dependency bloat
  (`aws-sdk-bedrockruntime` alone is non-trivial). The prior draft is
  preserved in git history if the deployment shape ever changes.
- **Single endpoint, no catalog (Kong is the only router).** Rejected:
  workflows already need to address "the strong model" vs. "the cheap
  model" by name, and routing that through a single Kong route forces
  the model id to encode the routing decision. Two named providers
  with two `base_url`s is closer to the operational reality and keeps
  Kong's per-route plugins (rate limit, quota, circuit breaker)
  meaningful.
- **Combined `provider/model` string (LiteLLM / OpenRouter convention).**
  Rejected: HuggingFace model ids legitimately contain `/`
  (`meta-llama/Meta-Llama-3.1-70B-Instruct`), so any split-on-first-slash
  scheme is a footgun that surfaces as confusing errors only at
  runtime. Two fields are slightly more verbose in workflow YAML and
  unambiguous.
- **Keep `llm_api_key_encrypted` as a no-op back-compat field.**
  Rejected: ork is pre-1.0 with no production tenants. Carrying a
  dead field permanently in the tenant JSONB blob, and the
  conditional logic to ignore it, costs more than the migration it
  avoids.

## Affected ork modules

- [`crates/ork-llm/`](../../crates/ork-llm/) — new
  `openai_compatible.rs` + `router.rs`; delete `minimax.rs`.
- [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs)
  — `ChatRequest.provider: Option<String>`. The `LlmProvider` trait
  itself is unchanged; `capabilities()` already exists from ADR 0011.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs)
  — add `llm_providers`, `default_provider`, `default_model`; remove
  `llm_api_key_encrypted` and the matching `UpdateTenantSettingsRequest`
  field.
- [`crates/ork-core/src/models/agent.rs`](../../crates/ork-core/src/models/agent.rs)
  — `AgentConfig.provider: Option<String>` next to the existing
  `model`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — `WorkflowStep.provider: Option<String>`, `WorkflowStep.model: Option<String>`.
- [`crates/ork-common/src/config.rs`](../../crates/ork-common/src/config.rs)
  — replace `LlmConfig { provider, base_url, model }` with the catalog
  shape (`default_provider`, `providers: Vec<LlmProviderConfig>`).
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) —
  boot `LlmRouter` instead of `MinimaxProvider`; drop the
  `MINIMAX_API_KEY` env-var read.
- [`config/default.toml`](../../config/default.toml) — rewrite the
  `[llm]` section.
- SQL: `TenantSettings` is stored as JSONB; the schema is
  forward-compatible (additive fields), but a one-shot UPDATE drops
  the dead `llm_api_key_encrypted` key from existing rows. The
  implementation session is responsible for the SQL.

## Reviewer findings

Captured from the `code-reviewer` subagent pass on the implementation
diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Critical | Tenant LLM provider headers stored unencrypted at rest, contradicting the ADR | **Rejected, false premise.** The legacy `github_token_encrypted` / `gitlab_token_encrypted` fields are also plaintext today — the `_encrypted` suffix is aspirational. ADR [`0020`](0020-tenant-security-and-trust.md) owns AES-GCM-encrypting the whole tenant-secrets surface, including these new fields. ADR text corrected to drop the false "AES-GCM scheme already used" claim. |
| Critical | Workflow-step `provider`/`model` overrides defined but never reach the LLM call | **Fixed.** `WorkflowStep.{provider, model}` now propagate through `WorkflowNode.{step_provider, step_model}` → `execute_agent_step` → `AgentContext.step_llm_overrides` → `LocalAgent::send_stream`, where they shadow `AgentConfig.{provider, model}` on the `ChatRequest` the router sees. |
| Critical | "step → tenant → operator" precedence test was mislabeled and only covered three of four levels | **Fixed.** Router test renamed to `request_field_beats_tenant_default_beats_operator_default` (its honest router-internal scope); a new engine-level test [`crates/ork-agents/tests/workflow_step_overrides_reach_llm.rs`](../../crates/ork-agents/tests/workflow_step_overrides_reach_llm.rs) drives a real `WorkflowDefinition` through the engine and asserts the bearer reaching the mock matches the step-level provider, not the agent's. |
| Major | `demo/config/default.toml` `[llm]` block still used the deleted flat shape — `make demo` would boot with an empty catalog | **Fixed.** Rewritten to the catalog shape with `default_provider = "minimax"` and a `[[llm.providers]]` entry pointing at `MINIMAX_API_KEY`. |
| Major | `.env.example` documented dead `ORK__LLM__*` keys | **Fixed.** Dead keys removed; comment now points at this ADR for the catalog shape and `MINIMAX_API_KEY` stays as the demo's required secret. |
| Major | Demo scripts and `demo/README.md` framed minimax as "the only wired LLM provider" | **Fixed.** Reworded to "the demo's `default_provider` is `minimax` and reads its key from `MINIMAX_API_KEY`; swap in any OpenAI-compatible endpoint by editing `[[llm.providers]]`." |
| Major | `make demo` stage 4 returned `401 Unauthorized` from Minimax even with `MINIMAX_API_KEY` set, because the deleted `MinimaxProvider` used to wrap the env var in `format!("Bearer {key}")` and the new generic `OpenAiCompatibleProvider` sends header values verbatim. Caught after the first end-to-end demo run on the post-review build, not by the `code-reviewer` pass. | **Fixed (docs-only).** `.env.example`, `demo/README.md`, `demo/config/default.toml` and the stage-4 skip text now document that header values are sent verbatim and `MINIMAX_API_KEY` MUST contain the literal `Bearer …` Authorization value. The stage-4 script also warns at runtime when the env var is set but missing the prefix. The alternative — re-introducing a `template`/concat variant on `HeaderValueSource` — was rejected as feature creep on a freshly-accepted ADR; documented for the next ADR-0012 follow-up if the footgun bites again. |
| Major | `LlmRouter::capabilities(&self, model)` silently lies for tenant-overridden providers (the agent loop's tool-call gate could read stale data) | **Fixed.** `LlmProvider` trait grew `async fn capabilities_for(&self, request: &ChatRequest)`; default impl delegates to the sync `capabilities`, `LlmRouter` overrides it to walk the same step → agent → tenant → operator chain `chat_stream` does. `LocalAgent::send_stream` calls `capabilities_for` instead of the sync `capabilities`. The sync method's docstring now warns that for routers it's operator-default-only. |
| Major | No row appended to `docs/adrs/metrics.csv` for ADR 0012; status still `Proposed` | **Fixed.** Status flipped to `Accepted`; metrics row appended; `docs/adrs/README.md` row updated. |
| Minor | `ResolveContext::scope` only wraps the `chat_stream(request)` future; comment could mislead a future reader | **Fixed.** Inline comment in [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs) now explicitly calls out that this is sound only because `LlmRouter::resolve` runs synchronously inside `chat_stream`. |
| Minor | Router model-precedence comment was half-true while Critical #2 was open | **Fixed.** The `WorkflowStep.model` half is now actually wired (Critical #2 fix), so the comment matches reality. |
| Minor | `tenant_provider_cache` has no upper bound; long-lived processes leak memory proportional to `(#tenants × #unique providers)` | **Deferred.** Documented under §`Negative / costs` (no hot-reload surface); `TODO(ADR-0012-followup): bound or LRU` left next to the field. |
| Minor | `ork-core` newly depends on `ork-common::config::LlmProviderConfig` for a domain model (`TenantLlmProviderConfig` is a type alias) | **Acknowledged, deferred.** Coupling is currently fine because `LlmProviderConfig` is pure data. If/when ADR 0020's encryption work introduces a separate `EncryptedTenantLlmProviderConfig`, that newtype lives in `ork-core::models::tenant` and the operator-side type stays in `ork-common`. |
| Minor | `parse_params` etc. clippy fixes (`unwrap_or_else(ContextId::new)` → `unwrap_or_default()`, `.max(1).min(500)` → `.clamp(1, 500)`, `#[allow(clippy::result_large_err)]`) | **Acknowledged.** Behaviour-equivalent (`ContextId::default()` is `ContextId::new()`); kept as-is per the reviewer's own note. |
| Nit | `CacheKey` rationale comment oversold the "header-hash needs eviction" argument | **Fixed.** Rationale tightened in [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs). |
| Nit | `openai_compatible.rs` claimed reqwest "case-preserves" custom header keys; HTTP/2 normalisation actually lowercases them | **Fixed.** Doc comment in [`crates/ork-llm/src/openai_compatible.rs`](../../crates/ork-llm/src/openai_compatible.rs) drops the "case-preserved" claim and reframes as case-insensitive matching. |
| Nit | ADR `LlmRouter` struct sketch (two fields) drifted from the four-field implementation | **Fixed.** Sketch above updated to match the implementation. |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Solace Agent Mesh | [`pyproject.toml`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/pyproject.toml) `litellm` dependency | `LlmRouter` over a catalog of `OpenAiCompatibleProvider`; protocol conversion stays in Kong / GPUStack rather than in-process |
| LiteLLM | provider routing inside the proxy | Out-of-process: Kong route + plugins. ork talks one wire shape. |
| OpenRouter | `<vendor>/<model>` string convention | Rejected; ork uses separate `provider` + `model` fields. |

## Open questions

- Do we need a per-tenant default capability override (e.g. tenant on
  a constrained Kong route forbids tool calls regardless of model)?
  Defer; out-of-band Kong policy is the operator answer for now.
- Should the router cache HTTP clients keyed on the **resolved**
  header set, or per `(tenant_id, provider_id)`? The implementation
  session picks the cheaper one once it has the call shape in hand.

## References

- OpenAI Chat Completions API:
  <https://platform.openai.com/docs/api-reference/chat>
- OpenAI tool calling:
  <https://platform.openai.com/docs/guides/function-calling>
- GPUStack: <https://github.com/gpustack/gpustack>
- Kong API Gateway: <https://docs.konghq.com/>
- ADR [`0011`](0011-native-llm-tool-calling.md) — `LlmProvider` tool-call surface
- ADR [`0010`](0010-mcp-tool-plane.md) — same operator+tenant catalog
  resolution rule used for MCP servers
- ADR [`0020`](0020-tenant-security-and-trust.md) — the authoritative
  source for tenant-secret encryption that this ADR consumes
