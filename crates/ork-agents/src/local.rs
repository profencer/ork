use std::io::Write;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, future::join_all};
use ork_a2a::{
    AgentCard, FileRef, Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_core::a2a::card_builder::{CardEnrichmentContext, build_local_card};
use ork_core::a2a::{AgentContext, AgentId, ResolveContext};
use ork_core::artifact_spill::spill_bytes_to_artifact;
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::artifact_store::ArtifactScope;
use ork_core::ports::llm::{
    ChatMessage, ChatRequest, ChatStreamEvent, FinishReason, LlmProvider, ToolCall, ToolChoice,
};
use ork_core::workflow::engine::ToolExecutor;
use tokio::sync::Semaphore;
use tracing::{info, warn};
use uuid::Uuid;

use crate::tool_catalog::ToolCatalogBuilder;

pub struct LocalAgent {
    id: AgentId,
    card: AgentCard,
    config: AgentConfig,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolExecutor>,
    tool_catalog: ToolCatalogBuilder,
}

impl LocalAgent {
    #[must_use]
    pub fn new(
        config: AgentConfig,
        card_ctx: &CardEnrichmentContext,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<dyn ToolExecutor>,
    ) -> Self {
        let card = build_local_card(&config, card_ctx);
        let id = config.id.clone();
        Self {
            id,
            card,
            config,
            llm,
            tools,
            tool_catalog: ToolCatalogBuilder::new(),
        }
    }

    #[must_use]
    pub fn with_tool_catalog(mut self, tool_catalog: ToolCatalogBuilder) -> Self {
        self.tool_catalog = tool_catalog;
        self
    }

    pub fn replace_card(&mut self, card: AgentCard) {
        self.card = card;
    }
}

fn extract_prompt_text(msg: &AgentMessage) -> Result<String, OrkError> {
    let mut s = String::new();
    for p in &msg.parts {
        match p {
            Part::Text { text, .. } => s.push_str(text),
            Part::Data { data, .. } => s.push_str(&serde_json::to_string(data).unwrap_or_default()),
            Part::File { .. } => {
                return Err(OrkError::Validation(
                    "file parts are not supported in LocalAgent yet (TODO(ADR-0003/0016))".into(),
                ));
            }
        }
    }
    if s.is_empty() {
        return Err(OrkError::Validation(
            "agent message has no usable text content".into(),
        ));
    }
    Ok(s)
}

fn print_llm_output_to_stderr_enabled() -> bool {
    std::env::var("ORK_PRINT_LLM_OUTPUT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn status_text(task_id: ork_a2a::TaskId, text: impl Into<String>, is_final: bool) -> AgentEvent {
    AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id,
        status: TaskStatus {
            state: if is_final {
                TaskState::Completed
            } else {
                TaskState::Working
            },
            message: Some(text.into()),
        },
        is_final,
    })
}

fn truncate_tool_result(serialized: String, max_bytes: usize) -> (String, bool) {
    if serialized.len() <= max_bytes {
        return (serialized, false);
    }
    let mut out = serialized;
    out.truncate(max_bytes);
    out.push_str("\n...[truncated]");
    (out, true)
}

/// ADR-0011 / ADR-0016: when a tool result exceeds the byte cap, store the full JSON and
/// return a short pointer the LLM can use (fetch via `artifact_uri` or tools).
async fn try_spill_oversized_tool_result(ctx: &AgentContext, serialized: String) -> Option<String> {
    let store = ctx.artifact_store.as_ref()?;
    let scope = ArtifactScope {
        tenant_id: ctx.tenant_id,
        context_id: ctx.context_id,
    };
    let name = format!("tool_result/{}", Uuid::new_v4());
    let (aref, part) = spill_bytes_to_artifact(
        store,
        ctx.artifact_public_base.as_deref(),
        &scope,
        &name,
        Bytes::from(serialized.into_bytes()),
        Some("application/json".into()),
        Some(ctx.task_id),
    )
    .await
    .map_err(|e| {
        warn!(error = %e, "tool result artifact spill failed");
        e
    })
    .ok()?;
    let uri = match &part {
        Part::File {
            file: FileRef::Uri { uri, .. },
            ..
        } => uri.to_string(),
        _ => return None,
    };
    let v = serde_json::json!({
        "ork_spilled_tool_result": true,
        "artifact_ref": aref.to_wire(),
        "artifact_uri": uri,
    });
    serde_json::to_string(&v).ok()
}

