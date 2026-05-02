//! Rig-backed LLM + tool loop (ADR-0047). Bridge-only; no `rig` types leak beyond this crate.

use std::future::Future;
use std::io::{Error as IoError, ErrorKind};
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use ork_a2a::{
    FileRef, Message as AgentMessage, MessageId, Part, Role, TaskEvent as AgentEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::error::OrkError;
use ork_core::a2a::{AgentContext, ResolveContext};
use ork_core::artifact_spill::spill_bytes_to_artifact;
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::AgentEventStream;
use ork_core::ports::artifact_store::ArtifactScope;
use ork_core::ports::llm::{
    ChatMessage, ChatRequest, ChatStreamEvent, LlmProvider, MessageRole, ToolCall as OrkToolCall,
    ToolChoice as OrkToolChoice, ToolDescriptor,
};
use ork_core::ports::tool_def::ToolDef;
use rig::agent::{AgentBuilder, MultiTurnStreamItem, StreamingError, StreamingResult};
use rig::completion::message::{
    AssistantContent as RigAssistantContent, Text as RigText, ToolCall as RigAssistantToolCall,
    ToolFunction as RigToolFunction, ToolResult as RigAssistantToolResult,
    ToolResultContent as RigToolResultContent, UserContent as RigUserContent,
};
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, GetTokenUsage,
    Message as RigMessage, PromptError as RigPromptError, ToolDefinition as RigToolDefinition,
    Usage as RigUsage,
};
use rig::message::ToolChoice as RigToolChoiceEnum;
use rig::streaming::{
    RawStreamingChoice, RawStreamingToolCall, StreamedAssistantContent,
    StreamingCompletionResponse, StreamingPrompt,
};
use rig::tool::{ToolDyn, ToolError as RigToolError};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

// ----------- Fatal payload helpers -----------

#[derive(Clone, Default)]
pub(crate) struct FatalSlot(Arc<Mutex<Option<String>>>);

impl FatalSlot {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub(crate) fn set(&self, msg: String) {
        if let Ok(mut g) = self.0.lock()
            && g.is_none()
        {
            *g = Some(msg);
        }
    }

    pub(crate) fn take(&self) -> Option<String> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

pub(crate) fn truncate_tool_result(serialized: String, max_bytes: usize) -> (String, bool) {
    if serialized.len() <= max_bytes {
        return (serialized, false);
    }
    let mut out = serialized;
    out.truncate(max_bytes);
    out.push_str("\n...[truncated]");
    (out, true)
}

pub(crate) async fn try_spill_oversized_tool_result(
    ctx: &AgentContext,
    serialized: String,
) -> Option<String> {
    use bytes::Bytes;

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

pub(crate) fn tool_error_payload(
    call_name: &str,
    err: &OrkError,
    max_tool_result_bytes: usize,
) -> String {
    let payload = serde_json::json!({
        "error": {
            "tool": call_name,
            "message": err.to_string(),
        }
    });
    let serialized = serde_json::to_string(&payload).unwrap_or_else(|_| {
        format!("{{\"error\":{{\"tool\":\"{call_name}\",\"message\":\"<unserializable>\"}}}}")
    });
    truncate_tool_result(serialized, max_tool_result_bytes).0
}

pub(crate) fn status_update_text(
    task_id: ork_a2a::TaskId,
    text: impl Into<String>,
    is_final: bool,
) -> AgentEvent {
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

pub(crate) struct OrkToolDyn {
    pub(crate) tool: Arc<dyn ToolDef>,
    pub(crate) ctx: AgentContext,
    pub(crate) fatal: FatalSlot,
    pub(crate) semaphore: Arc<Semaphore>,
    pub(crate) max_tool_result_bytes: usize,
}

impl ToolDyn for OrkToolDyn {
    fn name(&self) -> String {
        self.tool.id().to_string()
    }

    fn definition<'f>(
        &'f self,
        _prompt: String,
    ) -> rig::wasm_compat::WasmBoxedFuture<'f, RigToolDefinition> {
        let d = RigToolDefinition {
            name: self.tool.id().to_string(),
            description: self.tool.description().to_string(),
            parameters: self.tool.input_schema().clone(),
        };
        Box::pin(async move { d })
    }

    fn call<'f>(
        &'f self,
        args: String,
    ) -> rig::wasm_compat::WasmBoxedFuture<'f, Result<String, RigToolError>> {
        let tool = self.tool.clone();
        let ctx = self.ctx.clone();
        let semaphore = self.semaphore.clone();
        let fatal = self.fatal.clone();
        let max = self.max_tool_result_bytes;

