//! Integration smoke tests for [`ork_llm::router::LlmRouter`] (ADR 0012
//! §`Acceptance criteria` row "router_smoke.rs").
//!
//! These tests stand up a `wiremock` server per request and assert the
//! router's selection logic by inspecting which `Authorization` header
//! reaches the mock — each provider in the catalog gets a unique secret,
//! so the bearer token is a one-to-one fingerprint for the resolved
//! provider.
//!
//! Three properties are verified:
//!
//! 1. With no `provider` on the [`ChatRequest`], the operator
//!    [`LlmConfig::default_provider`] wins.
//! 2. A tenant-side `llm_providers[id]` entry replaces the operator
//!    catalog entry of the same id (id-collision-replaces, mirroring
//!    `mcp_servers` from ADR 0010).
//! 3. The **router-internal** three-level chain
//!    `ChatRequest.provider` → tenant default → operator default
//!    holds. The full ADR 0012 §`Selection` chain
//!    `WorkflowStep → AgentConfig → tenant → operator` is verified
//!    one layer up by the engine-level test in
//!    [`crates/ork-agents/tests/workflow_step_overrides_reach_llm.rs`](../../ork-agents/tests/workflow_step_overrides_reach_llm.rs);
//!    `LocalAgent::send_stream` is what collapses
//!    `WorkflowStep` and `AgentConfig` onto `ChatRequest.provider`
//!    before the request ever reaches the router.
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::config::{HeaderValueSource, LlmConfig, LlmProviderConfig, ModelCapabilitiesEntry};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::ResolveContext;
use ork_core::ports::llm::{ChatMessage, ChatRequest, LlmProvider};
use ork_llm::router::{LlmRouter, NoopTenantLlmCatalog, TenantLlmCatalog, TenantLlmCatalogEntry};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Stub `TenantLlmCatalog` returning a hard-coded entry for one tenant
/// and `None` for everyone else. Sufficient for these tests — the
/// production resolver is exercised by the `ork-api` integration tests.
struct StubTenantCatalog {
    entry: TenantLlmCatalogEntry,
    /// Only `tenant_id`s in this set get the entry; others see `None`.
    /// Empty ⇒ every tenant gets the entry (used by tests that don't
    /// care about the specific id).
    only_for: Option<TenantId>,
}

#[async_trait]
impl TenantLlmCatalog for StubTenantCatalog {
    async fn lookup(&self, tenant_id: TenantId) -> Result<Option<TenantLlmCatalogEntry>, OrkError> {
        match self.only_for {
            Some(target) if target != tenant_id => Ok(None),
            _ => Ok(Some(self.entry.clone())),
        }
    }
}

/// Mock OpenAI-compatible non-streaming chat completion response.
/// Returned as JSON so the [`OpenAiCompatibleProvider`]'s deserialiser
/// is exercised end-to-end; we only care that *a* response comes back so
/// the router's resolution path runs to completion.
fn ok_chat_body(model: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

fn provider_cfg_with_token(id: &str, base: &str, token: &str) -> LlmProviderConfig {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".into(),
        HeaderValueSource::Value {
            value: format!("Bearer {token}"),
        },
    );
    LlmProviderConfig {
        id: id.into(),
        base_url: base.into(),
        default_model: Some(format!("{id}-default")),
        headers,
        capabilities: vec![ModelCapabilitiesEntry {
            model: format!("{id}-default"),
            supports_tools: false,
            supports_streaming: true,
            supports_vision: false,
            max_context: Some(1024),
        }],
    }
}

fn ping_request(provider: Option<&str>) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::user("ping")],
        temperature: Some(0.0),
        max_tokens: Some(8),
        model: None,
        provider: provider.map(str::to_string),
        tools: Vec::new(),
        tool_choice: None,
    }
}

/// (a) With no `provider` in the request, the router resolves to
/// `LlmConfig::default_provider`.
#[tokio::test]
async fn no_provider_resolves_to_operator_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer op-default-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_chat_body("openai-default")))
        .expect(1)
        .mount(&server)
        .await;

    // A second mock that should not match — guards against accidental
    // fall-through to a different provider.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer op-other-key"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let cfg = LlmConfig {
        default_provider: Some("openai".into()),
        providers: vec![
            provider_cfg_with_token("openai", &server.uri(), "op-default-key"),
            provider_cfg_with_token("anthropic", &server.uri(), "op-other-key"),
        ],
    };
    let router =
        LlmRouter::from_config(&cfg, Arc::new(NoopTenantLlmCatalog)).expect("router builds");

    router
        .chat(ping_request(None))
        .await
        .expect("router resolves and the call succeeds");
}

