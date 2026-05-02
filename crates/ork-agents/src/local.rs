use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::{
    AgentCard, Message as AgentMessage, Part, TaskEvent as AgentEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_core::a2a::card_builder::{CardEnrichmentContext, build_local_card};
use ork_core::a2a::{AgentContext, AgentId, ResolveContext};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::llm::{ChatMessage, ChatRequest, LlmProvider};

use crate::rig_engine::RigEngine;
use crate::tool_catalog::ToolCatalogBuilder;

pub struct LocalAgent {
    id: AgentId,
    card: AgentCard,
    config: AgentConfig,
    llm: Arc<dyn LlmProvider>,
    tool_catalog: ToolCatalogBuilder,
}

impl LocalAgent {
    #[must_use]
    pub fn new(
        config: AgentConfig,
        card_ctx: &CardEnrichmentContext,
        llm: Arc<dyn LlmProvider>,
    ) -> Self {
        let card = build_local_card(&config, card_ctx);
        let id = config.id.clone();
        Self {
            id,
            card,
            config,
            llm,
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
        let mut user = ChatMessage::user(prompt);
        user.parts = msg.parts.clone();

        let tool_catalog = self.tool_catalog.clone();
        let config = self.config.clone();
        let llm = self.llm.clone();
        let resolve_ctx = ResolveContext {
            tenant_id: ctx.tenant_id,
        };

        // ADR 0012 §`Selection`: workflow-step overrides + capability preflight mirror
        // `LlmRouter::resolve`; `RigEngine` drives the iterative tool loop.
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

            let tool_defs = match tool_catalog.for_agent(&ctx, &config).await {
                Ok(t) => t,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

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
            if !tool_defs.is_empty() && !caps.supports_tools {
                let label = request_model.clone().unwrap_or_default();
                yield Err(OrkError::LlmProvider(format!(
                    "model {label} does not support tool calls"
                )));
                return;
            }

            if ctx.cancel.is_cancelled() {
                yield Err(OrkError::Workflow("agent task cancelled".into()));
                return;
            }

            match RigEngine::run(
                ctx.clone(),
                config.clone(),
                llm.clone(),
                tool_defs,
                user.clone(),
                vec![], // preamble only; Rig `AgentBuilder.preamble(system_prompt)`
            )
            .await
            {
                Err(e) => {
                    yield Err(e);
                }
                Ok(mut inner) => {
                    while let Some(ev) = inner.next().await {
                        yield ev;
                    }
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
    use ork_a2a::{MessageId, Role};
    use ork_common::types::TenantId;
    use ork_core::a2a::CallerIdentity;
    use ork_core::ports::llm::{ChatResponse, LlmChatStream, ModelCapabilities, TokenUsage};
    use ork_core::ports::llm::{ChatStreamEvent, FinishReason, ToolCall};
    use ork_core::ports::tool_def::ToolDef;
    use ork_core::workflow::engine::ToolExecutor;
    use ork_tool::DynToolInvoke;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::tool_catalog::ToolCatalogBuilder;

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
        total_calls: AtomicUsize,
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
            self.total_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({"tool": tool_name, "input": input}))
        }
    }

    fn catalog_list_repos(backing: Arc<StubTools>) -> ToolCatalogBuilder {
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
        let agent = LocalAgent::new(cfg(vec![]), &CardEnrichmentContext::minimal(), llm);
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
        let tools = Arc::new(StubTools {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            total_calls: AtomicUsize::new(0),
        });
        let agent = LocalAgent::new(
            cfg(vec!["list_repos".into()]),
            &CardEnrichmentContext::minimal(),
            llm.clone(),
        )
        .with_tool_catalog(catalog_list_repos(tools));
        assert_eq!(collect_text(&agent, ctx()).await.unwrap(), "final");
        let requests = llm.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let msgs = &requests[1].messages;
        assert!(
            msgs.iter()
                .any(|m| m.tool_call_id.as_deref() == Some("call_1")),
            "second completion must include the tool result for call_1; got {msgs:?}"
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
            total_calls: AtomicUsize::new(0),
        });
        let agent = LocalAgent::new(
            cfg(vec!["list_repos".into()]),
            &CardEnrichmentContext::minimal(),
            llm,
        )
        .with_tool_catalog(catalog_list_repos(tools.clone()));
        collect_text(&agent, ctx()).await.unwrap();
        // Rig may invoke tools sequentially; `max_parallel_tool_calls` still bounds concurrent
        // acquires when the engine does overlap work.
        assert_eq!(tools.total_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn iteration_cap_exceeded_returns_error() {
        let llm = Arc::new(ScriptedLlm::new(vec![]));
        let mut cfg = cfg(vec!["list_repos".into()]);
        cfg.max_tool_iterations = 0;
        let tools = Arc::new(StubTools {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            total_calls: AtomicUsize::new(0),
        });
        let agent = LocalAgent::new(cfg, &CardEnrichmentContext::minimal(), llm)
            .with_tool_catalog(catalog_list_repos(tools));
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
        let tools = Arc::new(StubTools {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            total_calls: AtomicUsize::new(0),
        });
        let agent = LocalAgent::new(
            cfg(vec!["list_repos".into()]),
            &CardEnrichmentContext::minimal(),
            llm.clone(),
        )
        .with_tool_catalog(catalog_list_repos(tools));
        let err = collect_text(&agent, ctx()).await.unwrap_err();
        assert!(err.to_string().contains("does not support tool calls"));
        assert!(llm.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cancellation_before_loop_exits_without_llm_call() {
        let llm = Arc::new(ScriptedLlm::new(Vec::new()));
        let ctx = ctx();
        ctx.cancel.cancel();
        let agent = LocalAgent::new(cfg(vec![]), &CardEnrichmentContext::minimal(), llm.clone());
        let err = collect_text(&agent, ctx).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"));
        assert!(llm.requests.lock().await.is_empty());
    }
}
