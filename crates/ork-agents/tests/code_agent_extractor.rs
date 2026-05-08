//! ADR 0052 §`Structured output via rig::Extractor` — `output_schema::<O>()`
//! produces a `CodeAgent` whose terminal Message carries one `Part::Data`
//! deserializable into `O` (acceptance criterion §4).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent};
use ork_agents::CodeAgent;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    ModelCapabilities, TokenUsage, ToolCall,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize, JsonSchema, Debug, PartialEq)]
struct Forecast {
    high_f: f32,
    low_f: f32,
    summary: String,
}

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
    async fn chat_stream(&self, _request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        let events = self.streams.lock().await.pop().expect("scripted stream");
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
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
    }
}

fn user_msg(task_id: ork_a2a::TaskId) -> AgentMessage {
    AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: "Forecast for SF tomorrow.".into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(task_id),
        context_id: None,
        metadata: None,
    }
}

#[tokio::test]
async fn extractor_mode_emits_terminal_part_data_with_typed_payload() {
    // Turn 1: LLM calls `submit` with a Forecast-shaped payload.
    // Turn 2: LLM produces a short ack as final text — the consumer ignores
    // the text and substitutes Part::Data using the captured submit args.
    let submit_args = json!({
        "high_f": 68.0,
        "low_f": 54.0,
        "summary": "Mostly sunny with afternoon clouds."
    });
    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "call_submit".into(),
                name: "submit".into(),
                arguments: submit_args.clone(),
            }),
            done(FinishReason::ToolCalls),
        ],
        vec![
            ChatStreamEvent::Delta("done".into()),
            done(FinishReason::Stop),
        ],
    ]));

    let agent = CodeAgent::builder("forecaster")
        .description("Produces a structured weather forecast.")
        .instructions("Produce a structured forecast.")
        .model("openai/gpt-4o-mini")
        .output_schema::<Forecast>()
        .llm(llm)
        .build()
        .expect("build forecaster");

    let c = ctx();
    let task_id = c.task_id;
    let mut s = agent.send_stream(c, user_msg(task_id)).await.expect("ok");

    let mut data_part: Option<serde_json::Value> = None;
    let mut text_parts = 0usize;
    while let Some(ev) = s.next().await {
        if let AgentEvent::Message(m) = ev.expect("event ok") {
            assert_eq!(
                m.parts.len(),
                1,
                "extractor mode emits exactly one terminal part, got {}",
                m.parts.len()
            );
            for p in m.parts {
                match p {
                    Part::Data { data, .. } => data_part = Some(data),
                    Part::Text { .. } => text_parts += 1,
                    Part::File { .. } => panic!("file part unexpected in extractor mode"),
                }
            }
        }
    }
    assert_eq!(
        text_parts, 0,
        "extractor mode must not surface free-form text in the terminal Message"
    );
    let data = data_part.expect("terminal Part::Data missing");

    // Acceptance criterion: O::deserialize(value) succeeds.
    let forecast: Forecast =
        serde_json::from_value(data.clone()).expect("payload deserializes into Forecast");
    let expected = Forecast {
        high_f: 68.0,
        low_f: 54.0,
        summary: "Mostly sunny with afternoon clouds.".into(),
    };
    assert_eq!(forecast, expected);
}
