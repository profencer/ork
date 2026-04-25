# 0012 — Multi-LLM provider abstraction

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 3
- **Relates to:** 0002, 0011, 0020, 0021

## Context

ork ships with exactly one LLM provider implementation, [`MinimaxProvider`](../../crates/ork-llm/src/minimax.rs), wired into [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) as the global LLM. Per-tenant credentials are partially modeled — [`TenantSettings.llm_api_key_encrypted`](../../crates/ork-core/src/models/tenant.rs) exists — but tenants cannot pick **which** provider, only the key for the single configured one. There is no abstraction over providers.

SAM solves this by integrating [`litellm`](https://github.com/BerriAI/litellm) as a passthrough router; tenants/agents specify model strings like `openai/gpt-4o` or `anthropic/claude-sonnet` and SAM routes accordingly.

For ork to:

- Let tenants use their preferred LLM vendor;
- Run different models per agent (planning agent on a strong model, summarisation agent on a cheap one);
- Avoid lock-in to Minimax;

we need a real provider abstraction. ADR [`0011`](0011-native-llm-tool-calling.md) already extended the [`LlmProvider`](../../crates/ork-core/src/ports/llm.rs) trait to support tool calling. This ADR ships multiple impls.

## Decision

ork **adopts a multi-provider model** with three impls in-tree at first cut and a per-tenant routing layer.

### Providers shipped

| Provider | Crate module | Wire compatibility | Auth |
| -------- | ------------ | ------------------ | ---- |
| **OpenAI-compatible** (covers OpenAI, Azure OpenAI, Minimax, vLLM, LM Studio, OpenRouter) | `crates/ork-llm/src/openai.rs` | OpenAI Chat Completions + tool calls | Bearer API key + `base_url` |
| **Anthropic** | `crates/ork-llm/src/anthropic.rs` | Messages API + tool use | API key |
| **AWS Bedrock** | `crates/ork-llm/src/bedrock.rs` | InvokeModel / Converse API for Claude / Titan / Llama on Bedrock | AWS sigv4 |

`MinimaxProvider` is **rewritten as a thin wrapper** around `OpenAiCompatibleProvider` with the Minimax base URL hardcoded — preserving backwards compatibility for existing callers in [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs).

A fourth optional impl, `LiteLlmProvider`, points at a self-hosted [LiteLLM proxy](https://github.com/BerriAI/litellm) and acts as a catch-all router for vendors we don't natively support. It is registered only when `[llm.litellm]` is configured.

### Provider selection — `LlmRouter`

A new `LlmRouter` lives in `crates/ork-llm/src/router.rs` and itself implements `LlmProvider`:

```rust
pub struct LlmRouter {
    providers: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    default: ProviderId,
}

impl LlmRouter {
    pub fn resolve(&self, request: &ChatRequest, ctx: &ResolveContext) -> ResolvedRequest;
}

#[async_trait]
impl LlmProvider for LlmRouter {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, OrkError> {
        let resolved = self.resolve(&req, &ResolveContext::current()?);
        self.providers[&resolved.provider].chat(resolved.request).await
    }
    // chat_stream similar
    fn provider_name(&self) -> &str { "router" }
}
```

`ChatRequest.model` is parsed as `<provider>/<model_id>`:

- `"openai/gpt-4o-mini"` → `providers["openai"]`, model `gpt-4o-mini`
- `"anthropic/claude-3-5-sonnet-latest"` → `providers["anthropic"]`
- `"bedrock/anthropic.claude-3-5-sonnet-20240620-v1:0"` → `providers["bedrock"]`
- `"minimax/abab6.5-chat"` → `providers["minimax"]` (legacy mapping; rewritten to `openai/...` with Minimax base_url)
- bare `"gpt-4o"` (no provider prefix) → router's `default`

### Per-tenant credential resolution

`TenantSettings` is **extended**:

```rust
pub struct TenantSettings {
    // legacy single-key (kept for backcompat)
    pub llm_api_key_encrypted: Option<String>,
    pub github_token_encrypted: Option<String>,
    pub gitlab_token_encrypted: Option<String>,
    pub gitlab_base_url: Option<String>,
    pub default_repos: Vec<String>,

    // NEW: per-provider credentials
    pub llm_credentials: HashMap<ProviderId, EncryptedSecret>,

    // NEW: per-provider config (e.g. Azure deployment name, OpenRouter base URL)
    pub llm_provider_config: HashMap<ProviderId, serde_json::Value>,

    // NEW: tenant default model
    pub default_model: Option<String>,

    // NEW: per-agent model overrides (agent_id -> model string)
    pub agent_models: HashMap<AgentId, String>,

    // NEW: MCP servers (ADR 0010)
    pub mcp_servers: Vec<McpServerConfig>,
}
```

`ResolveContext::current()` reads the request's tenant from the per-task context (`AgentContext.tenant_id`) propagated via tokio task-local storage. The router merges:

1. Tenant `llm_credentials[provider_id]` if set.
2. Tenant `llm_provider_config[provider_id]` (e.g. base URL override).
3. Global config from [`config/default.toml`](../../config/default.toml) `[llm.providers.<id>]`.

Encryption-at-rest reuses the AES-GCM scheme already used by the existing `*_encrypted` fields ([`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs)); the legacy `llm_api_key_encrypted` field is treated as an implicit `llm_credentials["minimax"]` entry during a one-version deprecation window.

### Per-agent model

[`AgentConfig`](../../crates/ork-core/src/models/agent.rs) gains an optional `model: Option<String>`. Resolution precedence:

1. `WorkflowStep.model` if set (per-step override; new optional field).
2. `TenantSettings.agent_models[agent_id]` if set (tenant override).
3. `AgentConfig.model` (agent's natural default).
4. `TenantSettings.default_model` (tenant default).
5. Router's `default`.

### Capability negotiation

Not all models support tool calls (e.g. some local models). Each provider implements:

```rust
fn capabilities(&self, model: &str) -> ModelCapabilities;
```

returning `{ supports_tools, supports_streaming, supports_vision, max_context }`. `LocalAgent` (ADR [`0002`](0002-agent-port.md)) checks `supports_tools` before sending a tool-laden request and either degrades gracefully (no tools) or fails with a clear error if tools are required.

### Cost / token accounting

`TokenUsage` stays per-response, but the router emits a `LlmUsageEvent` to ADR [`0022`](0022-observability.md)'s metrics with `(tenant_id, provider, model, input_tokens, output_tokens, est_usd)`. The `est_usd` field is computed from a static price table loaded from `config/llm_prices.toml` (separate file so updates don't require a code release).

### Feature flags / build size

To keep `cargo build --release` cheap for users who only use one provider, providers behind cargo features:

```toml
# crates/ork-llm/Cargo.toml
[features]
default = ["openai"]
openai = []
anthropic = []
bedrock = ["aws-config", "aws-sdk-bedrockruntime"]
litellm = []
```

The default deployment in [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) enables `["openai", "anthropic"]`.

## Consequences

### Positive

- Tenants pick their model per agent; ork stops dictating a single vendor.
- Tool calling (ADR [`0011`](0011-native-llm-tool-calling.md)) gets a uniform implementation across providers.
- The OpenAI-compatible impl covers a long tail of providers via `base_url` overrides without provider-specific code.
- Cost telemetry per tenant becomes possible.

### Negative / costs

- Three provider implementations to maintain; each provider's tool-call wire format differs subtly (Anthropic uses `input_schema`/`tool_use` blocks; OpenAI uses `parameters`/`tool_calls` arrays).
- Cargo build matrix grows with feature combinations; CI must test the meaningful ones.
- The price table is operational debt — out-of-date prices yield wrong cost telemetry.

### Neutral / follow-ups

- A future ADR may add a streaming usage event for per-token billing.
- Ollama / local-only models will land via the OpenAI-compatible impl with `base_url` pointing at an Ollama OpenAI-compat endpoint; no extra impl needed.
- ADR [`0021`](0021-rbac-scopes.md) defines `model:<provider>:<model>:invoke` scopes for fine-grained access control.

## Alternatives considered

- **Use a single LiteLLM proxy and only ship one provider in Rust.** Rejected: adds a required external service and an extra hop for the most common path (OpenAI-compatible).
- **Build provider-specific traits (e.g. `OpenAiProvider`, `AnthropicProvider`) instead of one trait.** Rejected: the engine and `LocalAgent` would need provider-specific code paths, defeating the abstraction.
- **Stay single-provider; let tenants stand up their own LiteLLM if they need variety.** Rejected: bad UX, no cost reporting, and locks tenants out of vendor-specific features (e.g. Anthropic's reasoning mode).
- **Adopt `async-openai` and similar crates rather than write provider clients ourselves.** We may; this ADR doesn't preclude it. Decision: try `async-openai` for the OpenAI-compatible impl; write the Anthropic one ourselves (its API surface is small).

## Affected ork modules

- [`crates/ork-llm/`](../../crates/ork-llm/) — `openai.rs`, `anthropic.rs`, `bedrock.rs`, `router.rs`, `litellm.rs`; rewrite [`minimax.rs`](../../crates/ork-llm/src/minimax.rs) as a thin alias.
- [`crates/ork-core/src/ports/llm.rs`](../../crates/ork-core/src/ports/llm.rs) — `capabilities(&str) -> ModelCapabilities`.
- [`crates/ork-core/src/models/tenant.rs`](../../crates/ork-core/src/models/tenant.rs) — extended `TenantSettings`.
- [`crates/ork-core/src/models/agent.rs`](../../crates/ork-core/src/models/agent.rs) — `AgentConfig.model: Option<String>`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) — `WorkflowStep.model: Option<String>`.
- [`crates/ork-api/src/main.rs`](../../crates/ork-api/src/main.rs) — boot `LlmRouter` in place of bare `MinimaxProvider`.
- [`config/default.toml`](../../config/default.toml) — `[llm.providers.openai]`, `[llm.providers.anthropic]`, `[llm.providers.bedrock]` sections.
- New: `config/llm_prices.toml` (or `[llm.prices]` block).
- SQL: tenant settings JSONB schema is forward-compatible (new fields are additive); no migration needed beyond updating the encoding helpers.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `litellm` integration | `pyproject.toml` dependency `litellm` | `LiteLlmProvider` (optional) + native OpenAI/Anthropic/Bedrock |
| Per-app model config | YAML `model:` in agent template | `AgentConfig.model` + per-tenant overrides |
| Platform model API | [`services/platform/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/services/platform) | DevPortal for catalog; `LlmRouter` for runtime |

## Open questions

- Do we cache provider clients per tenant or share across tenants? Decision: share clients; tenant-specific creds plumbed through `ResolveContext`. This avoids exploding HTTP connection counts.
- Vision/image input — depends on `Part::File` flow (ADR [`0003`](0003-a2a-protocol-model.md)). Defer until artifact pipeline (ADR [`0016`](0016-artifact-storage.md)) lands.
- Local Ollama via OpenAI-compatible impl: seems to work today; verify in integration tests.

## References

- OpenAI tool calling: <https://platform.openai.com/docs/guides/function-calling>
- Anthropic tool use: <https://docs.anthropic.com/claude/docs/tool-use>
- AWS Bedrock Converse API: <https://docs.aws.amazon.com/bedrock/latest/userguide/conversation-inference.html>
- LiteLLM: <https://github.com/BerriAI/litellm>
