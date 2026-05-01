//! ADR 0047 — integration smoke tests for the Rig-backed `LocalAgent` engine.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use ork_a2a::{Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent, TaskState};
use ork_agents::local::LocalAgent;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::card_builder::CardEnrichmentContext;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::Agent;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, ChatStreamEvent, FinishReason, LlmChatStream, LlmProvider,
    MessageRole, TokenUsage, ToolCall,
};
use ork_core::workflow::engine::ToolExecutor;
use serde_json::json;
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
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        self.requests.lock().await.push(request);
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
        "rig-smoke-scripted"
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
        id: "rig-smoke-agent".into(),
        name: "RigSmoke".into(),
        description: "smoke".into(),
        system_prompt: "you are a test agent".into(),
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

fn user_msg(ctx: &AgentContext, text: &str) -> AgentMessage {
    AgentMessage {
        role: Role::User,
        parts: vec![Part::Text {
            text: text.into(),
            metadata: None,
        }],
        message_id: MessageId::new(),
        task_id: Some(ctx.task_id),
        context_id: None,
        metadata: None,
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

struct FatalTools {
    invoked: AtomicUsize,
}

#[async_trait]
impl ToolExecutor for FatalTools {
    async fn execute(
        &self,
        _ctx: &AgentContext,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        self.invoked.fetch_add(1, Ordering::SeqCst);
        Err(OrkError::Workflow("boom".into()))
    }
}

#[tokio::test]
async fn smoke_text_only_streams_message_and_completed() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::Delta("hi".into()),
        done(FinishReason::Stop, "stub"),
    ]]));
    let ctx = test_ctx(CancellationToken::new());
    let agent = LocalAgent::new(
        smoke_config(&[]),
        &CardEnrichmentContext::minimal(),
        llm,
        Arc::new(EchoTools),
    );
    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "prompt"))
        .await
        .expect("send_stream");

    let mut tick = 0usize;
    let mut working_head_at = None;
    let mut first_status_text_at = None;
    let mut message_at = None;
    let mut completed_at = None;
    let mut saw_message_text = false;

    while let Some(ev) = stream.next().await {
        match ev.expect("event") {
            AgentEvent::Message(m) => {
                message_at.get_or_insert(tick);
                let t: String = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if t.contains("hi") {
                    saw_message_text = true;
                }
            }
            AgentEvent::StatusUpdate(s) => {
                if !s.is_final && s.status.state == TaskState::Working {
                    if s.status.message.is_none()
                        || s.status.message.as_ref().is_some_and(|m| m.is_empty())
                    {
                        working_head_at.get_or_insert(tick);
                    } else {
                        first_status_text_at.get_or_insert(tick);
                    }
                }
                if s.is_final && s.status.state == TaskState::Completed {
                    completed_at.get_or_insert(tick);
                }
            }
            AgentEvent::ArtifactUpdate(_) => {}
        }
        tick += 1;
    }

    let wh = working_head_at.expect("StatusUpdate(Working) before deltas (outer + rig)");
    let st = first_status_text_at.expect("at least one status_text delta as Working");
    let msg = message_at.expect("final Message");
    let done = completed_at.expect("StatusUpdate(Completed, is_final=true)");
    assert!(
        wh < st && st < msg && msg < done,
        "ADR-0047 AC (a) order: Working(head) < status_text < Message < Completed; got indices {wh} {st} {msg} {done}"
    );
    assert!(
        saw_message_text,
        "final message should contain streamed text"
    );
}

#[tokio::test]
async fn smoke_tool_round_trip_through_rig_engine() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "tc1".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls, "stub"),
        ],
        vec![
            ChatStreamEvent::Delta("wrapped-up".into()),
            done(FinishReason::Stop, "stub"),
        ],
    ]));
    let ctx = test_ctx(CancellationToken::new());
    let agent = LocalAgent::new(
        smoke_config(&["github_recent_activity"]),
        &CardEnrichmentContext::minimal(),
        llm.clone(),
        Arc::new(EchoTools),
    );

    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "run tool"))
        .await
        .expect("send_stream");

    while let Some(ev) = stream.next().await {
        ev.expect("event ok");
    }

    let requests = llm.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let msgs = &requests[1].messages;
    assert!(
        msgs.iter()
            .any(|m| m.tool_call_id.as_deref() == Some("tc1")),
        "second LLM prompt must carry tool tc1 result"
    );
}

