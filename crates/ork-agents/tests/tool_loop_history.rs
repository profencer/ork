use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role};
use ork_agents::local::LocalAgent;
use ork_agents::tool_catalog::ToolCatalogBuilder;
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
use ork_core::ports::tool_def::ToolDef;
use ork_core::workflow::engine::ToolExecutor;
use ork_tool::DynToolInvoke;
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

/// Tool executor that returns one scripted error per call, then succeeds.
/// Used to prove the agent loop converts recoverable tool errors into tool
/// results (per ADR 0010 §`Failure model`) instead of aborting the step.
struct FailFirstTools {
    pending_errors: Mutex<Vec<OrkError>>,
}

#[async_trait]
impl ToolExecutor for FailFirstTools {
    async fn execute(
        &self,
        _ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let mut errs = self.pending_errors.lock().await;
        if !errs.is_empty() {
            return Err(errs.remove(0));
        }
        Ok(json!({"tool": tool_name, "input": input}))
    }
}

fn catalog_list_repos(backing: Arc<dyn ToolExecutor>) -> ToolCatalogBuilder {
    let mut m = HashMap::new();
    let b = backing.clone();
    let def: Arc<dyn ToolDef> = Arc::new(DynToolInvoke::new(
        "list_repos",
        "List configured source repositories available to this tenant.",
        json!({"type": "object", "properties": {}}),
        json!({"type": "object"}),
        Arc::new(move |ctx, input| {
            let b = b.clone();
            Box::pin(async move { b.execute(&ctx, "list_repos", &input).await })
        }),
    ));
    m.insert("list_repos".into(), def);
    ToolCatalogBuilder::new().with_native_tools(Arc::new(m))
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
        provider: None,
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
    let agent = LocalAgent::new(config(), &CardEnrichmentContext::minimal(), llm.clone())
        .with_tool_catalog(catalog_list_repos(Arc::new(EchoTools)));
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
    // Rig-derived requests include preamble as a leading system message plus the multi-turn slice;
    // assert OpenAI-ish ordering without pinning an exact structural length.
    assert!(
        history.len() >= 4,
        "expected full tool round-trip history; got len {}: {history:?}",
        history.len()
    );

    let assistant_with_call = history
        .iter()
        .find(|m| {
            format!("{:?}", m.role) == "Assistant" && m.tool_calls.iter().any(|c| c.id == "call_1")
        })
        .expect("assistant message with tool call");
    assert_eq!(assistant_with_call.tool_calls[0].id, "call_1");

    let tool_msg = history
        .iter()
        .find(|m| format!("{:?}", m.role) == "Tool" && m.tool_call_id.as_deref() == Some("call_1"))
        .expect("tool result for call_1");
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
}

/// Regression test for the `review (failed): validation error: agent_call:
/// missing required field `prompt`` demo failure.
///
/// Per ADR 0010 §`Failure model` ("tool-call errors stay in the tool result
/// (LLM can retry); transport / connection errors bubble up as step
/// failures"), a tool call that returns a `Validation` /
/// `Integration` / `LlmProvider` / `NotFound` error MUST be surfaced back to
/// the LLM as a tool-role message containing the error payload, not abort
/// the agent step. Otherwise an LLM that emits a malformed `agent_call(...)`
/// (or any other tool call with bad args) on its first try kills the entire
/// workflow step instead of getting a chance to self-correct on the next
/// iteration.
#[tokio::test]
async fn tool_validation_error_feeds_back_to_llm_as_tool_result() {
    let llm = Arc::new(ScriptedLlm {
        streams: Mutex::new(vec![
            // Iter 2: LLM "recovers" after seeing the error in history.
            vec![
                ChatStreamEvent::Delta("recovered".into()),
                done(FinishReason::Stop),
            ],
            // Iter 1: LLM emits a malformed tool call that the executor
            // will reject with `OrkError::Validation`.
            vec![
                ChatStreamEvent::ToolCall(ToolCall {
                    id: "call_bad".into(),
                    name: "list_repos".into(),
                    arguments: json!({}),
                }),
                done(FinishReason::ToolCalls),
            ],
        ]),
        requests: Mutex::new(Vec::new()),
    });
    let tools = Arc::new(FailFirstTools {
        pending_errors: Mutex::new(vec![OrkError::Validation(
            "agent_call: missing required field `prompt`".into(),
        )]),
    });
    let ctx = ctx();
    let agent = LocalAgent::new(config(), &CardEnrichmentContext::minimal(), llm.clone())
        .with_tool_catalog(catalog_list_repos(tools));
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
    let mut saw_err = None;
    while let Some(event) = stream.next().await {
        if let Err(e) = event {
            saw_err = Some(e);
            break;
        }
    }
    assert!(
        saw_err.is_none(),
        "agent loop must not abort the step on a recoverable tool-call error \
         (ADR 0010 §`Failure model`); got: {saw_err:?}"
    );

    let requests = llm.requests.lock().await;
    assert_eq!(
        requests.len(),
        2,
        "LLM should be re-invoked after tool error so it can retry"
    );
    let history = &requests[1].messages;
    let tool_msg = history
        .iter()
        .find(|m| format!("{:?}", m.role) == "Tool")
        .expect("history must contain a Tool-role message with the error payload");
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_bad"));
    assert!(
        tool_msg.content.contains("missing required field `prompt`"),
        "tool result must carry the validation error verbatim so the LLM \
         can self-correct; got: {}",
        tool_msg.content
    );
}

/// Cancellation must still abort the step — it is not a tool-call error,
/// it is operator/engine signal that the entire step should stop. Without
/// this guardrail, the recoverable-error path above would swallow
/// cancellation tokens. ADR 0010 §`Failure model` ("transport / connection
/// errors bubble up as step failures") covers this by analogy.
#[tokio::test]
async fn tool_call_cancellation_still_aborts_step() {
    let llm = Arc::new(ScriptedLlm {
        streams: Mutex::new(vec![vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "call_x".into(),
                name: "list_repos".into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls),
        ]]),
        requests: Mutex::new(Vec::new()),
    });
    let tools = Arc::new(EchoTools);
    let ctx = ctx();
    ctx.cancel.cancel();
    let agent = LocalAgent::new(config(), &CardEnrichmentContext::minimal(), llm.clone())
        .with_tool_catalog(catalog_list_repos(tools));
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
    let mut saw_err = None;
    while let Some(event) = stream.next().await {
        if let Err(e) = event {
            saw_err = Some(e);
            break;
        }
    }
    let err = saw_err.expect("cancelled step must surface an error");
    assert!(
        format!("{err}").contains("cancelled"),
        "expected cancellation error, got: {err}"
    );
}