        Box::pin(async move {
            let call_name = tool.id().to_string();

            if ctx.cancel.is_cancelled() {
                fatal.set("agent task cancelled".into());
                return Err(RigToolError::ToolCallError(Box::new(IoError::new(
                    ErrorKind::Interrupted,
                    "cancelled",
                ))));
            }

            let parsed = match serde_json::from_str::<serde_json::Value>(&args) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(tool_error_payload(
                        &call_name,
                        &OrkError::Validation(e.to_string()),
                        max,
                    ));
                }
            };

            let _permit = semaphore.acquire_owned().await.map_err(|_| {
                RigToolError::ToolCallError(Box::new(IoError::other("tool semaphore closed")))
            })?;

            if ctx.cancel.is_cancelled() {
                fatal.set("agent task cancelled".into());
                return Err(RigToolError::ToolCallError(Box::new(IoError::new(
                    ErrorKind::Interrupted,
                    "cancelled",
                ))));
            }

            match tool.invoke(&ctx, &parsed).await {
                Ok(out) => match serde_json::to_string(&out) {
                    Ok(serialized) => {
                        if serialized.len() > max {
                            if let Some(sp) =
                                try_spill_oversized_tool_result(&ctx, serialized.clone()).await
                            {
                                Ok(sp)
                            } else {
                                Ok(truncate_tool_result(serialized, max).0)
                            }
                        } else {
                            Ok(serialized)
                        }
                    }
                    Err(e) => Err(RigToolError::JsonError(e)),
                },
                Err(e) if !tool.is_fatal(&e) => Ok(tool_error_payload(&call_name, &e, max)),
                Err(e) => {
                    fatal.set(e.to_string());
                    ctx.cancel.cancel();
                    Err(RigToolError::ToolCallError(Box::new(IoError::other(
                        e.to_string(),
                    ))))
                }
            }
        })
    }
}

#[derive(Clone)]
pub(crate) struct LlmProviderCompletionModel {
    inner: Arc<dyn LlmProvider>,
    request_provider: Option<String>,
    request_model: Option<String>,
    resolve_ctx: ResolveContext,
}