#[tokio::test]
async fn smoke_non_fatal_tool_error_allows_retry() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "bad".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls, "stub"),
        ],
        vec![
            ChatStreamEvent::Delta("fine".into()),
            done(FinishReason::Stop, "stub"),
        ],
    ]));
    let tools = Arc::new(FailFirstTools {
        pending_errors: Mutex::new(vec![OrkError::Validation("recover".into())]),
    });
    let ctx = test_ctx(CancellationToken::new());
    let agent = LocalAgent::new(
        smoke_config(&["github_recent_activity"]),
        &CardEnrichmentContext::minimal(),
        llm,
        tools,
    );

    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "x"))
        .await
        .expect("send_stream");
    let mut final_message = String::new();
    while let Some(ev) = stream.next().await {
        match ev.expect("stream must complete without fatal abort") {
            AgentEvent::Message(m) => {
                for p in m.parts {
                    if let Part::Text { text, .. } = p {
                        final_message.push_str(&text);
                    }
                }
            }
            AgentEvent::StatusUpdate(_) | AgentEvent::ArtifactUpdate(_) => {}
        }
    }
    assert!(
        final_message.contains("fine"),
        "expected recovered final text after non-fatal tool error; got {:?}",
        final_message
    );
}

#[tokio::test]
async fn smoke_fatal_tool_error_aborts_without_completed() {
    let llm = Arc::new(ScriptedLlm::new(vec![vec![
        ChatStreamEvent::ToolCall(ToolCall {
            id: "f".into(),
            name: "github_recent_activity".into(),
            arguments: json!({}),
        }),
        done(FinishReason::ToolCalls, "stub"),
    ]]));
    let ctx = test_ctx(CancellationToken::new());
    let tools = Arc::new(FatalTools {
        invoked: AtomicUsize::new(0),
    });
    let agent = LocalAgent::new(
        smoke_config(&["github_recent_activity"]),
        &CardEnrichmentContext::minimal(),
        llm,
        tools.clone(),
    );

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
        tools.invoked.load(Ordering::SeqCst) >= 1,
        "fatal tool must have been executed"
    );
    assert!(
        !saw_completed,
        "fatal tool error must not emit Completed status"
    );
    assert!(err.is_some(), "expected error from fatal tool branch");
}

/// Observes concurrent [`ToolExecutor::execute`] calls to ensure [`AgentConfig::max_parallel_tool_calls`]
/// is enforced via [`OrkToolDyn`]'s semaphore (ADR-0047 `## Open questions`).
struct ParallelProbeTools {
    active: AtomicUsize,
    max_active: AtomicUsize,
    total_calls: AtomicUsize,
}

#[async_trait]
impl ToolExecutor for ParallelProbeTools {
    async fn execute(
        &self,
        _ctx: &AgentContext,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.total_calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(json!({"ok": true}))
    }
}

#[tokio::test]
async fn smoke_parallel_tool_calls_observe_semaphore() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "p1".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            ChatStreamEvent::ToolCall(ToolCall {
                id: "p2".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            ChatStreamEvent::ToolCall(ToolCall {
                id: "p3".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            ChatStreamEvent::ToolCall(ToolCall {
                id: "p4".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls, "stub"),
        ],
        vec![
            ChatStreamEvent::Delta("parallel-done".into()),
            done(FinishReason::Stop, "stub"),
        ],
    ]));
    let tools = Arc::new(ParallelProbeTools {
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        total_calls: AtomicUsize::new(0),
    });
    let ctx = test_ctx(CancellationToken::new());
    let mut cfg = smoke_config(&["github_recent_activity"]);
    cfg.max_parallel_tool_calls = 2;
    let agent = LocalAgent::new(cfg, &CardEnrichmentContext::minimal(), llm, tools.clone());
    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "parallel tools"))
        .await
        .expect("send_stream");
    while let Some(ev) = stream.next().await {
        ev.expect("stream event");
    }
    assert_eq!(
        tools.total_calls.load(Ordering::SeqCst),
        4,
        "all tool calls must run"
    );
    let max = tools.max_active.load(Ordering::SeqCst);
    assert!(
        max <= 2,
        "`max_parallel_tool_calls` must cap concurrent tool execution; max_active={max}"
    );
}

/// Minimal in-memory store for spill regression (ADR-0016 / ADR-0047).
struct MemoryArtifactStore {
    scheme: &'static str,
    blobs: StdMutex<HashMap<String, Bytes>>,
}

