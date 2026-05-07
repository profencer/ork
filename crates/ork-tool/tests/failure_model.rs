//! Non-fatal vs fatal tool errors through `LocalAgent` / `RigEngine` (ADR-0051).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent, TaskState};
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
use ork_tool::{IntoToolDef, tool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize, JsonSchema)]
struct Empty {}

#[derive(Debug, Serialize, JsonSchema)]
struct OkOut {
    ok: bool,
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
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }

    async fn chat_stream(&self, _request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        let events = self
            .streams
            .lock()
            .await
            .pop()
            .unwrap_or_else(|| vec![done(FinishReason::Stop, "stub-fallback")]);
        Ok(Box::pin(async_stream::stream! {
            for ev in events {
                yield Ok(ev);
            }
        }))
    }

    fn provider_name(&self) -> &str {
        "0051-failure-model-scripted"
    }
}

fn done(reason: FinishReason, model: &str) -> ChatStreamEvent {
    ChatStreamEvent::Done {
        usage: TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        model: model.into(),
        finish_reason: reason,
    }
}

fn smoke_config(tool_names: &[&str]) -> AgentConfig {
    AgentConfig {
        id: "failure-model-agent".into(),
        name: "FailureModel".into(),
        description: "test".into(),
        system_prompt: "test".into(),
        tools: tool_names.iter().map(|s| (*s).to_string()).collect(),
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 256,
        max_tool_iterations: 16,
        max_parallel_tool_calls: 4,
        max_tool_result_bytes: 65_536,
        expose_reasoning: false,
    }
}

fn test_ctx(cancel: CancellationToken) -> AgentContext {
    let tenant = TenantId::new();
    AgentContext {
        tenant_id: tenant,
        task_id: ork_a2a::TaskId::new(),
        parent_task_id: None,
        cancel,
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: vec![],
            ..CallerIdentity::default()
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: json!({}),
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
    }
}

fn user_msg(ctx: &AgentContext, text: &str) -> AgentMessage {
    AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: text.into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(ctx.task_id),
        context_id: ctx.context_id,
        metadata: None,
    }
}

fn catalog_validation_then_text() -> ToolCatalogBuilder {
    use ork_core::ports::tool_def::ToolDef;
    use std::collections::HashMap;

    let validation_tool = tool("validation_fail")
        .description("returns validation error (non-fatal by default)")
        .input::<Empty>()
        .output::<OkOut>()
        .execute(|_, _| async { Err(OrkError::Validation("bad input".into())) });

    let mut m = HashMap::new();
    m.insert(
        "validation_fail".into(),
        validation_tool.into_tool_def() as Arc<dyn ToolDef>,
    );
    ToolCatalogBuilder::new().with_native_tools(Arc::new(m))
}

#[tokio::test]
async fn validation_tool_error_is_non_fatal_and_llm_can_continue() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "v1".into(),
                name: "validation_fail".into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls, "stub"),
        ],
        vec![
            ChatStreamEvent::Delta("recovered".into()),
            done(FinishReason::Stop, "stub"),
        ],
    ]));
    let agent = LocalAgent::new(
        smoke_config(&["validation_fail"]),
        &CardEnrichmentContext::minimal(),
        llm,
    )
    .with_tool_catalog(catalog_validation_then_text());

    let ctx = test_ctx(CancellationToken::new());
    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "x"))
        .await
        .expect("send_stream");
    let mut final_text = String::new();
    while let Some(ev) = stream.next().await {
        match ev.expect("stream ok") {
            AgentEvent::Message(m) => {
                for p in m.parts {
                    if let Part::Text { text, .. } = p {
                        final_text.push_str(&text);
                    }
                }
            }
            AgentEvent::StatusUpdate(_) | AgentEvent::ArtifactUpdate(_) => {}
        }
    }
    assert!(
        final_text.contains("recovered"),
        "expected LLM to continue after non-fatal tool error; got {final_text:?}"
    );
}

fn catalog_unauthorized_fatal() -> ToolCatalogBuilder {
    use ork_core::ports::tool_def::ToolDef;
    use std::collections::HashMap;

    let fatal_tool = tool("auth_gate")
        .description("unauthorized marked fatal via `.fatal_on` (ADR-0051 / Authn parity)")
        .input::<Empty>()
        .output::<OkOut>()
        .fatal_on(|e| matches!(e, OrkError::Unauthorized(_)))
        .execute(|_, _| async { Err(OrkError::Unauthorized("nope".into())) });

    let mut m = HashMap::new();
    m.insert(
        "auth_gate".into(),
        fatal_tool.into_tool_def() as Arc<dyn ToolDef>,
    );
    ToolCatalogBuilder::new().with_native_tools(Arc::new(m))
}

#[tokio::test]
async fn unauthorized_with_fatal_on_aborts_without_completed() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::ToolCall(ToolCall {
            id: "a1".into(),
            name: "auth_gate".into(),
            arguments: json!({}),
        }),
        done(FinishReason::ToolCalls, "stub"),
    ]]));

    let agent = LocalAgent::new(
        smoke_config(&["auth_gate"]),
        &CardEnrichmentContext::minimal(),
        llm,
    )
    .with_tool_catalog(catalog_unauthorized_fatal());

    let ctx = test_ctx(CancellationToken::new());
    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "y"))
        .await
        .expect("send_stream");

    let mut saw_completed = false;
    let mut err = None;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(AgentEvent::StatusUpdate(s)) => {
                if s.is_final && s.status.state == TaskState::Completed {
                    saw_completed = true;
                }
            }
            Ok(AgentEvent::ArtifactUpdate(_)) => {}
            Ok(_) => {}
            Err(e) => err = Some(e),
        }
    }
    assert!(
        !saw_completed,
        "fatal tool error must not emit Completed (ADR-0047 FatalSlot)"
    );
    assert!(
        err.is_some(),
        "expected terminal error from fatal tool branch"
    );
}