impl LlmProviderCompletionModel {
    pub(crate) fn new(
        inner: Arc<dyn LlmProvider>,
        request_provider: Option<String>,
        request_model: Option<String>,
        resolve_ctx: ResolveContext,
    ) -> Self {
        Self {
            inner,
            request_provider,
            request_model,
            resolve_ctx,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OrkStreamingMeta {
    #[serde(rename = "usage_tokens")]
    pub(crate) usage: RigUsage,
}

impl GetTokenUsage for OrkStreamingMeta {
    fn token_usage(&self) -> Option<RigUsage> {
        Some(self.usage)
    }
}

/// Bridges [`LlmProvider::chat_stream`] to rig's `CompletionModel` trait.
///
/// ADR-0047 `Decision` shows pseudocode `type Response = OrkCompletionResponse`; in production
/// ork only wires Rig `AgentBuilder::stream_prompt`, which invokes `CompletionModel::stream`, not
/// `CompletionModel::completion`. We therefore use `type Response = ()` and return
/// [`CompletionError::ProviderError`] from [`Self::completion`] so any accidental non-streaming
/// call fails loudly. Streaming metadata (token usage) rides on [`OrkStreamingMeta`] as
/// `type StreamingResponse`.
impl CompletionModel for LlmProviderCompletionModel {
    type Response = ();
    type StreamingResponse = OrkStreamingMeta;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unreachable!("constructed explicitly")
    }

    #[allow(clippy::manual_async_fn)]
    fn completion(
        &self,
        _: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse<Self::Response>, CompletionError>>
    + rig::wasm_compat::WasmCompatSend {
        async move {
            Err(CompletionError::ProviderError(
                "ork LlmProviderCompletionModel only supports CompletionModel::stream (chat_stream); \
                 completion() is not used by RigEngine".into(),
            ))
        }
    }

    fn stream(
        &self,
        request: CompletionRequest,
    ) -> impl Future<
        Output = Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError>,
    > + rig::wasm_compat::WasmCompatSend {
        let llm = self.inner.clone();
        let rp = self.request_provider.clone();
        let rm = self.request_model.clone();
        let rx = self.resolve_ctx;

        async move {
            let chat = rig_completion_request_to_chat(request, rp, rm)?;
            let upstream = rx
                .scope(llm.chat_stream(chat))
                .await
                .map_err(|e| CompletionError::ProviderError(format!("chat_stream scope: {e}")))?;

            let stream = async_stream::stream! {
                futures::pin_mut!(upstream);
                while let Some(next) = upstream.next().await {
                    match next {
                        Ok(ChatStreamEvent::Delta(d)) if !d.is_empty() => {
                            yield Ok(RawStreamingChoice::Message(d));
                        }
                        Ok(ChatStreamEvent::ToolCall(tc)) => {
                            yield Ok(RawStreamingChoice::ToolCall(
                                RawStreamingToolCall::new(
                                    tc.id.clone(),
                                    tc.name,
                                    tc.arguments,
                                )
                                .with_call_id(tc.id),
                            ));
                        }
                        Ok(ChatStreamEvent::ToolCallDelta { .. }) => {}
                        Ok(ChatStreamEvent::Done { usage, .. }) => {
                            let mut u = RigUsage::new();
                            u.input_tokens = u64::from(usage.prompt_tokens);
                            u.output_tokens = u64::from(usage.completion_tokens);
                            u.total_tokens = u64::from(usage.total_tokens);
                            yield Ok(RawStreamingChoice::FinalResponse(OrkStreamingMeta { usage: u }));
                        }
                        Ok(ChatStreamEvent::Delta(_)) => {}
                        Err(e) => yield Err(CompletionError::ProviderError(e.to_string())),
                    }
                }
            };

            Ok(StreamingCompletionResponse::stream(Box::pin(stream)))
        }
    }
}

fn rig_tool_choice_to_ork(
    tc: Option<RigToolChoiceEnum>,
    tools_non_empty: bool,
) -> Option<OrkToolChoice> {
    Some(
        match tc.unwrap_or(if tools_non_empty {
            RigToolChoiceEnum::Auto
        } else {
            RigToolChoiceEnum::None
        }) {
            RigToolChoiceEnum::Auto => OrkToolChoice::Auto,
            RigToolChoiceEnum::None => OrkToolChoice::None,
            RigToolChoiceEnum::Required => OrkToolChoice::Required,
            RigToolChoiceEnum::Specific { function_names } => {
                match function_names.into_iter().next() {
                    Some(name) => OrkToolChoice::Named { name },
                    None => OrkToolChoice::Required,
                }
            }
        },
    )
}

#[allow(clippy::too_many_lines)]
fn rig_completion_request_to_chat(
    req: CompletionRequest,
    fallback_provider: Option<String>,
    fallback_model: Option<String>,
) -> Result<ChatRequest, CompletionError> {
    let preamble = req.preamble.clone();

    let mut messages = Vec::<ChatMessage>::new();

    if let Some(p) = preamble.filter(|s| !s.trim().is_empty()) {
        messages.push(ChatMessage::system(p));
    }

    if !req.documents.is_empty() {
        let mut body = String::new();
        for d in req.documents.iter() {
            body.push_str(&format!(
                concat!("<document id:", "{id}", ">\n", "{txt}", "</document>\n"),
                id = d.id,
                txt = d.text
            ));
        }
        messages.push(ChatMessage::user(body));
    }

    for rm in req.chat_history.into_iter() {
        match rm {
            RigMessage::System { content } => {
                if !content.trim().is_empty() {
                    messages.push(ChatMessage::system(content));
                }
            }
            RigMessage::User { content } => messages.extend(one_or_many_user_to_ork(&content)?),
            RigMessage::Assistant { id: _, content } => {
                messages.extend(rig_assistant_to_ork(&content)?)
            }
        }
    }

    let ork_tools: Vec<ToolDescriptor> = req
        .tools
        .into_iter()
        .map(|t| ToolDescriptor {
            name: t.name,
            description: t.description,
            parameters: t.parameters,
        })
        .collect();

    Ok(ChatRequest {
        messages,
        temperature: req.temperature.map(|x| x as f32),
        max_tokens: req.max_tokens.map(|x| x as u32),
        model: req.model.or(fallback_model),
        provider: fallback_provider,
        tools: ork_tools.clone(),
        tool_choice: rig_tool_choice_to_ork(req.tool_choice, !ork_tools.is_empty()),
    })
}

fn one_or_many_user_to_ork(
    content: &rig::OneOrMany<RigUserContent>,
) -> Result<Vec<ChatMessage>, CompletionError> {
    let mut out = Vec::<ChatMessage>::new();
    for item in content.clone().into_iter() {
        match item {
            RigUserContent::Text(t) => {
                let mut m = ChatMessage::user(t.text.clone());
                m.content = t.text;
                out.push(m);
            }
            RigUserContent::ToolResult(tr) => {
                let id = tr.call_id.clone().unwrap_or_else(|| tr.id.clone());
                let body = concat_tool_result(tr.content.clone());
                out.push(ChatMessage::tool(id, body.clone()));
                if let Some(last) = out.last_mut() {
                    last.content = body;
                }
            }
            RigUserContent::Image(_)
            | RigUserContent::Audio(_)
            | RigUserContent::Video(_)
            | RigUserContent::Document(_) => {
                return Err(CompletionError::ProviderError(
                    "unsupported multimodal user content in Rig→ork adapter".into(),
                ));
            }
        }
    }
    Ok(out)
}

fn concat_tool_result(content: rig::OneOrMany<RigToolResultContent>) -> String {
    let mut s = String::new();
    for p in content {
        match p {
            RigToolResultContent::Text(t) => s.push_str(&t.text),
            RigToolResultContent::Image(_) => s.push_str("<image>"),
        }
    }
    s
}

fn rig_assistant_to_ork(
    content: &rig::OneOrMany<RigAssistantContent>,
) -> Result<Vec<ChatMessage>, CompletionError> {
    let mut txt = String::new();
    let mut tool_calls = Vec::<OrkToolCall>::new();
    for piece in content.clone().into_iter() {
        match piece {
            RigAssistantContent::Text(t) => txt.push_str(&t.text),
            RigAssistantContent::ToolCall(tc) => {
                tool_calls.push(OrkToolCall {
                    id: tc.id,
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                });
            }
            RigAssistantContent::Reasoning(_) | RigAssistantContent::Image(_) => {}
        }
    }
    Ok(vec![ChatMessage::assistant(txt, tool_calls)])
}

pub(crate) fn chat_message_to_rig(msg: ChatMessage) -> Result<RigMessage, OrkError> {
    Ok(match msg.role {
        MessageRole::System => RigMessage::System {
            content: msg.content,
        },
        MessageRole::User => {
            let mut parts = Vec::new();
            if !msg.content.is_empty() {
                parts.push(RigUserContent::Text(RigText {
                    text: msg.content.clone(),
                }));
            }
            for part in msg.parts {
                match part {
                    Part::Text { text, .. } => parts.push(RigUserContent::Text(RigText { text })),
                    Part::Data { data, .. } => parts.push(RigUserContent::Text(RigText {
                        text: serde_json::to_string(&data).unwrap_or_else(|_| "null".into()),
                    })),
                    Part::File { .. } => {
                        return Err(OrkError::Validation(
                            "file parts are not supported in LocalAgent yet (TODO(ADR-0003/0016))"
                                .into(),
                        ));
                    }
                }
            }
            RigMessage::User {
                content: rig::OneOrMany::many(parts).map_err(|_| {
                    OrkError::Validation(
                        "user ChatMessage yielded no textual content for Rig".into(),
                    )
                })?,
            }
        }
        MessageRole::Assistant => {
            let mut contents = Vec::new();
            if !msg.content.is_empty() {
                contents.push(RigAssistantContent::Text(RigText {
                    text: msg.content.clone(),
                }));
            }
            for tc in msg.tool_calls {
                contents.push(RigAssistantContent::ToolCall(RigAssistantToolCall::new(
                    tc.id.clone(),
                    RigToolFunction::new(tc.name.clone(), tc.arguments.clone()),
                )));
            }
            RigMessage::Assistant {
                id: None,
                content: rig::OneOrMany::many(contents).map_err(|_| {
                    OrkError::Validation("assistant ChatMessage has no Rig content".into())
                })?,
            }
        }
        MessageRole::Tool => {
            let cid = msg
                .tool_call_id
                .clone()
                .ok_or_else(|| OrkError::Validation("missing tool_call_id".into()))?;
            let res = RigAssistantToolResult {
                id: cid.clone(),
                call_id: Some(cid),
                content: rig::OneOrMany::one(RigToolResultContent::Text(RigText {
                    text: msg.content,
                })),
            };
            RigMessage::User {
                content: rig::OneOrMany::one(RigUserContent::ToolResult(res)),
            }
        }
    })
}

pub(crate) struct RigEngine;

impl RigEngine {
    pub(crate) async fn run(
        ctx: AgentContext,
        config: AgentConfig,
        llm: Arc<dyn LlmProvider>,
        tool_defs: Vec<Arc<dyn ToolDef>>,
        prompt: ChatMessage,
        history_seed: Vec<ChatMessage>,
    ) -> Result<AgentEventStream, OrkError> {
        if config.max_tool_iterations == 0 {
            let (tx, rx) = mpsc::channel::<Result<AgentEvent, OrkError>>(4);
            let task_id = ctx.task_id;
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(status_update_text(task_id, "tool loop exceeded", false)))
                    .await;
                let _ = tx
                    .send(Err(OrkError::Workflow("tool_loop_exceeded".into())))
                    .await;
            });
            return Ok(Box::pin(ReceiverStream::new(rx)));
        }

        let step_ov = ctx.step_llm_overrides.clone();
        let request_provider = step_ov
            .as_ref()
            .and_then(|o| o.provider.clone())
            .or_else(|| config.provider.clone());
        let request_model = step_ov
            .as_ref()
            .and_then(|o| o.model.clone())
            .or_else(|| config.model.clone());

        let resolve_ctx = ResolveContext {
            tenant_id: ctx.tenant_id,
        };
        let adapter =
            LlmProviderCompletionModel::new(llm, request_provider, request_model, resolve_ctx);

        let fatal = FatalSlot::new();
        let semaphore = Arc::new(Semaphore::new(config.max_parallel_tool_calls.max(1)));

        let mut rig_tools = Vec::<Box<dyn ToolDyn>>::new();
        for def in tool_defs {
            rig_tools.push(Box::new(OrkToolDyn {
                tool: def,
                ctx: ctx.clone(),
                fatal: fatal.clone(),
                semaphore: semaphore.clone(),
                max_tool_result_bytes: config.max_tool_result_bytes,
            }));
        }

        let mut hist = Vec::<RigMessage>::new();
        for m in history_seed {
            hist.push(chat_message_to_rig(m)?);
        }

        let user_prompt_message = chat_message_to_rig(prompt)?;
        let max_turn = config.max_tool_iterations;
        let cancel = ctx.cancel.clone();
        let task_id = ctx.task_id;
        let context_id = ctx.context_id;
        let expose = config.expose_reasoning;

        let agent = AgentBuilder::new(adapter)
            .preamble(&config.system_prompt)
            .temperature(f64::from(config.temperature))
            .max_tokens(u64::from(config.max_tokens))
            .tools(rig_tools)
            .build();

        let outer_stream = agent
            .stream_prompt(user_prompt_message)
            .with_history(hist.clone())
            .multi_turn(max_turn)
            .await;

        let (tx, rx) = mpsc::channel::<Result<AgentEvent, OrkError>>(128);
        let agent_id_line = config.id.clone();
        tokio::spawn(run_rig_consumer(
            outer_stream,
            cancel,
            fatal,
            tx,
            task_id,
            context_id,
            expose,
            agent_id_line,
        ));

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_rig_consumer(
    mut outer: StreamingResult<OrkStreamingMeta>,
    cancel: tokio_util::sync::CancellationToken,
    fatal: FatalSlot,
    tx: mpsc::Sender<Result<AgentEvent, OrkError>>,
    task_id: ork_a2a::TaskId,
    context_id: Option<ork_a2a::ContextId>,
    expose_reasoning: bool,
    agent_id: String,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let err = match fatal.take() {
                    Some(msg) => OrkError::Workflow(msg),
                    None => OrkError::Workflow("agent task cancelled".into()),
                };
                let _ = tx.send(Err(err)).await;
                break;
            }
            item = outer.next() => {
                let Some(next) = item else {
                    break;
                };
                match next {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(slice)) => {
                        match slice {
                            StreamedAssistantContent::Text(t) if !t.text.is_empty() => {
                                let _ = tx
                                    .send(Ok(status_update_text(task_id, t.text.clone(), false)))
                                    .await;
                            }
                            StreamedAssistantContent::Reasoning(r) if expose_reasoning => {
                                let t = r.display_text();
                                if !t.is_empty() {
                                    let _ = tx
                                        .send(Ok(status_update_text(task_id, t, false)))
                                        .await;
                                }
                            }
                            StreamedAssistantContent::ReasoningDelta { reasoning, .. }
                                if expose_reasoning =>
                            {
                                if !reasoning.is_empty() {
                                    let _ = tx
                                        .send(Ok(status_update_text(
                                            task_id,
                                            reasoning.clone(),
                                            false,
                                        )))
                                        .await;
                                }
                            }
                            StreamedAssistantContent::ToolCall { .. }
                            | StreamedAssistantContent::ToolCallDelta { .. }
                            | StreamedAssistantContent::Final(_) => {}
                            StreamedAssistantContent::Reasoning(_)
                            | StreamedAssistantContent::ReasoningDelta { .. }
                                if !expose_reasoning => {}
                            _ => {}
                        }
                    }
                    Ok(MultiTurnStreamItem::StreamUserItem(_)) => {}
                    Ok(MultiTurnStreamItem::FinalResponse(fr)) => {
                        if cancel.is_cancelled() {
                            let err = match fatal.take() {
                                Some(msg) => OrkError::Workflow(msg),
                                None => OrkError::Workflow("agent task cancelled".into()),
                            };
                            let _ = tx.send(Err(err)).await;
                            break;
                        }
                        if let Some(msg) = fatal.take() {
                            let _ = tx.send(Err(OrkError::Workflow(msg))).await;
                            break;
                        }
                        info!(
                            agent = %agent_id,
                            tool_calls = "?",
                            iterations = "?",
                            tool_loop_exceeded = false,
                            "TODO(ADR-0022): agent tool loop telemetry",
                        );
                        let txt = fr.response().to_string();
                        let msg = AgentMessage {
                            role: Role::Agent,
                            parts: vec![Part::Text { text: txt, metadata: None }],
                            message_id: MessageId::new(),
                            task_id: Some(task_id),
                            context_id,
                            metadata: None,
                        };
                        let _ = tx.send(Ok(AgentEvent::Message(msg))).await;
                        let _ = tx.send(Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
                            task_id,
                            status: TaskStatus {
                                state: TaskState::Completed,
                                message: None,
                            },
                            is_final: true,
                        }))).await;
                        break;
                    }
                    Ok(_) => {}
                    Err(stream_err) => {
                        handle_stream_err(stream_err, &fatal, &tx, task_id).await;
                        break;
                    }
                }
            }
        }
    }
}