impl MemoryArtifactStore {
    fn new(scheme: &'static str) -> Self {
        Self {
            scheme,
            blobs: StdMutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ArtifactStore for MemoryArtifactStore {
    fn scheme(&self) -> &'static str {
        self.scheme
    }

    async fn put(
        &self,
        scope: &ArtifactScope,
        name: &str,
        body: ArtifactBody,
        meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        let _ = meta;
        let bytes = match body {
            ArtifactBody::Bytes(b) => b,
            ArtifactBody::Stream(_) => {
                return Err(OrkError::Internal(
                    "MemoryArtifactStore test helper expects bytes body".into(),
                ));
            }
        };
        let aref = ArtifactRef {
            scheme: self.scheme.to_string(),
            tenant_id: scope.tenant_id,
            context_id: scope.context_id,
            name: name.to_string(),
            version: 1,
            etag: "test".into(),
        };
        self.blobs.lock().unwrap().insert(aref.to_wire(), bytes);
        Ok(aref)
    }

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        self.blobs
            .lock()
            .unwrap()
            .get(&r#ref.to_wire())
            .cloned()
            .map(ArtifactBody::Bytes)
            .ok_or_else(|| OrkError::NotFound(r#ref.to_wire()))
    }

    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        let body = self.get(r#ref).await?;
        let size = match body {
            ArtifactBody::Bytes(b) => b.len() as u64,
            ArtifactBody::Stream(_) => 0,
        };
        Ok(ArtifactMeta {
            mime: Some("application/json".into()),
            size,
            created_at: Utc::now(),
            created_by: None,
            task_id: None,
            labels: Default::default(),
        })
    }

    async fn list(
        &self,
        _scope: &ArtifactScope,
        _prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        Ok(vec![])
    }

    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        let _ = r#ref;
        Ok(())
    }
}

struct BigJsonTools;

#[async_trait]
impl ToolExecutor for BigJsonTools {
    async fn execute(
        &self,
        _ctx: &AgentContext,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        Ok(json!({ "payload": "y".repeat(4096) }))
    }
}

#[tokio::test]
async fn smoke_oversized_tool_result_spills_to_artifact() {
    let store = Arc::new(MemoryArtifactStore::new("mem"));
    let cancel = CancellationToken::new();
    let mut ctx = test_ctx(cancel);
    ctx.artifact_store = Some(store);
    ctx.artifact_public_base = Some("http://artifacts.test".into());

    let llm = Arc::new(ScriptedLlm::new(vec![
        vec![
            ChatStreamEvent::ToolCall(ToolCall {
                id: "spill1".into(),
                name: "github_recent_activity".into(),
                arguments: json!({}),
            }),
            done(FinishReason::ToolCalls, "stub"),
        ],
        vec![
            ChatStreamEvent::Delta("spilled-ok".into()),
            done(FinishReason::Stop, "stub"),
        ],
    ]));

    let mut cfg = smoke_config(&["github_recent_activity"]);
    cfg.max_tool_result_bytes = 256;

    let agent = LocalAgent::new(
        cfg,
        &CardEnrichmentContext::minimal(),
        llm.clone(),
        Arc::new(BigJsonTools),
    );
    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "spill"))
        .await
        .expect("send_stream");
    while let Some(ev) = stream.next().await {
        ev.expect("event");
    }

    let requests = llm.requests.lock().await;
    assert_eq!(requests.len(), 2, "tool round-trip then final text");
    let hist = &requests[1].messages;
    assert!(
        hist.iter().any(|m| {
            m.role == MessageRole::Tool
                && m.tool_call_id.as_deref() == Some("spill1")
                && m.content.contains("ork_spilled_tool_result")
        }),
        "expected ADR-0016 spill marker in second-request history; got {hist:?}"
    );
}

struct StallAfterFirstDelta;

#[async_trait]
impl LlmProvider for StallAfterFirstDelta {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, OrkError> {
        unreachable!()
    }

    async fn chat_stream(&self, _request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        Ok(Box::pin(async_stream::stream! {
            yield Ok(ChatStreamEvent::Delta("partial".into()));
            tokio::time::sleep(Duration::from_millis(250)).await;
            yield Ok(done(FinishReason::Stop, "stub"));
        }))
    }

    fn provider_name(&self) -> &str {
        "stall-after-delta"
    }
}

/// Cancel quickly after streaming starts — should surface within ADR-implied budget (50 ms).
#[tokio::test]
async fn smoke_cancel_mid_stream_under_50ms() {
    let cancel = CancellationToken::new();
    let ctx = test_ctx(cancel.clone());
    let agent = LocalAgent::new(
        smoke_config(&[]),
        &CardEnrichmentContext::minimal(),
        Arc::new(StallAfterFirstDelta),
        Arc::new(EchoTools),
    );

    tokio::spawn({
        let c = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            c.cancel();
        }
    });

    let start = Instant::now();
    let mut stream = agent
        .send_stream(ctx.clone(), user_msg(&ctx, "prompt"))
        .await
        .expect("send_stream");

    loop {
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("stream must not hang without progress");
        let Some(ev) = next else { break };

        match ev {
            Err(e) if e.to_string().to_lowercase().contains("cancel") => {
                assert!(
                    start.elapsed() < Duration::from_millis(50),
                    "ADR-0047 AC (e): cancel within ≤50 ms of next poll; saw {:?}",
                    start.elapsed()
                );
                return;
            }
            Ok(_) | Err(_) => {}
        }
    }

    panic!("expected cancellation error mid-stream");
}
