//! ADR-0055 AC #5 / ADR-0056 §`Streaming` — the rig adapter must
//! mirror tool calls onto the agent event stream so Studio's Chat
//! panel (via the auto-router's SSE encoder) can render a `tool_call`
//! chip. Regression test for the rig_engine.rs streaming-loop patch
//! that replaced the `ToolCall { .. } => {}` swallow with a
//! `tool_call_event(...)` emission.
//!
//! Shape asserted: at least one `TaskEvent::Message` whose `parts`
//! contain a `Part::Data { data: {"kind":"tool_call","id":...,
//! "name":..., "args":...} }`. The SSE encoder
//! (`ork_api::sse::encoder::data_part_event`) translates that data
//! shape into `event: tool_call` per ADR-0056.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role, TaskEvent};
use ork_agents::CodeAgent;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    ModelCapabilities, TokenUsage, ToolCall,
};
use ork_core::ports::tool_def::ToolDef;
use ork_tool::DynToolInvoke;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct ScriptedLlm {
    streams: Mutex<Vec<Vec<ChatStreamEvent>>>,
}

impl ScriptedLlm {
    fn new(streams: Vec<Vec<ChatStreamEvent>>) -> Self {
        Self {
            streams: Mutex::new(streams.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }
    async fn chat_stream(&self, _: ChatRequest) -> Result<LlmChatStream, OrkError> {
        let events = self
            .streams
            .lock()
            .await
            .pop()
            .expect("scripted stream exhausted");
        Ok(Box::pin(async_stream::stream! {
            for ev in events {
                yield Ok(ev);
            }
        }))
    }
    fn provider_name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn done(reason: FinishReason) -> ChatStreamEvent {
    ChatStreamEvent::Done {
        usage: TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        model: "stub".into(),
        finish_reason: reason,
    }
}

fn ctx() -> AgentContext {
    let tenant = TenantId::new();
    AgentContext {
        tenant_id: tenant,
        task_id: ork_a2a::TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: vec![],
            ..CallerIdentity::default()
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}

fn user_msg(task_id: ork_a2a::TaskId, text: &str) -> AgentMessage {
    AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: text.into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(task_id),
        context_id: None,
        metadata: None,
    }
}

fn counter_tool(counter: Arc<AtomicUsize>, payload: Value) -> Arc<dyn ToolDef> {
    Arc::new(DynToolInvoke::new(
        "stamp",
        "Echoes a fixed payload; bumps the call counter.",
        json!({"type":"object","properties":{}}),
        json!({"type":"object"}),
        Arc::new(move |_c, _i| {
            counter.fetch_add(1, Ordering::SeqCst);
            let payload = payload.clone();
            Box::pin(async move { Ok(payload) })
        }),
    ))
}

#[tokio::test]
async fn tool_call_streams_as_message_data_part_to_chat_panel() {
    // Two-turn script: turn 1 emits a tool call, turn 2 sends the
    // final assistant text. The rig adapter forwards the tool-call
    // event onto the agent event stream so the SSE encoder maps it
    // to `event: tool_call`.
    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "call_abc".into(),
                name: "stamp".into(),
                arguments: json!({"v": 42}),
            }),
            done(FinishReason::ToolCalls),
        ],
        vec![
            ChatStreamEvent::Delta("done".into()),
            done(FinishReason::Stop),
        ],
    ]));
    let calls = Arc::new(AtomicUsize::new(0));

    let agent = CodeAgent::builder("stamper")
        .description("Stamps a payload.")
        .instructions("Use the tool")
        .model("openai/gpt-4o-mini")
        .tool_dyn(counter_tool(calls.clone(), json!({"ok": true})))
        .llm(llm.clone())
        .build()
        .expect("build agent");

    let c = ctx();
    let task_id = c.task_id;
    let mut s = agent
        .send_stream(c, user_msg(task_id, "stamp it"))
        .await
        .expect("send_stream");

    let mut tool_call_events: Vec<AgentMessage> = Vec::new();
    while let Some(ev) = s.next().await {
        let ev = ev.expect("event ok");
        if let TaskEvent::Message(m) = ev {
            // Studio chip events carry a single Data part with
            // `kind: "tool_call"`.
            let is_tool_call = m.parts.iter().any(|p| {
                if let Part::Data { data, .. } = p {
                    data.get("kind").and_then(|k| k.as_str()) == Some("tool_call")
                } else {
                    false
                }
            });
            if is_tool_call {
                tool_call_events.push(m);
            }
        }
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the underlying tool must still run; the SSE event is a mirror, not a replacement"
    );
    assert_eq!(
        tool_call_events.len(),
        1,
        "expected exactly one `tool_call` Message on the stream, got {}",
        tool_call_events.len()
    );

    let msg = &tool_call_events[0];
    assert_eq!(msg.task_id, Some(task_id), "Message.task_id mirrors the run");
    assert_eq!(msg.role, Role::Agent, "tool_call comes from the agent side");
    let Part::Data { data, .. } = &msg.parts[0] else {
        panic!("expected Part::Data, got {:?}", msg.parts[0]);
    };
    assert_eq!(
        data.get("kind").and_then(|v| v.as_str()),
        Some("tool_call")
    );
    assert_eq!(
        data.get("id").and_then(|v| v.as_str()),
        Some("call_abc"),
        "tool_call id round-trips so Studio can correlate with a later tool_result"
    );
    assert_eq!(
        data.get("name").and_then(|v| v.as_str()),
        Some("stamp"),
        "tool name lights up the Chat panel chip"
    );
    assert_eq!(
        data.get("args"),
        Some(&json!({"v": 42})),
        "args round-trip verbatim so the chip can render the JSON the LLM produced"
    );
}
