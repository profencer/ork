//! ADR 0052 — `CodeAgent` satisfies the A2A surface for cards, streaming, cancel
//! (acceptance criterion §2). The same scripted-LLM pattern as
//! [`crates/ork-agents/src/local.rs`](../src/local.rs) and
//! [`crates/ork-agents/tests/rig_engine_smoke.rs`](rig_engine_smoke.rs).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{
    AgentSkill, Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent, TaskState,
    extensions::EXT_TRANSPORT_HINT,
};
use ork_agents::CodeAgent;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    ModelCapabilities, TokenUsage,
};
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
        unreachable!("CodeAgent uses chat_stream only")
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

fn ctx_with_cancel(cancel: CancellationToken) -> AgentContext {
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

fn ctx() -> AgentContext {
    ctx_with_cancel(CancellationToken::new())
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

#[test]
fn card_carries_description_default_skill_and_transport_hint() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let agent = CodeAgent::builder("planner")
        .description("Plans research tasks.")
        .instructions("Be concise.")
        .model("openai/gpt-4o-mini")
        .llm(llm)
        .build()
        .expect("build planner");

    let card = agent.card();
    assert_eq!(card.name, "planner");
    assert_eq!(card.description, "Plans research tasks.");
    assert!(card.capabilities.streaming);

    let skills = &card.skills;
    assert_eq!(skills.len(), 1, "default skill set is exactly one entry");
    assert_eq!(skills[0].id, "planner-default");
    assert_eq!(skills[0].name, "planner");
    assert_eq!(skills[0].description, "Plans research tasks.");

    let exts = card
        .extensions
        .as_ref()
        .expect("transport-hint extension always present (build_local_card)");
    assert!(
        exts.iter().any(|e| e.uri == EXT_TRANSPORT_HINT),
        "card must include the transport-hint extension"
    );
}

#[test]
fn skills_override_replaces_default_skill_set() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let agent = CodeAgent::builder("planner")
        .description("Plans.")
        .instructions("Be concise.")
        .model("openai/gpt-4o-mini")
        .skills(vec![
            AgentSkill {
                id: "plan-research".into(),
                name: "Plan research".into(),
                description: "Decompose a research goal.".into(),
                tags: vec!["planning".into()],
                examples: vec![],
                input_modes: None,
                output_modes: None,
            },
            AgentSkill {
                id: "plan-build".into(),
                name: "Plan build".into(),
                description: "Decompose a build goal.".into(),
                tags: vec!["planning".into()],
                examples: vec![],
                input_modes: None,
                output_modes: None,
            },
        ])
        .llm(llm)
        .build()
        .expect("build planner");
    let ids: Vec<&str> = agent.card().skills.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["plan-research", "plan-build"]);
}

#[tokio::test]
async fn streaming_yields_working_then_message_then_final() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::Delta("hello".into()),
        done(FinishReason::Stop),
    ]]));
    let agent = CodeAgent::builder("greeter")
        .description("Greets users.")
        .instructions("Greet briefly.")
        .model("openai/gpt-4o-mini")
        .llm(llm)
        .build()
        .expect("build greeter");

    let ctx = ctx();
    let task_id = ctx.task_id;
    let mut stream = agent
        .send_stream(ctx, user_msg(task_id, "hi"))
        .await
        .expect("send_stream");

    let mut got_working = false;
    let mut got_message = false;
    let mut got_final = false;
    while let Some(ev) = stream.next().await {
        match ev.expect("event ok") {
            AgentEvent::StatusUpdate(s) if s.status.state == TaskState::Working => {
                got_working = true;
            }
            AgentEvent::Message(m) => {
                let text = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert_eq!(text, "hello");
                got_message = true;
            }
            AgentEvent::StatusUpdate(s) if s.is_final => {
                assert_eq!(s.status.state, TaskState::Completed);
                got_final = true;
            }
            _ => {}
        }
    }
    assert!(got_working, "missing initial Working status");
    assert!(got_message, "missing terminal Message event");
    assert!(
        got_final,
        "missing terminal StatusUpdate {{ is_final: true }}"
    );
}

#[tokio::test]
async fn pre_cancelled_context_short_circuits_without_llm_call() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let agent = CodeAgent::builder("greeter")
        .description("Greets users.")
        .instructions("Greet briefly.")
        .model("openai/gpt-4o-mini")
        .llm(llm.clone())
        .build()
        .expect("build greeter");

    let ctx = ctx_with_cancel(cancel);
    let task_id = ctx.task_id;
    let mut stream = agent
        .send_stream(ctx, user_msg(task_id, "hi"))
        .await
        .expect("send_stream");

    let mut saw_error = false;
    while let Some(ev) = stream.next().await {
        if let Err(e) = ev {
            assert!(
                e.to_string().to_lowercase().contains("cancelled"),
                "expected cancellation error, got {e}"
            );
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "expected a cancellation error in the stream");
    assert!(
        llm.requests.lock().await.is_empty(),
        "no LLM request should have been issued for a pre-cancelled context"
    );
}

#[tokio::test]
async fn missing_tool_capability_short_circuits_before_llm_call() {
    struct NoToolsLlm {
        captured: Mutex<Vec<ChatRequest>>,
    }
    #[async_trait]
    impl LlmProvider for NoToolsLlm {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, OrkError> {
            unreachable!()
        }
        async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError> {
            self.captured.lock().await.push(request);
            Ok(Box::pin(
                async_stream::stream! { yield Ok(done(FinishReason::Stop)); },
            ))
        }
        fn provider_name(&self) -> &str {
            "no-tools"
        }
        fn capabilities(&self, _: &str) -> ModelCapabilities {
            ModelCapabilities {
                supports_tools: false,
                ..ModelCapabilities::default()
            }
        }
    }

    let llm = Arc::new(NoToolsLlm {
        captured: Mutex::new(Vec::new()),
    });
    // Build a tiny tool to attach so the tools-list is non-empty and the preflight check fires.
    use ork_core::a2a::AgentContext as ToolCtx;
    use ork_core::ports::tool_def::ToolDef;
    use serde_json::Value;
    struct NoOpTool;
    #[async_trait]
    impl ToolDef for NoOpTool {
        fn id(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "noop"
        }
        fn input_schema(&self) -> &Value {
            static S: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({"type":"object","properties":{}}))
        }
        fn output_schema(&self) -> &Value {
            static S: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn invoke(&self, _: &ToolCtx, _: &Value) -> Result<Value, OrkError> {
            Ok(Value::Null)
        }
    }

    let agent = CodeAgent::builder("toolful")
        .description("Has a tool.")
        .instructions("Use the tool.")
        .model("openai/gpt-4o-mini")
        .tool_dyn(Arc::new(NoOpTool))
        .llm(llm.clone())
        .build()
        .expect("build toolful");

    let ctx = ctx();
    let task_id = ctx.task_id;
    let mut stream = agent
        .send_stream(ctx, user_msg(task_id, "hi"))
        .await
        .expect("send_stream");

    let mut saw_error = false;
    while let Some(ev) = stream.next().await {
        if let Err(e) = ev {
            assert!(
                e.to_string().contains("does not support tool calls"),
                "expected tool-capability error, got {e}"
            );
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "expected a tool-capability error in the stream");
    assert!(
        llm.captured.lock().await.is_empty(),
        "no chat_stream request should have been issued"
    );
}
