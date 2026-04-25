use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role};
use ork_agents::local::LocalAgent;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    TokenUsage, ToolCall,
};
use ork_core::workflow::engine::ToolExecutor;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct ScriptedLlm {
    streams: Mutex<Vec<Vec<ChatStreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        self.requests.lock().await.push(request);
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
}

struct EchoTools;

#[async_trait]
impl ToolExecutor for EchoTools {
    async fn execute(
        &self,
        _ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        Ok(json!({"tool": tool_name, "input": input}))
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

fn config() -> AgentConfig {
    AgentConfig {
        id: "writer".into(),
        name: "Writer".into(),
        description: "test".into(),
        system_prompt: "sys".into(),
        tools: vec!["list_repos".into()],
        model: None,
        temperature: 0.0,
        max_tokens: 100,
        max_tool_iterations: 16,
        max_parallel_tool_calls: 4,
        max_tool_result_bytes: 65_536,
        expose_reasoning: false,
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
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
    }
}

#[tokio::test]
async fn tool_loop_history_matches_openai_conventions() {
    let llm = Arc::new(ScriptedLlm {
        streams: Mutex::new(vec![
            vec![
                ChatStreamEvent::Delta("done".into()),
                done(FinishReason::Stop),
            ],
            vec![
                ChatStreamEvent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "list_repos".into(),
                    arguments: json!({}),
                }),
                done(FinishReason::ToolCalls),
            ],
        ]),
        requests: Mutex::new(Vec::new()),
    });
    let ctx = ctx();
    let agent = LocalAgent::new(
        config(),
        &CardEnrichmentContext::minimal(),
        llm.clone(),
        Arc::new(EchoTools),
    );
    let msg = AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: "hi".into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(ctx.task_id),
        context_id: None,
        metadata: None,
    };

    let mut stream = agent.send_stream(ctx, msg).await.expect("stream");
    while let Some(event) = stream.next().await {
        let _ = event.expect("event");
    }

    let requests = llm.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let history = &requests[1].messages;
    assert_eq!(history.len(), 4);
    assert_eq!(format!("{:?}", history[0].role), "System");
    assert_eq!(format!("{:?}", history[1].role), "User");
    assert_eq!(format!("{:?}", history[2].role), "Assistant");
    assert_eq!(history[2].tool_calls[0].id, "call_1");
    assert_eq!(format!("{:?}", history[3].role), "Tool");
    assert_eq!(history[3].tool_call_id.as_deref(), Some("call_1"));
}
