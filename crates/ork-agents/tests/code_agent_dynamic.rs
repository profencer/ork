//! ADR 0052 — `dynamic_instructions / dynamic_model / dynamic_tools` resolve at
//! request entry from the per-request [`AgentContext`] (acceptance criterion §6).

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role};
use ork_agents::{CodeAgent, ModelSpec};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    ModelCapabilities, TokenUsage,
};
use ork_core::ports::tool_def::ToolDef;
use ork_tool::DynToolInvoke;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct ScriptedLlm {
    streams: Mutex<Vec<Vec<ChatStreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedLlm {
    fn new(streams: Vec<Vec<ChatStreamEvent>>) -> Self {
        Self {
            streams: Mutex::new(streams.into_iter().rev().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, OrkError> {
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
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn done() -> ChatStreamEvent {
    ChatStreamEvent::Done {
        usage: TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        model: "stub".into(),
        finish_reason: FinishReason::Stop,
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
        resource_id: None,
        thread_id: None,
    }
}

fn user_msg(task_id: ork_a2a::TaskId) -> AgentMessage {
    AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: "hi".into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(task_id),
        context_id: None,
        metadata: None,
    }
}

async fn drain<A: Agent>(agent: &A, ctx: AgentContext) -> Result<(), OrkError> {
    let task_id = ctx.task_id;
    let mut s = agent.send_stream(ctx, user_msg(task_id)).await?;
    while let Some(ev) = s.next().await {
        ev?;
    }
    Ok(())
}

#[tokio::test]
async fn dynamic_instructions_overrides_static_at_request_time() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::Delta("ok".into()),
        done(),
    ]]));

    let agent = CodeAgent::builder("triage")
        .description("Routes tickets.")
        .instructions("static prompt — should be ignored when dynamic is set")
        .model("openai/gpt-4o-mini")
        .dynamic_instructions(|ctx| {
            // Capture by value out of `ctx` to keep the future `'static`.
            let caller_tenant = ctx.caller.tenant_id;
            Box::pin(async move { format!("dynamic prompt for tenant {caller_tenant}") })
        })
        .llm(llm.clone())
        .build()
        .expect("build triage");

    drain(&agent, ctx()).await.expect("send_stream");

    let requests = llm.requests.lock().await;
    let last = requests.last().expect("one request issued");
    let preamble = last
        .messages
        .iter()
        .find(|m| matches!(m.role, ork_core::ports::llm::MessageRole::System))
        .map(|m| m.content.clone())
        .unwrap_or_default();
    assert!(
        preamble.starts_with("dynamic prompt for tenant "),
        "expected dynamic prompt, got {preamble:?}"
    );
}

#[tokio::test]
async fn dynamic_model_overrides_static_at_request_time() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![done()]]));

    let agent = CodeAgent::builder("router")
        .description("Routes by tier.")
        .instructions("be brief")
        .model("openai/gpt-4o-mini")
        .dynamic_model(|_ctx| {
            Box::pin(async move {
                // Use the resolver's input to choose a different model.
                ModelSpec::from("openai/gpt-4o")
            })
        })
        .llm(llm.clone())
        .build()
        .expect("build router");

    drain(&agent, ctx()).await.expect("send_stream");

    let requests = llm.requests.lock().await;
    let last = requests.last().expect("one request issued");
    assert_eq!(last.provider.as_deref(), Some("openai"));
    assert_eq!(last.model.as_deref(), Some("gpt-4o"));
}

#[tokio::test]
async fn dynamic_tools_appends_for_this_request() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![done()]]));

    let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let captured_for_resolver = captured.clone();

    let agent = CodeAgent::builder("toolful")
        .description("Has dynamic tools.")
        .instructions("do work")
        .model("openai/gpt-4o-mini")
        .dynamic_tools(move |_ctx| {
            let captured = captured_for_resolver.clone();
            let def: Arc<dyn ToolDef> = Arc::new(DynToolInvoke::new(
                "tenant-scoped",
                "Available only for this tenant.",
                json!({"type":"object","properties":{}}),
                json!({"type":"object"}),
                Arc::new(move |_c, _i| {
                    captured.lock().unwrap().push("invoked".into());
                    Box::pin(async move { Ok(Value::Null) })
                }),
            ));
            vec![def]
        })
        .llm(llm.clone())
        .build()
        .expect("build toolful");

    drain(&agent, ctx()).await.expect("send_stream");

    let requests = llm.requests.lock().await;
    let last = requests.last().expect("one request issued");
    let tool_names: Vec<&str> = last.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"tenant-scoped"),
        "the dynamic tool must be visible to the LLM in this request, got {tool_names:?}"
    );
}
