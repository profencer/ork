//! ADR 0052 §`Hooks` — `Proceed`, `Override`, and `Cancel` paths each behave
//! per-spec (acceptance criterion §8). Each test scripts a single tool call and
//! asserts the hook's effect on the tool result observed by the LLM.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role};
use ork_agents::{CodeAgent, CompletionHook, ToolHook, ToolHookAction};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    MessageRole, ModelCapabilities, TokenUsage, ToolCall, ToolDescriptor,
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

/// Builds a ToolDef whose execution increments `counter` and returns `payload`.
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

fn one_tool_call_then_final(tool_id: &str, after_msg: &str) -> Vec<Vec<ChatStreamEvent>> {
    vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "call_1".into(),
                name: tool_id.into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls),
        ],
        vec![
            ChatStreamEvent::Delta(after_msg.into()),
            done(FinishReason::Stop),
        ],
    ]
}

fn last_tool_result_text(req: &ChatRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, MessageRole::Tool))
        .map(|m| m.content.clone())
}

// `Proceed` — hook returns Proceed, tool runs normally, after-hook observes Ok.
#[tokio::test]
async fn proceed_hook_runs_tool_and_records_after_observation() {
    struct ProceedHook {
        before_seen: Arc<AtomicUsize>,
        after_seen: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ToolHook for ProceedHook {
        async fn before(&self, _: &AgentContext, _: &ToolDescriptor, _: &Value) -> ToolHookAction {
            self.before_seen.fetch_add(1, Ordering::SeqCst);
            ToolHookAction::Proceed
        }
        async fn after(
            &self,
            _: &AgentContext,
            _: &ToolDescriptor,
            result: &Result<Value, OrkError>,
        ) {
            assert!(result.is_ok(), "after-hook saw an error in Proceed path");
            self.after_seen.fetch_add(1, Ordering::SeqCst);
        }
    }

    let llm = Arc::new(ScriptedLlm::new(one_tool_call_then_final("stamp", "ok")));
    let calls = Arc::new(AtomicUsize::new(0));
    let before_seen = Arc::new(AtomicUsize::new(0));
    let after_seen = Arc::new(AtomicUsize::new(0));

    let agent = CodeAgent::builder("stamper")
        .description("Stamps a payload.")
        .instructions("Use the tool")
        .model("openai/gpt-4o-mini")
        .tool_dyn(counter_tool(calls.clone(), json!({"v":"real"})))
        .on_tool_call(ProceedHook {
            before_seen: before_seen.clone(),
            after_seen: after_seen.clone(),
        })
        .llm(llm.clone())
        .build()
        .expect("build stamper");

    let c = ctx();
    let task_id = c.task_id;
    let mut s = agent.send_stream(c, user_msg(task_id)).await.expect("ok");
    while let Some(ev) = s.next().await {
        ev.expect("event ok");
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1, "tool must run on Proceed");
    assert_eq!(before_seen.load(Ordering::SeqCst), 1);
    assert_eq!(after_seen.load(Ordering::SeqCst), 1);
    let requests = llm.requests.lock().await;
    let result_seen = last_tool_result_text(&requests[1]).expect("tool result in turn 2");
    assert!(
        result_seen.contains("real"),
        "LLM saw the real tool output, got {result_seen}"
    );
}

// `Override` — tool is NOT invoked; the hook's value is sent to the LLM as the result.
#[tokio::test]
async fn override_hook_short_circuits_tool_and_substitutes_result() {
    struct OverrideHook;
    #[async_trait]
    impl ToolHook for OverrideHook {
        async fn before(&self, _: &AgentContext, _: &ToolDescriptor, _: &Value) -> ToolHookAction {
            ToolHookAction::Override(json!({"v": "overridden"}))
        }
        async fn after(&self, _: &AgentContext, _: &ToolDescriptor, _: &Result<Value, OrkError>) {}
    }

    let llm = Arc::new(ScriptedLlm::new(one_tool_call_then_final("stamp", "ok")));
    let calls = Arc::new(AtomicUsize::new(0));

    let agent = CodeAgent::builder("stamper")
        .description("Stamps a payload.")
        .instructions("Use the tool")
        .model("openai/gpt-4o-mini")
        .tool_dyn(counter_tool(calls.clone(), json!({"v":"real"})))
        .on_tool_call(OverrideHook)
        .llm(llm.clone())
        .build()
        .expect("build stamper");

    let c = ctx();
    let task_id = c.task_id;
    let mut s = agent.send_stream(c, user_msg(task_id)).await.expect("ok");
    while let Some(ev) = s.next().await {
        ev.expect("event ok");
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "Override must skip the real tool"
    );
    let requests = llm.requests.lock().await;
    let result_seen = last_tool_result_text(&requests[1]).expect("tool result in turn 2");
    assert!(
        result_seen.contains("overridden"),
        "LLM saw the override, not the real value, got {result_seen}"
    );
}

// `Cancel` — abort the run with a workflow error; tool is NOT invoked.
#[tokio::test]
async fn cancel_hook_aborts_run_without_invoking_tool() {
    struct CancelHook {
        seen: Arc<StdMutex<Option<Result<Value, OrkError>>>>,
    }
    #[async_trait]
    impl ToolHook for CancelHook {
        async fn before(&self, _: &AgentContext, _: &ToolDescriptor, _: &Value) -> ToolHookAction {
            ToolHookAction::Cancel
        }
        async fn after(
            &self,
            _: &AgentContext,
            _: &ToolDescriptor,
            result: &Result<Value, OrkError>,
        ) {
            *self.seen.lock().unwrap() = Some(match result {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(OrkError::Workflow(e.to_string())),
            });
        }
    }

    let llm = Arc::new(ScriptedLlm::new(one_tool_call_then_final("stamp", "ok")));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen: Arc<StdMutex<Option<Result<Value, OrkError>>>> = Arc::new(StdMutex::new(None));

    let agent = CodeAgent::builder("stamper")
        .description("Stamps a payload.")
        .instructions("Use the tool")
        .model("openai/gpt-4o-mini")
        .tool_dyn(counter_tool(calls.clone(), json!({"v":"real"})))
        .on_tool_call(CancelHook { seen: seen.clone() })
        .llm(llm.clone())
        .build()
        .expect("build stamper");

    let c = ctx();
    let task_id = c.task_id;
    let mut s = agent.send_stream(c, user_msg(task_id)).await.expect("ok");
    let mut saw_error = false;
    while let Some(ev) = s.next().await {
        if let Err(e) = ev {
            assert!(
                e.to_string().to_lowercase().contains("hook cancelled")
                    || e.to_string().to_lowercase().contains("cancelled"),
                "expected cancellation error, got {e}"
            );
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "Cancel must surface as an error in the stream");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "Cancel must skip the tool");
    assert!(
        matches!(seen.lock().unwrap().as_ref(), Some(Err(_))),
        "after-hook must observe the cancelled invocation as an Err"
    );
}

// `CompletionHook` fires once per terminal Message with the LLM's final text.
// Multiple hooks fire in registration order.
#[tokio::test]
async fn completion_hooks_fire_once_in_registration_order() {
    struct OrderedCompletion {
        slot: Arc<StdMutex<Vec<(usize, String)>>>,
        index: usize,
    }
    #[async_trait]
    impl CompletionHook for OrderedCompletion {
        async fn on_completion(&self, _: &AgentContext, final_text: &str) {
            self.slot
                .lock()
                .unwrap()
                .push((self.index, final_text.to_string()));
        }
    }

    let llm = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::Delta("the answer is 42".into()),
        done(FinishReason::Stop),
    ]]));

    let observations: Arc<StdMutex<Vec<(usize, String)>>> = Arc::new(StdMutex::new(Vec::new()));

    let agent = CodeAgent::builder("answerer")
        .description("Answers questions.")
        .instructions("Answer briefly.")
        .model("openai/gpt-4o-mini")
        .on_completion(OrderedCompletion {
            slot: observations.clone(),
            index: 0,
        })
        .on_completion(OrderedCompletion {
            slot: observations.clone(),
            index: 1,
        })
        .llm(llm)
        .build()
        .expect("build answerer");

    let c = ctx();
    let task_id = c.task_id;
    let mut s = agent.send_stream(c, user_msg(task_id)).await.expect("ok");
    while let Some(ev) = s.next().await {
        ev.expect("event ok");
    }

    let log = observations.lock().unwrap().clone();
    assert_eq!(
        log.len(),
        2,
        "exactly one fire per registered hook per terminal Message; got {log:?}"
    );
    assert_eq!(log[0].0, 0, "registration order: hook 0 fires first");
    assert_eq!(log[1].0, 1);
    assert!(
        log[0].1.contains("the answer is 42"),
        "final_text matches the LLM output, got {:?}",
        log[0].1
    );
    assert_eq!(log[0].1, log[1].1, "all hooks see the same final text");
}
