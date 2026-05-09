//! ADR-0053 acceptance criterion: `CodeAgent` consumes the registered
//! `MemoryStore` automatically and the "agent remembers user's name
//! across threads" pattern works end-to-end.
//!
//! Coverage:
//! 1. `memory.update_working` tool is auto-attached when memory is enabled.
//! 2. The LLM calling that tool persists working memory.
//! 3. A second turn under a different `ThreadId` (same `ResourceId`)
//!    sees the working-memory snapshot in its preamble.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, ResourceId, Role, TaskId, ThreadId};
use ork_agents::CodeAgent;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    ModelCapabilities, TokenUsage, ToolCall,
};
use ork_memory::{Memory, MemoryOptions, SemanticRecallConfig};
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
        ModelCapabilities {
            supports_tools: true,
            ..ModelCapabilities::default()
        }
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

fn ctx_for(tenant: TenantId, resource: ResourceId, thread: ThreadId) -> AgentContext {
    AgentContext {
        tenant_id: tenant,
        task_id: TaskId::new(),
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
        resource_id: Some(resource),
        thread_id: Some(thread),
    }
}

fn user_msg(task_id: TaskId, text: &str) -> AgentMessage {
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

async fn drain(agent: &dyn Agent, ctx: AgentContext, msg: AgentMessage) {
    let mut s = agent
        .send_stream(ctx, msg)
        .await
        .expect("send_stream returns ok");
    while let Some(ev) = s.next().await {
        let _ = ev;
    }
}

/// "Agent remembers user's name across threads" — the ADR's flagship
/// integration scenario. The LLM script in turn-A emits a tool_call to
/// `memory.update_working` with `{"name":"Arseny"}`; the libsql backend
/// persists it; turn-B in a different thread reads the snapshot back
/// through the preamble injected by `CodeAgent::send_stream`.
#[tokio::test]
async fn agent_remembers_user_name_across_threads() {
    let td = tempfile::tempdir().unwrap();
    let url = format!("file:{}", td.path().join("ork.db").display());

    let memory = Memory::libsql(url)
        .options(MemoryOptions {
            include_working: true,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: Some(ork_core::ports::memory_store::WorkingMemoryShape::User),
            last_messages: 0,
        })
        .open()
        .await
        .expect("memory open");

    // Turn A: assistant emits one tool_call (memory.update_working) and
    // then a final assistant message after observing the tool result.
    let turn_a = vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "tc1".into(),
                name: "memory.update_working".into(),
                arguments: serde_json::json!({"name": "Arseny"}),
            }),
            done(FinishReason::ToolCalls),
        ],
        vec![
            ChatStreamEvent::Delta("Got it, Arseny.".into()),
            done(FinishReason::Stop),
        ],
    ];

    let llm = Arc::new(ScriptedLlm::new(turn_a));
    let agent = CodeAgent::builder("greeter")
        .description("Remembers names.")
        .instructions("If the user shares their name, call memory.update_working.")
        .model("openai/gpt-4o-mini")
        .memory(memory.clone())
        .memory_options(MemoryOptions {
            include_working: true,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: Some(ork_core::ports::memory_store::WorkingMemoryShape::User),
            last_messages: 0,
        })
        .llm(llm.clone())
        .build()
        .expect("build greeter");

    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let thread_a = ThreadId::new();
    let ctx_a = ctx_for(tenant, resource, thread_a);
    let task_a = ctx_a.task_id;
    drain(&agent, ctx_a, user_msg(task_a, "Hi, my name is Arseny.")).await;

    // Sanity: the first ChatRequest's tool list must include
    // `memory.update_working`.
    let first_req = {
        let reqs = llm.requests.lock().await;
        reqs.first().cloned().expect("first request captured")
    };
    let tool_ids: Vec<String> = first_req.tools.iter().map(|t| t.name.clone()).collect();
    assert!(
        tool_ids.contains(&"memory.update_working".to_string()),
        "memory.update_working tool MUST be registered when memory is enabled, \
         got tools={tool_ids:?}"
    );

    // Working memory was persisted via the tool call.
    let mc = ork_core::ports::memory_store::MemoryContext {
        tenant_id: tenant,
        resource_id: resource,
        thread_id: thread_a,
        agent_id: "greeter".to_string(),
    };
    let wm = memory.working_memory(&mc).await.expect("read");
    assert_eq!(
        wm.as_ref()
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
        Some("Arseny"),
        "memory.update_working tool should have persisted name=Arseny"
    );

    // Turn B: a brand-new thread for the same resource. The agent's
    // preamble MUST include the working-memory snapshot so the LLM can
    // recall the name. The scripted LLM here just emits the canned
    // answer; we assert the captured ChatRequest carried the snapshot.
    let llm_b = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::Delta("You said Arseny.".into()),
        done(FinishReason::Stop),
    ]]));
    let agent_b = CodeAgent::builder("greeter")
        .description("Remembers names.")
        .instructions("Recall the user's name when asked.")
        .model("openai/gpt-4o-mini")
        .memory(memory.clone())
        .memory_options(MemoryOptions {
            include_working: true,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: Some(ork_core::ports::memory_store::WorkingMemoryShape::User),
            last_messages: 0,
        })
        .llm(llm_b.clone())
        .build()
        .expect("build greeter b");

    let thread_b = ThreadId::new();
    let ctx_b = ctx_for(tenant, resource, thread_b);
    let task_b = ctx_b.task_id;
    drain(&agent_b, ctx_b, user_msg(task_b, "What's my name?")).await;

    let req_b = {
        let reqs = llm_b.requests.lock().await;
        reqs.first().cloned().expect("turn B captured a request")
    };
    let saw_snapshot = req_b
        .messages
        .iter()
        .any(|m| m.content.contains("[memory.working]") && m.content.contains("Arseny"));
    assert!(
        saw_snapshot,
        "turn B's prompt MUST include the working-memory snapshot from turn A, \
         got messages={:?}",
        req_b
            .messages
            .iter()
            .map(|m| &m.content)
            .collect::<Vec<_>>()
    );
}