/// (b) A tenant `llm_providers[id]` entry overrides the operator entry
/// with the same id. The mock is keyed on the tenant-side bearer; if the
/// router fell through to the operator entry, the request would 500.
#[tokio::test]
async fn tenant_provider_overrides_operator_with_same_id() {
    let op_server = MockServer::start().await;
    let tenant_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer tenant-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_chat_body("openai-default")))
        .expect(1)
        .mount(&tenant_server)
        .await;

    // Mounted on the operator server so we can detect a fall-through:
    // any request that reaches the operator base_url means the override
    // didn't take effect.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&op_server)
        .await;

    let cfg = LlmConfig {
        default_provider: Some("openai".into()),
        providers: vec![provider_cfg_with_token(
            "openai",
            &op_server.uri(),
            "operator-key",
        )],
    };
    let tenant_id = TenantId(Uuid::from_u128(0xa11ce));
    let tenant_catalog = StubTenantCatalog {
        entry: TenantLlmCatalogEntry {
            providers: vec![provider_cfg_with_token(
                "openai",
                &tenant_server.uri(),
                "tenant-key",
            )],
            default_provider: None,
            default_model: None,
        },
        only_for: Some(tenant_id),
    };

    let router = LlmRouter::from_config(&cfg, Arc::new(tenant_catalog)).expect("router builds");

    ResolveContext { tenant_id }
        .scope(async move {
            router
                .chat(ping_request(None))
                .await
                .expect("tenant override resolves");
        })
        .await;
}

/// (c) Router-internal three-level chain: `ChatRequest.provider` beats
/// the tenant default, which beats the operator default. This test
/// verifies what the router itself owns; the upstream "workflow step
/// override actually reaches the request" leg is verified by
/// `crates/ork-agents/tests/workflow_step_overrides_reach_llm.rs` per
/// ADR 0012 §`Selection`.
#[tokio::test]
async fn request_field_beats_tenant_default_beats_operator_default() {
    let server = MockServer::start().await;

    // One mock per provider entry; each expects exactly one hit. The
    // assertions are encoded in `.expect(1)` and the `Authorization`
    // header matcher.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer step-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_chat_body("step-default")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer tenant-default-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_chat_body("anthropic-default")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer operator-default-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_chat_body("openai-default")))
        .expect(1)
        .mount(&server)
        .await;

    // Operator catalog: openai (default) + anthropic + step-only entry.
    let cfg = LlmConfig {
        default_provider: Some("openai".into()),
        providers: vec![
            provider_cfg_with_token("openai", &server.uri(), "operator-default-key"),
            provider_cfg_with_token("anthropic", &server.uri(), "anthropic-op-key"),
            provider_cfg_with_token("step-only", &server.uri(), "step-key"),
        ],
    };
    let tenant_id = TenantId(Uuid::from_u128(0xb0b));
    let tenant_catalog = StubTenantCatalog {
        entry: TenantLlmCatalogEntry {
            // Tenant overrides anthropic's headers (so we can verify
            // the tenant-default code path) and points its default at
            // the overridden entry.
            providers: vec![provider_cfg_with_token(
                "anthropic",
                &server.uri(),
                "tenant-default-key",
            )],
            default_provider: Some("anthropic".into()),
            default_model: None,
        },
        only_for: Some(tenant_id),
    };
    let router =
        Arc::new(LlmRouter::from_config(&cfg, Arc::new(tenant_catalog)).expect("router builds"));

    // ChatRequest.provider wins: explicitly set to "step-only"
    // (catalog id unique to this request) ⇒ the step-key bearer
    // should reach the mock. Wrapped in a tenant scope to prove the
    // tenant default is shadowed by the request field. The step-only
    // id is named to match the engine-level test that drives this
    // field via `WorkflowStep.provider` upstream.
    let r = router.clone();
    ResolveContext { tenant_id }
        .scope(async move {
            r.chat(ping_request(Some("step-only")))
                .await
                .expect("step-level provider wins");
        })
        .await;

    // Tenant default wins: no `provider` on the request, but the
    // tenant entry has `default_provider = "anthropic"`.
    let r = router.clone();
    ResolveContext { tenant_id }
        .scope(async move {
            r.chat(ping_request(None))
                .await
                .expect("tenant default beats operator default");
        })
        .await;

    // Operator default wins: no tenant scope at all ⇒ the resolver
    // sees `ResolveContext::current() == None`, falls all the way
    // through to `cfg.default_provider`.
    router
        .chat(ping_request(None))
        .await
        .expect("operator default is the last fallback");
}