async fn handle_stream_err(
    e: StreamingError,
    fatal: &FatalSlot,
    tx: &mpsc::Sender<Result<AgentEvent, OrkError>>,
    task_id: ork_a2a::TaskId,
) {
    match e {
        StreamingError::Prompt(boxed_p) => match &*boxed_p {
            RigPromptError::MaxTurnsError { .. } => {
                let _ = tx
                    .send(Ok(status_update_text(task_id, "tool loop exceeded", false)))
                    .await;
                let _ = tx
                    .send(Err(OrkError::Workflow("tool_loop_exceeded".into())))
                    .await;
            }
            _ => {
                if let Some(tool_msg) = fatal.take() {
                    let _ = tx.send(Err(OrkError::Workflow(tool_msg))).await;
                } else {
                    let _ = tx
                        .send(Err(OrkError::LlmProvider(boxed_p.to_string())))
                        .await;
                }
            }
        },
        other => {
            let _ = tx.send(Err(stream_other_to_ork(other))).await;
        }
    }
}

fn stream_other_to_ork(other: StreamingError) -> OrkError {
    match other {
        StreamingError::Completion(c) => OrkError::LlmProvider(c.to_string()),
        StreamingError::Tool(t) => OrkError::Workflow(t.to_string()),
        StreamingError::Prompt(p) => OrkError::LlmProvider(p.to_string()),
    }
}