/// ADR-0053: both user and assistant turns must persist so
/// `last_messages(...)` returns a real transcript. Two turns under the
/// same thread; the second turn's prompt must carry both the prior user
/// line and the prior assistant line.
#[tokio::test]
async fn last_messages_carries_user_and_assistant_turns() {
    let td = tempfile::tempdir().unwrap();
    let url = format!("file:{}", td.path().join("ork.db").display());

    let memory = Memory::libsql(url)
        .options(MemoryOptions {
            include_working: false,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: None,
            last_messages: 10,
        })
        .open()
        .await
        .expect("memory open");

    let scripts = vec![
        vec![
            ChatStreamEvent::Delta("hi there".into()),
            done(FinishReason::Stop),
        ],
        vec![
            ChatStreamEvent::Delta("ok".into()),
            done(FinishReason::Stop),
        ],
    ];
    let llm = Arc::new(ScriptedLlm::new(scripts));
    let agent = CodeAgent::builder("greeter")
        .description("Two turns.")
        .instructions("You are terse.")
        .model("openai/gpt-4o-mini")
        .memory(memory.clone())
        .memory_options(MemoryOptions {
            include_working: false,
            semantic_recall: SemanticRecallConfig {
                enabled: false,
                ..Default::default()
            },
            working_memory: None,
            last_messages: 10,
        })
        .llm(llm.clone())
        .build()
        .expect("build");

    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let thread = ThreadId::new();

    let ctx_1 = ctx_for(tenant, resource, thread);
    let task_1 = ctx_1.task_id;
    drain(&agent, ctx_1, user_msg(task_1, "hello")).await;

    let ctx_2 = ctx_for(tenant, resource, thread);
    let task_2 = ctx_2.task_id;
    drain(&agent, ctx_2, user_msg(task_2, "still there?")).await;

    let req_2 = {
        let reqs = llm.requests.lock().await;
        reqs.get(1).cloned().expect("turn 2 captured")
    };
    let texts: Vec<String> = req_2.messages.iter().map(|m| m.content.clone()).collect();
    assert!(
        texts.iter().any(|t| t == "hello"),
        "turn 2 prompt must include prior user line, got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "hi there"),
        "turn 2 prompt must include prior assistant line, got {texts:?}"
    );
}
