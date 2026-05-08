//! ADR 0052 §`request_context_schema` — `request_context_schema::<C>()`
//! produces a JSON Schema readable by ADR-0056's OpenAPI emitter
//! (acceptance criterion §7).
//!
//! Snapshot semantics: we don't fix the entire schemars output (it's stable
//! enough but not contract-tested), instead we assert the *load-bearing
//! shape*: top-level "type": "object", every declared field present in
//! "properties", and required fields surfaced in "required".

use std::sync::Arc;

use async_trait::async_trait;
use ork_agents::CodeAgent;
use ork_common::error::OrkError;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, LlmChatStream, LlmProvider, ModelCapabilities,
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SupportRequestContext {
    /// Tier slug used by `dynamic_model` to pick the right LLM.
    tier: String,
    /// Optional per-call locale; defaults to the tenant's locale.
    locale: Option<String>,
}

struct DummyLlm;

#[async_trait]
impl LlmProvider for DummyLlm {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }
    async fn chat_stream(&self, _: ChatRequest) -> Result<LlmChatStream, OrkError> {
        unreachable!()
    }
    fn provider_name(&self) -> &str {
        "dummy"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

#[test]
fn request_context_schema_surfaces_load_bearing_shape() {
    let agent = CodeAgent::builder("triage")
        .description("Routes support tickets.")
        .instructions("Be concise.")
        .model("openai/gpt-4o-mini")
        .request_context_schema::<SupportRequestContext>()
        .llm(Arc::new(DummyLlm))
        .build()
        .expect("build triage");

    let schema = agent
        .request_context_schema()
        .expect("schema set by builder method");

    let obj = schema.as_object().expect("top-level object");
    let properties = obj
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("properties object present");
    assert!(
        properties.contains_key("tier"),
        "tier field surfaced in properties: {schema}"
    );
    assert!(
        properties.contains_key("locale"),
        "locale field surfaced in properties: {schema}"
    );

    let required: Vec<&str> = obj
        .get("required")
        .and_then(|v| v.as_array())
        .map(|v| v.iter().filter_map(|s| s.as_str()).collect())
        .unwrap_or_default();
    assert!(
        required.contains(&"tier"),
        "non-Option `tier` must appear in required, got {required:?}"
    );
    assert!(
        !required.contains(&"locale"),
        "Option<String> `locale` must NOT appear in required, got {required:?}"
    );
}

#[test]
fn request_context_schema_is_none_when_not_set() {
    let agent = CodeAgent::builder("triage")
        .description("Routes support tickets.")
        .instructions("Be concise.")
        .model("openai/gpt-4o-mini")
        .llm(Arc::new(DummyLlm))
        .build()
        .expect("build triage");
    assert!(agent.request_context_schema().is_none());
}