async fn execute_tool_call(
    tools: Arc<dyn ToolExecutor>,
    ctx: AgentContext,
    call: ToolCall,
    max_tool_result_bytes: usize,
    semaphore: Arc<Semaphore>,
) -> Result<(String, String, bool), OrkError> {
    let _permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_| OrkError::Workflow("tool semaphore closed".into()))?;
    if ctx.cancel.is_cancelled() {
        return Err(OrkError::Workflow("agent task cancelled".into()));
    }
    let output = tools.execute(&ctx, &call.name, &call.arguments).await?;
    let serialized = serde_json::to_string(&output)
        .map_err(|e| OrkError::Internal(format!("serialize tool result: {e}")))?;
    let (content, truncated) = if serialized.len() > max_tool_result_bytes {
        if let Some(s) = try_spill_oversized_tool_result(&ctx, serialized.clone()).await {
            (s, false)
        } else {
            truncate_tool_result(serialized, max_tool_result_bytes)
        }
    } else {
        (serialized, false)
    };
    Ok((call.id, content, truncated))
}

/// Per ADR 0010 §`Failure model` ("tool-call errors stay in the tool result
/// (LLM can retry); transport / connection errors bubble up as step
/// failures"), only a narrow set of error variants are treated as fatal to
/// the step. Everything else gets converted into a structured tool-result
/// payload below so the LLM can see the failure and self-correct on the
/// next iteration. Without this, an LLM that emits a single malformed
/// tool call (e.g. `agent_call` without a `prompt` field) kills the entire
/// workflow step instead of retrying.
fn is_fatal_tool_error(err: &OrkError) -> bool {
    match err {
        OrkError::Workflow(_) | OrkError::Internal(_) | OrkError::Database(_) => true,
        OrkError::NotFound(_)
        | OrkError::Unauthorized(_)
        | OrkError::Forbidden(_)
        | OrkError::Validation(_)
        | OrkError::Conflict(_)
        | OrkError::LlmProvider(_)
        | OrkError::Integration(_)
        | OrkError::Unsupported(_)
        | OrkError::A2aClient(..)
        | OrkError::A2aStreamLost(_) => false,
    }
}

fn tool_error_payload(call_name: &str, err: &OrkError, max_tool_result_bytes: usize) -> String {
    let payload = serde_json::json!({
        "error": {
            "tool": call_name,
            "message": err.to_string(),
        }
    });
    let serialized = serde_json::to_string(&payload).unwrap_or_else(|_| {
        format!("{{\"error\":{{\"tool\":\"{call_name}\",\"message\":\"<unserializable>\"}}}}")
    });
    let (content, _truncated) = truncate_tool_result(serialized, max_tool_result_bytes);
    content
}

