//! ADR 0052 — `CodeAgentBuilder::build()` enforces required fields.
//!
//! Acceptance criterion §3: id, instructions, and model (and the LLM provider)
//! must be set; each missing field returns `OrkError::Configuration` with a
//! specific, identifying message.

use std::sync::Arc;

use async_trait::async_trait;
use ork_agents::CodeAgent;
use ork_common::error::OrkError;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, LlmChatStream, LlmProvider, ModelCapabilities,
};

struct DummyLlm;

#[async_trait]
impl LlmProvider for DummyLlm {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!("required-field tests never invoke the LLM")
    }
    async fn chat_stream(&self, _: ChatRequest) -> Result<LlmChatStream, OrkError> {
        unreachable!("required-field tests never invoke the LLM")
    }
    fn provider_name(&self) -> &str {
        "dummy"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn assert_config_message(err: OrkError, must_contain: &[&str]) {
    let msg = match err {
        OrkError::Configuration { message } => message,
        other => panic!("expected OrkError::Configuration, got {other:?}"),
    };
    for fragment in must_contain {
        assert!(
            msg.contains(fragment),
            "expected message {msg:?} to contain {fragment:?}"
        );
    }
}

#[test]
fn empty_id_is_rejected() {
    let err = CodeAgent::builder("")
        .instructions("be helpful")
        .model("openai/gpt-4o-mini")
        .llm(Arc::new(DummyLlm))
        .build()
        .expect_err("empty id must fail");
    assert_config_message(err, &["id must not be empty"]);
}

#[test]
fn missing_instructions_is_rejected() {
    let err = CodeAgent::builder("agent-a")
        .model("openai/gpt-4o-mini")
        .llm(Arc::new(DummyLlm))
        .build()
        .expect_err("missing instructions must fail");
    assert_config_message(err, &["agent-a", "instructions"]);
}

#[test]
fn missing_model_is_rejected() {
    let err = CodeAgent::builder("agent-b")
        .instructions("be helpful")
        .llm(Arc::new(DummyLlm))
        .build()
        .expect_err("missing model must fail");
    assert_config_message(err, &["agent-b", "model"]);
}

#[test]
fn missing_llm_is_rejected() {
    let err = CodeAgent::builder("agent-c")
        .instructions("be helpful")
        .model("openai/gpt-4o-mini")
        .build()
        .expect_err("missing llm provider must fail");
    assert_config_message(err, &["agent-c", "llm"]);
}

#[test]
fn all_required_fields_set_builds_ok() {
    let agent = CodeAgent::builder("agent-d")
        .description("test")
        .instructions("be helpful")
        .model("openai/gpt-4o-mini")
        .llm(Arc::new(DummyLlm))
        .build()
        .expect("builder should succeed when required fields are set");
    assert_eq!(agent.id(), "agent-d");
}

#[test]
fn whitespace_only_id_is_rejected() {
    let err = CodeAgent::builder("   ")
        .instructions("be helpful")
        .model("openai/gpt-4o-mini")
        .llm(Arc::new(DummyLlm))
        .build()
        .expect_err("whitespace-only id must fail");
    assert_config_message(err, &["id must not be empty"]);
}

// Inline helper so the tests do not need to depend on `ork_core::ports::agent::Agent`
// directly (Agent is the trait CodeAgent implements; we only need its `id` here).
trait HasId {
    fn id(&self) -> &str;
}
impl HasId for ork_agents::CodeAgent {
    fn id(&self) -> &str {
        ork_core::ports::agent::Agent::id(self).as_str()
    }
}
