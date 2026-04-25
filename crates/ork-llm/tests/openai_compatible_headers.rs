//! Header-propagation tests for [`ork_llm::openai_compatible::OpenAiCompatibleProvider`]
//! (ADR 0012 §`Acceptance criteria` row "openai_compatible_headers.rs").
//!
//! Verifies that every name/value pair the catalog declares — both
//! `env`-resolved and literal — reaches the wire on both `chat()` and
//! `chat_stream()`. Header *names* are case-preserved per the catalog
//! comment (the wire client maps `BTreeMap<String, _>` straight into
//! `reqwest`'s header builder, which keeps custom keys verbatim).
#![allow(clippy::expect_used)]

use std::collections::HashMap;

use futures::StreamExt;
use ork_common::config::ModelCapabilitiesEntry;
use ork_core::ports::llm::{ChatMessage, ChatRequest, ChatStreamEvent, LlmProvider};
use ork_llm::openai_compatible::OpenAiCompatibleProvider;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ping_request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::user("ping")],
        temperature: Some(0.0),
        max_tokens: Some(8),
        model: None,
        provider: None,
        tools: Vec::new(),
        tool_choice: None,
    }
}

fn provider_with_headers(server: &MockServer) -> OpenAiCompatibleProvider {
    let mut headers = HashMap::new();
    // Authorization: simulates the env-resolved variant; the router
    // would have populated this from `HeaderValueSource::Env` at boot.
    headers.insert("Authorization".into(), "Bearer secret-token".into());
    // X-Trace-Tag: simulates the literal variant.
    headers.insert("X-Trace-Tag".into(), "ork-edge".into());
    // x-tenant-id: lower-case custom header to prove case is preserved
    // (BTreeMap<String, _> ⇒ reqwest keeps the key verbatim).
    headers.insert("x-tenant-id".into(), "t-42".into());
    OpenAiCompatibleProvider::new(
        "openai",
        server.uri(),
        Some("gpt-test".to_string()),
        headers,
        vec![ModelCapabilitiesEntry {
            model: "gpt-test".into(),
            supports_tools: false,
            supports_streaming: true,
            supports_vision: false,
            max_context: Some(1024),
        }],
    )
}

#[tokio::test]
async fn chat_sends_all_configured_headers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer secret-token"))
        .and(header("X-Trace-Tag", "ork-edge"))
        .and(header("x-tenant-id", "t-42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_with_headers(&server);
    let resp = provider
        .chat(ping_request())
        .await
        .expect("chat() with all configured headers reaches the mock");
    assert_eq!(resp.content, "ok");
}

#[tokio::test]
async fn chat_stream_sends_all_configured_headers() {
    let server = MockServer::start().await;

    // Minimal SSE stream: one delta + one terminator. The wire client's
    // SSE parser only cares that we end on `[DONE]`. Body is the OpenAI
    // chat-completions chunk shape.
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl\",\"object\":\"chat.completion.chunk\",\
         \"created\":0,\"model\":\"gpt-test\",\
         \"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl\",\"object\":\"chat.completion.chunk\",\
         \"created\":0,\"model\":\"gpt-test\",\
         \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer secret-token"))
        .and(header("X-Trace-Tag", "ork-edge"))
        .and(header("x-tenant-id", "t-42"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_with_headers(&server);
    let mut stream = provider
        .chat_stream(ping_request())
        .await
        .expect("chat_stream() with all configured headers reaches the mock");

    // Drain the stream so the assertion fires from `Mock::expect` on
    // server drop. We don't need to inspect the content beyond proving
    // the stream terminates cleanly.
    let mut got_done = false;
    while let Some(ev) = stream.next().await {
        if let Ok(ChatStreamEvent::Done { .. }) = ev {
            got_done = true;
        }
    }
    assert!(
        got_done,
        "stream should terminate with a Done event when the SSE [DONE] sentinel arrives"
    );
}