#[async_trait]
impl Agent for LocalAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn card(&self) -> &AgentCard {
        &self.card
    }

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let prompt = extract_prompt_text(&msg)?;
        let task_id = ctx.task_id;
        let context_id = ctx.context_id;
        let mut user = ChatMessage::user(prompt);
        user.parts = msg.parts.clone();

        let tools = self.tools.clone();
        let tool_catalog = self.tool_catalog.clone();
        let config = self.config.clone();
        let llm = self.llm.clone();
        let agent_id = self.id.clone();
        // ADR 0012 §`Routing`: bind the resolution-time context so the
        // `LlmRouter` underneath `llm` can read `tenant_id` from a tokio
        // task-local. We do not pass the tenant id through `LlmProvider`
        // directly — the trait stays clean.
        let resolve_ctx = ResolveContext {
            tenant_id: ctx.tenant_id,
        };

        // ADR 0012 §`Selection`: resolve the per-step → agent precedence
        // here so every iteration of the loop below builds the same
        // `request.{provider, model}`. The tenant default + operator
        // default still resolve inside `LlmRouter::resolve`.
        let step_overrides = ctx.step_llm_overrides.clone();
        let request_provider = step_overrides
            .as_ref()
            .and_then(|o| o.provider.clone())
            .or_else(|| config.provider.clone());
        let request_model = step_overrides
            .as_ref()
            .and_then(|o| o.model.clone())
            .or_else(|| config.model.clone());

        let s = stream! {
            yield Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                status: TaskStatus { state: TaskState::Working, message: None },
                is_final: false,
            }));

            let tool_descriptors = match tool_catalog.for_agent(&ctx, &config).await {
                Ok(t) => t,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            // ADR 0012 §`Selection`: ask the provider/router for the
            // capabilities of the *resolved* (provider, model) pair —
            // not the static `AgentConfig.model` that may not be the
            // one this request actually hits. Routers override
            // `capabilities_for` to honour the full chain; single-
            // provider impls fall back to `capabilities(model)`.
            let preflight = ChatRequest {
                messages: Vec::new(),
                temperature: None,
                max_tokens: None,
                model: request_model.clone(),
                provider: request_provider.clone(),
                tools: Vec::new(),
                tool_choice: None,
            };
            let caps = resolve_ctx.scope(llm.capabilities_for(&preflight)).await;
            if !tool_descriptors.is_empty() && !caps.supports_tools {
                let label = request_model.clone().unwrap_or_default();
                yield Err(OrkError::LlmProvider(format!(
                    "model {label} does not support tool calls"
                )));
                return;
            }

            let mut history = vec![ChatMessage::system(config.system_prompt.clone()), user];
            let show = print_llm_output_to_stderr_enabled();
            if show {
                let mut stderr = std::io::stderr().lock();
                let _ = writeln!(stderr);
                let _ = writeln!(stderr, "========== LLM output ({agent_id}) ==========");
            }

            let mut total_tool_calls = 0usize;
            let mut iterations = 0usize;
            loop {
                if ctx.cancel.is_cancelled() {
                    yield Err(OrkError::Workflow("agent task cancelled".into()));
                    return;
                }
                if iterations >= config.max_tool_iterations {
                    info!(
                        agent = %agent_id,
                        tool_calls = total_tool_calls,
                        iterations,
                        tool_loop_exceeded = true,
                        "TODO(ADR-0022): agent tool loop telemetry"
                    );
                    yield Ok(status_text(task_id, "tool loop exceeded", false));
                    yield Err(OrkError::Workflow("tool_loop_exceeded".into()));
                    return;
                }
                iterations += 1;

                let request = ChatRequest {
                    messages: history.clone(),
                    temperature: Some(config.temperature),
                    max_tokens: Some(config.max_tokens),
                    // ADR 0012 §`Selection`: workflow-step override wins
                    // over agent-config; both already collapsed into
                    // `request_{provider, model}` above. Tenant + operator
                    // defaults are resolved inside `LlmRouter::resolve`.
                    model: request_model.clone(),
                    provider: request_provider.clone(),
                    tools: tool_descriptors.clone(),
                    tool_choice: if tool_descriptors.is_empty() { None } else { Some(ToolChoice::Auto) },
                };

                // Wrap the LLM call in the tenant ResolveContext so the
                // router can read `tenant_id` synchronously inside its
                // resolver. This is sound because `LlmRouter::chat_stream`
                // resolves the provider eagerly before returning — the
                // `LlmChatStream` it hands back already closes over the
                // resolved client and never re-enters the resolver. If
                // a future provider deferred resolution into the stream,
                // we would need to scope the whole stream consumption,
                // not just the `chat_stream(request)` future.
                let mut llm_stream = match resolve_ctx.scope(llm.chat_stream(request)).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };

                let mut content = String::new();
                let mut tool_calls = Vec::new();
                let mut finish_reason = FinishReason::Stop;
                while let Some(ev) = llm_stream.next().await {
                    if ctx.cancel.is_cancelled() {
                        yield Err(OrkError::Workflow("agent task cancelled".into()));
                        return;
                    }
                    match ev {
                        Ok(ChatStreamEvent::Delta(d)) => {
                            content.push_str(&d);
                            if show {
                                let mut stderr = std::io::stderr().lock();
                                let _ = write!(stderr, "{d}");
                                let _ = stderr.flush();
                            }
                            if !d.is_empty() {
                                yield Ok(status_text(task_id, d, false));
                            }
                        }
                        Ok(ChatStreamEvent::ToolCall(call)) => tool_calls.push(call),
                        Ok(ChatStreamEvent::ToolCallDelta { .. }) => {}
                        Ok(ChatStreamEvent::Done { finish_reason: reason, .. }) => {
                            finish_reason = reason;
                            break;
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }

                if !matches!(finish_reason, FinishReason::ToolCalls) {
                    if show {
                        let mut stderr = std::io::stderr().lock();
                        let _ = writeln!(stderr);
                        let _ = writeln!(stderr, "========== end LLM output ==========");
                        let _ = writeln!(stderr);
                    }
                    info!(
                        agent = %agent_id,
                        tool_calls = total_tool_calls,
                        iterations,
                        tool_loop_exceeded = false,
                        "TODO(ADR-0022): agent tool loop telemetry"
                    );
                    let final_msg = AgentMessage {
                        role: Role::Agent,
                        parts: vec![Part::Text { text: content.clone(), metadata: None }],
                        message_id: MessageId::new(),
                        task_id: Some(task_id),
                        context_id,
                        metadata: None,
                    };
                    yield Ok(AgentEvent::Message(final_msg));
                    yield Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                        task_id,
                        status: TaskStatus { state: TaskState::Completed, message: None },
                        is_final: true,
                    }));
                    return;
                }

                if tool_calls.is_empty() {
                    yield Err(OrkError::LlmProvider("finish_reason=tool_calls but no tool calls were emitted".into()));
                    return;
                }

                total_tool_calls += tool_calls.len();
                history.push(ChatMessage::assistant(content, tool_calls.clone()));

                let semaphore = Arc::new(Semaphore::new(config.max_parallel_tool_calls.max(1)));
                let max_bytes = config.max_tool_result_bytes;
                // ADR 0010 §`Failure model`: we want recoverable per-call
                // errors to surface as Tool-role messages so the LLM can
                // retry on the next iteration. `try_join_all` short-
                // circuits on the first `Err` and would abort the whole
                // step (which is what killed the demo's `review` step on
                // a single malformed `agent_call`). `join_all` preserves
                // every result; `is_fatal_tool_error` then decides which
                // ones still abort vs. get fed back to the LLM.
                let futures = tool_calls.into_iter().map(|call| {
                    let tools = tools.clone();
                    let ctx = ctx.clone();
                    let semaphore = semaphore.clone();
                    let call_name = call.name.clone();
                    let call_id = call.id.clone();
                    async move {
                        match execute_tool_call(tools, ctx, call, max_bytes, semaphore).await {
                            Ok(triple) => Ok(triple),
                            Err(e) if is_fatal_tool_error(&e) => Err(e),
                            Err(e) => {
                                let content = tool_error_payload(&call_name, &e, max_bytes);
                                Ok((call_id, content, false))
                            }
                        }
                    }
                });
                let raw_results = join_all(futures).await;
                let mut tool_results = Vec::with_capacity(raw_results.len());
                for r in raw_results {
                    match r {
                        Ok(triple) => tool_results.push(triple),
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }

                for (tool_call_id, content, truncated) in tool_results {
                    if truncated {
                        yield Ok(status_text(
                            task_id,
                            "tool result truncated (byte cap); configure artifact store + public base for spillover",
                            false,
                        ));
                    }
                    history.push(ChatMessage::tool(tool_call_id, content));
                }
            }
        };

        Ok(Box::pin(s) as AgentEventStream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use ork_common::types::TenantId;
    use ork_core::a2a::CallerIdentity;
    use ork_core::ports::llm::{ChatResponse, LlmChatStream, ModelCapabilities, TokenUsage};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct ScriptedLlm {
        streams: Mutex<Vec<Vec<ChatStreamEvent>>>,
        requests: Mutex<Vec<ChatRequest>>,
        capabilities: ModelCapabilities,
    }

    impl ScriptedLlm {
        fn new(streams: Vec<Vec<ChatStreamEvent>>) -> Self {
            Self {
                streams: Mutex::new(streams.into_iter().rev().collect()),
                requests: Mutex::new(Vec::new()),
                capabilities: ModelCapabilities::default(),
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

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            self.capabilities
        }
    }

    struct StubTools {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    #[async_trait]
    impl ToolExecutor for StubTools {
        async fn execute(
            &self,
            _ctx: &AgentContext,
            tool_name: &str,
            input: &serde_json::Value,
        ) -> Result<serde_json::Value, OrkError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
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
            step_llm_overrides: None,
            artifact_store: None,
            artifact_public_base: None,
        }
    }

    fn cfg(tools: Vec<String>) -> AgentConfig {
        AgentConfig {
            id: "writer".into(),
            name: "Writer".into(),
            description: "test".into(),
            system_prompt: "sys".into(),
            tools,
            provider: None,
            model: None,
            temperature: 0.3,
            max_tokens: 100,
            max_tool_iterations: 16,
            max_parallel_tool_calls: 4,
            max_tool_result_bytes: 65_536,
            expose_reasoning: false,
        }
    }

    fn msg(task_id: ork_a2a::TaskId) -> AgentMessage {
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

    async fn collect_text(agent: &LocalAgent, ctx: AgentContext) -> Result<String, OrkError> {
        let mut stream = agent.send_stream(ctx.clone(), msg(ctx.task_id)).await?;
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Message(m) = ev? {
                for part in m.parts {
                    if let Part::Text { text: t, .. } = part {
                        text.push_str(&t);
                    }
                }
            }
        }
        Ok(text)
    }

    #[tokio::test]
    async fn no_tool_calls_returns_final_text() {
        let llm = Arc::new(ScriptedLlm::new(vec![vec![
            ChatStreamEvent::Delta("hello".into()),
            done(FinishReason::Stop),
        ]]));
        let agent = LocalAgent::new(
            cfg(vec![]),
            &CardEnrichmentContext::minimal(),
            llm,
            Arc::new(StubTools {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }),
        );
        assert_eq!(collect_text(&agent, ctx()).await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn tool_call_result_is_added_to_next_history() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            vec![
                ChatStreamEvent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "list_repos".into(),
                    arguments: json!({}),
                }),
                done(FinishReason::ToolCalls),
            ],
            vec![
                ChatStreamEvent::Delta("final".into()),
                done(FinishReason::Stop),
            ],
        ]));
        let agent = LocalAgent::new(
            cfg(vec!["list_repos".into()]),
            &CardEnrichmentContext::minimal(),
            llm.clone(),
            Arc::new(StubTools {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }),
        );
        assert_eq!(collect_text(&agent, ctx()).await.unwrap(), "final");
        let requests = llm.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].messages[3].tool_call_id.as_deref(),
            Some("call_1")
        );
    }

    #[tokio::test]
    async fn multiple_tool_calls_dispatch_concurrently() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            vec![
                ChatStreamEvent::ToolCall(ToolCall {
                    id: "a".into(),
                    name: "list_repos".into(),
                    arguments: json!({}),
                }),
                ChatStreamEvent::ToolCall(ToolCall {
                    id: "b".into(),
                    name: "list_repos".into(),
                    arguments: json!({}),
                }),
                ChatStreamEvent::ToolCall(ToolCall {
                    id: "c".into(),
                    name: "list_repos".into(),
                    arguments: json!({}),
                }),
                done(FinishReason::ToolCalls),
            ],
            vec![done(FinishReason::Stop)],
        ]));
        let tools = Arc::new(StubTools {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let agent = LocalAgent::new(
            cfg(vec!["list_repos".into()]),
            &CardEnrichmentContext::minimal(),
            llm,
            tools.clone(),
        );
        collect_text(&agent, ctx()).await.unwrap();
        assert!(tools.max_active.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn iteration_cap_exceeded_returns_error() {
        let llm = Arc::new(ScriptedLlm::new(vec![]));
        let mut cfg = cfg(vec!["list_repos".into()]);
        cfg.max_tool_iterations = 0;
        let agent = LocalAgent::new(
            cfg,
            &CardEnrichmentContext::minimal(),
            llm,
            Arc::new(StubTools {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }),
        );
        let err = collect_text(&agent, ctx()).await.unwrap_err();
        assert!(err.to_string().contains("tool_loop_exceeded"));
    }

    #[tokio::test]
    async fn unsupported_tool_capability_fails_before_llm_call() {
        let llm = Arc::new(ScriptedLlm {
            streams: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            capabilities: ModelCapabilities {
                supports_tools: false,
                ..ModelCapabilities::default()
            },
        });
        let agent = LocalAgent::new(
            cfg(vec!["list_repos".into()]),
            &CardEnrichmentContext::minimal(),
            llm.clone(),
            Arc::new(StubTools {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }),
        );
        let err = collect_text(&agent, ctx()).await.unwrap_err();
        assert!(err.to_string().contains("does not support tool calls"));
        assert!(llm.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cancellation_before_loop_exits_without_llm_call() {
        let llm = Arc::new(ScriptedLlm::new(Vec::new()));
        let ctx = ctx();
        ctx.cancel.cancel();
        let agent = LocalAgent::new(
            cfg(vec![]),
            &CardEnrichmentContext::minimal(),
            llm.clone(),
            Arc::new(StubTools {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }),
        );
        let err = collect_text(&agent, ctx).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"));
        assert!(llm.requests.lock().await.is_empty());
    }
}
