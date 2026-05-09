//! A2A HTTP surface.
//!
//! Mounts the public well-known card endpoints (ADR-0005 §`HTTP well-known endpoints`)
//! and the authenticated JSON-RPC dispatcher / SSE bridge per ADR-0008.
//!
//! - **Public**: `/.well-known/agent-card.json` (default agent) and
//!   `/a2a/agents/{agent_id}/.well-known/agent-card.json` (per-agent).
//! - **Protected**: `POST /a2a/agents/{agent_id}` for JSON-RPC dispatch and the SSE
//!   resume bridge (added with the bridge handler in Task 12).
//!
//! The well-known handlers take a small [`WellKnownState`] (registry + default agent id)
//! rather than the full `AppState` so integration tests can spin them up without a
//! Postgres-backed workflow stack. The JSON-RPC dispatcher needs the full `AppState`
//! because individual handlers reach into the task repo, push repo, registry, and SSE
//! buffer.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use ork_a2a::{
    A2aMethod, AgentCard, JsonRpcError, JsonRpcRequest, JsonRpcResponse, Message as A2aMessage,
    MessageSendParams, Part, Role, SendMessageResult, Task, TaskId, TaskState, TaskStatus,
};
use ork_common::auth::{OPS_READ_SCOPE, agent_cancel_scope, agent_invoke_scope};
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::agent_registry::AgentRegistry;
use ork_core::embeds::EmbedContext;
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRow};
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::streaming::late_embed::LateEmbedResolver;
use ork_storage::ScopeCheckedArtifactStore;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::artifact_inbound;
use crate::middleware::AuthContext;
use crate::require_scope;
use crate::state::AppState;

/// ADR-0021 §`Decision points` step 4: wrap the raw `state.artifact_store`
/// with a [`ScopeCheckedArtifactStore`] keyed on the caller's scope set.
/// Built per request so a long-lived `AppState` cannot leak the wrong
/// scope into an `AgentContext`. Returns `None` when the operator has
/// not configured artifacts.
fn scoped_artifact_store(state: &AppState, auth: &AuthContext) -> Option<Arc<dyn ArtifactStore>> {
    state.artifact_store.as_ref().map(|raw| {
        Arc::new(ScopeCheckedArtifactStore::new(
            raw.clone(),
            auth.scopes.clone(),
        )) as Arc<dyn ArtifactStore>
    })
}

/// Minimal state slice the well-known handlers need.
#[derive(Clone)]
pub struct WellKnownState {
    pub agent_registry: Arc<AgentRegistry>,
    pub default_agent_id: Option<String>,
}

impl WellKnownState {
    #[must_use]
    pub fn from_app_state(state: &AppState) -> Self {
        Self {
            agent_registry: state.agent_registry.clone(),
            default_agent_id: state.config.discovery.default_agent_id.clone(),
        }
    }
}

pub fn well_known_routes(state: AppState) -> Router {
    well_known_router(WellKnownState::from_app_state(&state))
}

pub fn well_known_router(state: WellKnownState) -> Router {
    Router::new()
        .route("/.well-known/agent-card.json", get(default_agent_card))
        .route(
            "/a2a/agents/{agent_id}/.well-known/agent-card.json",
            get(per_agent_card),
        )
        .with_state(state)
}

async fn default_agent_card(State(state): State<WellKnownState>) -> impl IntoResponse {
    let Some(default_id) = state.default_agent_id.as_ref() else {
        return (StatusCode::NOT_FOUND, "no default agent configured").into_response();
    };
    match state.agent_registry.card_for(default_id).await {
        Some(card) => Json::<AgentCard>(card).into_response(),
        None => (StatusCode::NOT_FOUND, "default agent not registered").into_response(),
    }
}

async fn per_agent_card(
    Path(agent_id): Path<String>,
    State(state): State<WellKnownState>,
) -> impl IntoResponse {
    match state.agent_registry.card_for(&agent_id).await {
        Some(card) => Json::<AgentCard>(card).into_response(),
        None => (StatusCode::NOT_FOUND, "agent not found").into_response(),
    }
}

// =====================================================================================
// ADR-0008 §`JSON-RPC dispatcher` — authenticated routes
// =====================================================================================

/// Mounts the authenticated A2A surface: the JSON-RPC dispatcher.
///
/// SSE bridge (`GET /a2a/agents/{agent_id}/stream/{task_id}`) and convenience
/// REST endpoints (Tasks 12 and 14) chain off the same `AppState`.
pub fn protected_routes(state: AppState) -> Router {
    protected_router(state)
}

/// Internal `Router` builder; kept separate so integration tests can build the same
/// router from a manually-constructed `AppState`.
pub fn protected_router(state: AppState) -> Router {
    Router::new()
        .route("/a2a/agents/{agent_id}", post(jsonrpc_dispatch))
        .route(
            "/a2a/agents/{agent_id}/stream/{task_id}",
            get(handle_stream_replay),
        )
        .route("/a2a/agents", get(list_agents))
        .route("/a2a/tasks/{task_id}", get(lookup_task))
        .with_state(state)
}

/// ADR-0008 §convenience endpoints: `GET /a2a/agents` returns every card the
/// registry knows about (local + remote), in the same shape as the per-agent
/// well-known endpoint, for catalog UIs and external discovery.
///
/// ADR-0021 §`Vocabulary` — admin / catalog views are gated on `ops:read`.
async fn list_agents(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> impl IntoResponse {
    require_scope!(auth, OPS_READ_SCOPE);
    let cards: Vec<AgentCard> = state.agent_registry.list_cards().await;
    Json(cards).into_response()
}

/// ADR-0008 §convenience endpoints: `GET /a2a/tasks/{task_id}` is a cross-agent
/// lookup that returns the same `Task` shape `tasks/get` produces, but as a
/// REST GET so admin tools can dereference task ids without wrapping them in a
/// JSON-RPC envelope. Bad UUIDs are `400`; cross-tenant or missing tasks are
/// `404`.
async fn lookup_task(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(task_id_str): Path<String>,
) -> impl IntoResponse {
    require_scope!(auth, OPS_READ_SCOPE);
    let Ok(uuid) = uuid::Uuid::parse_str(&task_id_str) else {
        return (StatusCode::BAD_REQUEST, "bad task id").into_response();
    };
    let task_id = TaskId(uuid);
    match state.a2a_task_repo.get_task(auth.tenant_id, task_id).await {
        Ok(Some(_)) => Json(build_task_response(&state, &auth, task_id).await).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "task not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Single entry point for all six A2A JSON-RPC methods (ADR-0008 §`JSON-RPC dispatcher`).
///
/// Parses the envelope as untyped JSON first so we can preserve `id` for the error
/// response when validation fails, then dispatches on the parsed [`A2aMethod`]. Each
/// per-method handler is responsible for its own typed `params` parse so we can return
/// `INVALID_PARAMS` (`-32602`) close to where the parse fails.
async fn jsonrpc_dispatch(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(agent_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let envelope: JsonRpcRequest<serde_json::Value> = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            return Json(JsonRpcResponse::<serde_json::Value>::err(
                None,
                JsonRpcError {
                    code: JsonRpcError::PARSE_ERROR,
                    message: e.to_string(),
                    data: None,
                },
            ))
            .into_response();
        }
    };
    if let Err(err) = envelope.validate() {
        return Json(JsonRpcResponse::<serde_json::Value>::err(
            envelope.id.clone(),
            err,
        ))
        .into_response();
    }
    let method = match A2aMethod::from_str(&envelope.method) {
        Ok(m) => m,
        Err(_) => {
            return Json(JsonRpcResponse::<serde_json::Value>::err(
                envelope.id.clone(),
                JsonRpcError::method_not_found(&envelope.method),
            ))
            .into_response();
        }
    };

    match method {
        A2aMethod::MessageSend => handle_message_send(&state, &auth, &agent_id, envelope).await,
        A2aMethod::MessageStream => {
            handle_message_stream(&state, &auth, &agent_id, envelope, &headers).await
        }
        A2aMethod::TasksGet => handle_tasks_get(&state, &auth, envelope).await,
        A2aMethod::TasksCancel => handle_tasks_cancel(&state, &auth, &agent_id, envelope).await,
        A2aMethod::TasksPushNotificationConfigSet => handle_push_set(&state, &auth, envelope).await,
        A2aMethod::TasksPushNotificationConfigGet => handle_push_get(&state, &auth, envelope).await,
    }
}

// --- Per-method stubs (bodies land in Tasks 8-11, 13). -------------------------------
//
// Stubs return `INTERNAL_ERROR` with a "not yet implemented" message so that the
// dispatcher envelope shape is testable in Task 7 without committing to handler
// behaviour. Each stub is replaced one-for-one in its dedicated task.

/// ADR-0008 §`message/send`: persist task + inbound user message, invoke the agent,
/// persist the agent reply, and return a populated `Task` to the caller.
///
/// The handler is intentionally synchronous from the caller's perspective: A2A 1.0
/// `message/send` returns either a final `Message` or a `Task`; we always return
/// `Task` here so callers see the persistent ledger entry. Streaming is a separate
/// method (`message/stream`, Task 11).
async fn handle_message_send(
    state: &AppState,
    auth: &AuthContext,
    agent_id: &str,
    env: JsonRpcRequest<serde_json::Value>,
) -> axum::response::Response {
    // ADR-0021 §`Decision points` step 1.
    require_scope!(*auth, agent_invoke_scope(agent_id));
    let params: MessageSendParams = match parse_params(&env) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let agent = match state.agent_registry.resolve(&agent_id.to_string()).await {
        Some(a) => a,
        None => {
            return Json(JsonRpcResponse::<serde_json::Value>::err(
                env.id,
                JsonRpcError::agent_not_found(agent_id),
            ))
            .into_response();
        }
    };

    let task_id = TaskId::new();
    let context_id = params.message.context_id.unwrap_or_default();
    let now = chrono::Utc::now();

    if let Err(e) = state
        .a2a_task_repo
        .create_task(&A2aTaskRow {
            id: task_id,
            context_id,
            tenant_id: auth.tenant_id,
            agent_id: agent_id.to_string(),
            parent_task_id: None,
            workflow_run_id: None,
            state: TaskState::Submitted,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
        .await
    {
        return internal_err(env.id, e);
    }

    let parts = match (&state.artifact_store, &state.artifact_meta) {
        (Some(store), Some(meta)) => {
            match artifact_inbound::rewrite_inbound_file_parts(
                store,
                meta,
                &state.artifact_public_base,
                auth.tenant_id,
                context_id,
                task_id,
                params.message.message_id,
                params.message.parts.clone(),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => return internal_err(env.id, e),
            }
        }
        _ => params.message.parts.clone(),
    };

    // Persist the inbound user turn. Best-effort: a failure here should not abort
    // the call (the agent invocation has not run yet); we log and move on.
    if let Err(e) = state
        .a2a_task_repo
        .append_message(&A2aMessageRow {
            id: params.message.message_id,
            task_id,
            role: role_to_wire(params.message.role).to_string(),
            parts: serde_json::to_value(&parts).unwrap_or(serde_json::Value::Null),
            metadata: serde_json::json!({}),
            created_at: now,
        })
        .await
    {
        tracing::warn!(error = %e, "ADR-0008: failed to persist inbound user message");
    }

    let inbound = A2aMessage {
        role: params.message.role,
        parts,
        message_id: params.message.message_id,
        task_id: Some(task_id),
        context_id: Some(context_id),
        metadata: params.message.metadata.clone(),
    };

    let ctx = AgentContext {
        tenant_id: auth.tenant_id,
        task_id,
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: caller_identity(auth),
        push_notification_url: None,
        trace_ctx: None,
        context_id: Some(context_id),
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: scoped_artifact_store(state, auth),
        artifact_public_base: state
            .artifact_store
            .as_ref()
            .map(|_| state.artifact_public_base.clone()),
        resource_id: None,
        thread_id: None,
    };

    let result = agent.send(ctx, inbound).await;
    let final_state = match &result {
        Ok(_) => TaskState::Completed,
        Err(_) => TaskState::Failed,
    };
    if let Err(e) = state
        .a2a_task_repo
        .update_state(auth.tenant_id, task_id, final_state)
        .await
    {
        tracing::warn!(error = %e, "ADR-0008: failed to update task state after agent run");
    }
    // ADR-0009: terminal state — publish to the push outbox so the worker can
    // fan the notification out to every registered subscriber. Best-effort.
    state
        .push_service
        .publish_terminal(auth.tenant_id, task_id, final_state)
        .await;

    match result {
        Ok(reply) => {
            if let Err(e) = state
                .a2a_task_repo
                .append_message(&A2aMessageRow {
                    id: reply.message_id,
                    task_id,
                    role: role_to_wire(reply.role).to_string(),
                    parts: serde_json::to_value(&reply.parts).unwrap_or(serde_json::Value::Null),
                    metadata: serde_json::json!({}),
                    created_at: chrono::Utc::now(),
                })
                .await
            {
                tracing::warn!(error = %e, "ADR-0008: failed to persist agent reply");
            }
            let task = build_task_response(state, auth, task_id).await;
            Json(JsonRpcResponse::ok(env.id, SendMessageResult::Task(task))).into_response()
        }
        Err(e) => internal_err(env.id, e),
    }
}

/// ADR-0008 §`message/stream`: same wire surface as [`handle_message_send`] but
/// the response is a `text/event-stream` of `JsonRpcResponse<TaskEvent>` chunks.
///
/// Each event is teed three ways:
/// 1. Out to the SSE response body (the live tail this caller is watching).
/// 2. Into the SSE replay buffer ([`SseBuffer::append`]) so a reconnecting
///    client with `Last-Event-ID` can resume within the buffer window.
/// 3. Into the Kafka `agent.status.<task_id>` topic so the SSE bridge in Task
///    12 (and any external observer) sees the same canonical sequence.
///
/// Persistence is best-effort up-front (create task, append inbound message);
/// per-event state mutations stay async to keep the SSE write loop tight.
async fn handle_message_stream(
    state: &AppState,
    auth: &AuthContext,
    agent_id: &str,
    env: JsonRpcRequest<serde_json::Value>,
    _headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;

    // ADR-0021 §`Decision points` step 1.
    require_scope!(*auth, agent_invoke_scope(agent_id));
    let params: MessageSendParams = match parse_params(&env) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let agent = match state.agent_registry.resolve(&agent_id.to_string()).await {
        Some(a) => a,
        None => {
            return Json(JsonRpcResponse::<serde_json::Value>::err(
                env.id,
                JsonRpcError::agent_not_found(agent_id),
            ))
            .into_response();
        }
    };

    let task_id = TaskId::new();
    let context_id = params.message.context_id.unwrap_or_default();
    let now = chrono::Utc::now();

    if let Err(e) = state
        .a2a_task_repo
        .create_task(&A2aTaskRow {
            id: task_id,
            context_id,
            tenant_id: auth.tenant_id,
            agent_id: agent_id.to_string(),
            parent_task_id: None,
            workflow_run_id: None,
            state: TaskState::Working,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
        .await
    {
        return internal_err(env.id, e);
    }

    let parts = match (&state.artifact_store, &state.artifact_meta) {
        (Some(store), Some(meta)) => {
            match artifact_inbound::rewrite_inbound_file_parts(
                store,
                meta,
                &state.artifact_public_base,
                auth.tenant_id,
                context_id,
                task_id,
                params.message.message_id,
                params.message.parts.clone(),
            )
            .await
            {
                Ok(p) => p,
                Err(e) => return internal_err(env.id, e),
            }
        }
        _ => params.message.parts.clone(),
    };

    if let Err(e) = state
        .a2a_task_repo
        .append_message(&A2aMessageRow {
            id: params.message.message_id,
            task_id,
            role: role_to_wire(params.message.role).to_string(),
            parts: serde_json::to_value(&parts).unwrap_or(serde_json::Value::Null),
            metadata: serde_json::json!({}),
            created_at: now,
        })
        .await
    {
        tracing::warn!(error = %e, "ADR-0008: failed to persist inbound stream message");
    }

    let inbound = A2aMessage {
        role: params.message.role,
        parts,
        message_id: params.message.message_id,
        task_id: Some(task_id),
        context_id: Some(context_id),
        metadata: params.message.metadata.clone(),
    };

    let ctx = AgentContext {
        tenant_id: auth.tenant_id,
        task_id,
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: caller_identity(auth),
        push_notification_url: None,
        trace_ctx: None,
        context_id: Some(context_id),
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: scoped_artifact_store(state, auth),
        artifact_public_base: state
            .artifact_store
            .as_ref()
            .map(|_| state.artifact_public_base.clone()),
        resource_id: None,
        thread_id: None,
    };

    let id = env.id.clone();
    let stream = match agent.send_stream(ctx, inbound).await {
        Ok(s) => s,
        Err(e) => return internal_err(env.id, e),
    };

    let mut embed_base = EmbedContext::with_limits(
        auth.tenant_id,
        Some(context_id),
        Some(task_id),
        Some(state.a2a_task_repo.clone()),
        chrono::Utc::now(),
        HashMap::new(),
        &state.embed_limits,
    );
    embed_base.artifact_store = state.artifact_store.clone();
    embed_base.artifact_public_base = state
        .artifact_store
        .as_ref()
        .map(|_| state.artifact_public_base.clone());
    let embed_ctx = Arc::new(embed_base);
    let stream = LateEmbedResolver::new(
        state.embed_registry.clone(),
        embed_ctx,
        state.embed_limits.clone(),
    )
    .wrap(stream);

    let producer = state.eventing.producer.clone();
    let namespace = state.config.kafka.namespace.clone();
    let buffer = state.sse_buffer.clone();
    let task_id_str = task_id.to_string();
    let topic = ork_a2a::topics::agent_status(&namespace, &task_id_str);
    let mut event_seq: u64 = 0;
    // ADR-0009: post-stream coda needs to know whether the agent emitted an
    // explicit final `TaskStatusUpdateEvent` (rare for local agents, common
    // for remote ones). On EOS without one we publish `Completed` implicitly.
    let last_terminal: Arc<tokio::sync::Mutex<Option<TaskState>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let last_terminal_per_event = last_terminal.clone();
    let mapped = stream.then(move |item| {
        event_seq += 1;
        let producer = producer.clone();
        let buffer = buffer.clone();
        let id = id.clone();
        let topic = topic.clone();
        let task_id_str = task_id_str.clone();
        let last_terminal_for_event = last_terminal_per_event.clone();
        async move {
            // Inspect-then-serialise so the wire shape is unchanged but we can
            // detect terminal `StatusUpdate { final: true }` events for the coda.
            if let Ok(ork_a2a::TaskEvent::StatusUpdate(ref ev)) = item
                && ev.is_final
                && is_terminal_state(ev.status.state)
            {
                *last_terminal_for_event.lock().await = Some(ev.status.state);
            }
            let payload = match item {
                Ok(ev) => {
                    serde_json::to_vec(&JsonRpcResponse::ok(id.clone(), ev)).unwrap_or_default()
                }
                Err(e) => serde_json::to_vec(&JsonRpcResponse::<serde_json::Value>::err(
                    id.clone(),
                    JsonRpcError {
                        code: JsonRpcError::INTERNAL_ERROR,
                        message: e.to_string(),
                        data: None,
                    },
                ))
                .unwrap_or_default(),
            };
            // Tee to Kafka. Best-effort: the SSE response is the source of truth
            // for this caller; Kafka is for the bridge / external observers.
            let _ = producer
                .publish(&topic, Some(task_id_str.as_bytes()), &[], &payload)
                .await;
            buffer
                .append(
                    &task_id_str,
                    crate::sse_buffer::ReplayEvent {
                        id: event_seq,
                        payload: payload.clone(),
                        at: std::time::SystemTime::now(),
                    },
                )
                .await;
            Ok::<_, std::convert::Infallible>(
                Event::default()
                    .id(event_seq.to_string())
                    .data(String::from_utf8_lossy(&payload)),
            )
        }
    });

    let publish_state = state.clone();
    let tenant_id = auth.tenant_id;
    let last_terminal_for_coda = last_terminal.clone();
    let coda = futures::stream::once(async move {
        let final_state = last_terminal_for_coda
            .lock()
            .await
            .unwrap_or(TaskState::Completed);
        // Persist the terminal state if the agent stream did not emit one
        // (the inspect-then-serialise loop above doesn't update_state for
        // intermediate events; the engine relies on this coda to land the
        // terminal row).
        if let Err(e) = publish_state
            .a2a_task_repo
            .update_state(tenant_id, task_id, final_state)
            .await
        {
            tracing::warn!(error = %e, "ADR-0009: failed to persist terminal state after stream");
        }
        publish_state
            .push_service
            .publish_terminal(tenant_id, task_id, final_state)
            .await;
        // Comment-only SSE frame so we don't disturb the wire shape; SSE
        // clients ignore lines that start with `:`.
        Ok::<_, std::convert::Infallible>(Event::default().comment("ork.a2a.v1:terminal"))
    });

    Sse::new(mapped.chain(coda))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// ADR-0008 §SSE bridge: `GET /a2a/agents/{agent_id}/stream/{task_id}`.
///
/// Three-tier replay:
/// 1. **Postgres history** (`a2a_messages`) — only when no `Last-Event-ID` header
///    is present (the client is opening a fresh stream and wants the full log).
/// 2. **Redis 60-s window** ([`SseBuffer::replay`]) — events newer than the
///    `Last-Event-ID` (or all buffered events on a fresh open).
/// 3. **Kafka live tail** (`agent.status.<task_id>`) — appended forever after
///    the cached events drain, keeping the connection open.
///
/// Tier 1 events are tagged with synthetic `1..=N` ids so the SSE `Last-Event-ID`
/// recovery contract works even before the buffer has any entries. A bad task id
/// path segment is rejected with `400 Bad Request` because the route precedes the
/// JSON-RPC envelope and there is no `id` field to bind to.
async fn handle_stream_replay(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path((agent_id, task_id_str)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    use futures::stream;

    // ADR-0021: SSE-resume is part of the agent invocation surface — requires
    // the same `agent:<id>:invoke` scope as `message/stream`.
    require_scope!(auth, agent_invoke_scope(&agent_id));

    let task_uuid = match uuid::Uuid::parse_str(&task_id_str) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad task id").into_response(),
    };
    let task_id = TaskId(task_uuid);

    let last_event_id = headers
        .get("Last-Event-Id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let history: Vec<Event> = if last_event_id.is_none() {
        state
            .a2a_task_repo
            .list_messages(auth.tenant_id, task_id, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                Event::default().id((i + 1).to_string()).data(
                    serde_json::to_string(&serde_json::json!({
                        "role": m.role,
                        "parts": m.parts,
                    }))
                    .unwrap_or_default(),
                )
            })
            .collect()
    } else {
        vec![]
    };

    let cached: Vec<Event> = state
        .sse_buffer
        .replay(&task_id_str, last_event_id)
        .await
        .into_iter()
        .map(|e| {
            Event::default()
                .id(e.id.to_string())
                .data(String::from_utf8_lossy(&e.payload))
        })
        .collect();

    let topic = ork_a2a::topics::agent_status(&state.config.kafka.namespace, &task_id_str);
    let live = match state.eventing.consumer.subscribe(&topic).await {
        Ok(s) => s
            .filter_map(|res| async move {
                res.ok()
                    .map(|m| Event::default().data(String::from_utf8_lossy(&m.payload)))
            })
            .boxed(),
        Err(_) => stream::empty().boxed(),
    };

    let combined = stream::iter(
        history
            .into_iter()
            .chain(cached)
            .map(Ok::<_, std::convert::Infallible>),
    )
    .chain(live.map(Ok::<_, std::convert::Infallible>));

    Sse::new(combined)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// ADR-0008 §`tasks/get`: read-side counterpart of `message/send`. Returns the
/// canonical task JSON (status, history, artifacts, metadata) for the requested
/// id. `history_length` clamps the message log; the underlying repo applies the
/// cap, so the wire response is the same shape regardless.
async fn handle_tasks_get(
    state: &AppState,
    auth: &AuthContext,
    env: JsonRpcRequest<serde_json::Value>,
) -> axum::response::Response {
    let params: ork_a2a::TaskQueryParams = match parse_params(&env) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match state
        .a2a_task_repo
        .get_task(auth.tenant_id, params.id)
        .await
    {
        Ok(Some(_)) => {
            let task =
                build_task_response_with_limit(state, auth, params.id, params.history_length).await;
            Json(JsonRpcResponse::ok(env.id, task)).into_response()
        }
        Ok(None) => Json(JsonRpcResponse::<serde_json::Value>::err(
            env.id,
            JsonRpcError::task_not_found(&params.id),
        ))
        .into_response(),
        Err(e) => internal_err(env.id, e),
    }
}

/// ADR-0008 §`tasks/cancel`: best-effort cancellation. We try the agent's
/// `cancel` hook (which lets local agents propagate cancellation through their
/// `CancellationToken`s); regardless of the hook outcome we update the
/// persisted state so a re-`tasks/get` reflects the cancellation request.
///
/// If the agent reports `Unsupported`, we surface it as `TASK_NOT_CANCELABLE`
/// (`-32002`) per the A2A error code table.
async fn handle_tasks_cancel(
    state: &AppState,
    auth: &AuthContext,
    agent_id: &str,
    env: JsonRpcRequest<serde_json::Value>,
) -> axum::response::Response {
    // ADR-0021 §`Vocabulary` row `agent:<id>:cancel`. The cross-tenant cancel
    // policy from §`Open questions` ("only with `tenant:cross_delegate` already
    // in the chain") rides on top of existing tenant isolation: the repo's
    // `update_state` is `WHERE tenant_id = $1`, so cross-tenant cancel today
    // is structurally unreachable. The full cross-tenant path is a follow-up
    // to ADR-0006 / ADR-0007.
    require_scope!(*auth, agent_cancel_scope(agent_id));
    let params: ork_a2a::TaskIdParams = match parse_params(&env) {
        Ok(p) => p,
        Err(e) => return e,
    };

    if let Some(agent) = state.agent_registry.resolve(&agent_id.to_string()).await {
        let ctx = AgentContext {
            tenant_id: auth.tenant_id,
            task_id: params.id,
            parent_task_id: None,
            cancel: CancellationToken::new(),
            caller: caller_identity(auth),
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
        };
        if let Err(e) = agent.cancel(ctx, &params.id).await
            && !matches!(e, ork_common::error::OrkError::Unsupported(_))
        {
            return internal_err(env.id, e);
        }
    }

    if let Err(e) = state
        .a2a_task_repo
        .update_state(auth.tenant_id, params.id, ork_a2a::TaskState::Canceled)
        .await
    {
        tracing::warn!(error = %e, "ADR-0008: failed to persist canceled state");
    }
    // ADR-0009: cancellation is a terminal transition — fan out to subscribers.
    state
        .push_service
        .publish_terminal(auth.tenant_id, params.id, ork_a2a::TaskState::Canceled)
        .await;

    // Honour tenant isolation: if the row never existed under this tenant the
    // update_state call is a no-op and the read returns None — surface that as
    // TASK_NOT_FOUND so we don't return a synthetic `Working` task.
    match state
        .a2a_task_repo
        .get_task(auth.tenant_id, params.id)
        .await
    {
        Ok(Some(_)) => {
            let task = build_task_response(state, auth, params.id).await;
            Json(JsonRpcResponse::ok(env.id, task)).into_response()
        }
        Ok(None) => Json(JsonRpcResponse::<serde_json::Value>::err(
            env.id,
            JsonRpcError::task_not_found(&params.id),
        ))
        .into_response(),
        Err(e) => internal_err(env.id, e),
    }
}

/// ADR-0008 §`tasks/pushNotificationConfig/set` (also ADR-0009 pulled forward):
/// register or replace the webhook callback for an existing task. We upsert by
/// row id (a fresh UUIDv7 each call) — re-`set` MUST overwrite the row stored
/// under the original id, but new configs land as new rows so the repo can
/// pick "the latest by created_at" on `get`.
///
/// We do not validate that the task exists (the FK cascade in the migration
/// keeps integrity); a `set` against an unknown task simply fails the FK at
/// the DB layer and surfaces as `INTERNAL_ERROR`. We also do not echo the
/// stored `id` back — the wire shape is the inbound `PushNotificationConfig`.
async fn handle_push_set(
    state: &AppState,
    auth: &AuthContext,
    env: JsonRpcRequest<serde_json::Value>,
) -> axum::response::Response {
    let params: ork_a2a::TaskPushNotificationConfigParams = match parse_params(&env) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // ADR-0009 §`tasks/pushNotificationConfig/set`: HTTPS-only outside of
    // dev. Localhost is allowed in dev so contributors can run the loopback
    // self-test against `http://127.0.0.1:8080/api/webhooks/a2a-ack`.
    if let Err(rpc_err) = validate_push_url(&params.push_notification_config.url, &state.config.env)
    {
        return Json(JsonRpcResponse::<serde_json::Value>::err(env.id, rpc_err)).into_response();
    }
    // ADR-0009 §`Per-tenant cap`: prevent a runaway tenant from registering
    // unbounded push subscribers. The cap defaults to 100 (`config.push.max_per_tenant`).
    match state
        .a2a_push_repo
        .count_active_for_tenant(auth.tenant_id)
        .await
    {
        Ok(count) if count >= u64::from(state.config.push.max_per_tenant) => {
            return Json(JsonRpcResponse::<serde_json::Value>::err(
                env.id,
                JsonRpcError {
                    code: JsonRpcError::INVALID_PARAMS,
                    message: format!(
                        "push notification cap reached for tenant ({} configs)",
                        state.config.push.max_per_tenant
                    ),
                    data: None,
                },
            ))
            .into_response();
        }
        Ok(_) => {}
        Err(e) => return internal_err(env.id, e),
    }
    let row = ork_core::ports::a2a_push_repo::A2aPushConfigRow {
        id: uuid::Uuid::now_v7(),
        task_id: params.task_id,
        tenant_id: auth.tenant_id,
        url: params.push_notification_config.url.clone(),
        token: params.push_notification_config.token.clone(),
        authentication: params
            .push_notification_config
            .authentication
            .as_ref()
            .and_then(|a| serde_json::to_value(a).ok()),
        metadata: serde_json::json!({}),
        created_at: chrono::Utc::now(),
    };
    if let Err(e) = state.a2a_push_repo.upsert(&row).await {
        return internal_err(env.id, e);
    }
    Json(JsonRpcResponse::ok(env.id, params.push_notification_config)).into_response()
}

/// Validate a subscriber URL per ADR-0009 §`tasks/pushNotificationConfig/set`.
///
/// Outside of `env=dev` only `https://` is accepted. In `dev`, plain `http://`
/// is allowed but only for localhost so a contributor's `tcpdump` proxy works
/// end-to-end without TLS.
fn validate_push_url(url: &Url, env: &str) -> Result<(), JsonRpcError> {
    let scheme = url.scheme();
    if scheme == "https" {
        return Ok(());
    }
    if scheme == "http" && env == "dev" {
        // `url::Url::host_str()` returns IPv6 hosts wrapped in brackets per RFC
        // 3986 §3.2.2 (e.g. "[::1]"), but the loopback v4 address comes back
        // bare. We accept both forms so contributors can register either.
        if let Some(host) = url.host_str()
            && matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        {
            return Ok(());
        }
        return Err(JsonRpcError {
            code: JsonRpcError::INVALID_PARAMS,
            message: format!(
                "push notification URL must be https:// (got {scheme}://; \
                 http:// is allowed in dev only for localhost)"
            ),
            data: None,
        });
    }
    Err(JsonRpcError {
        code: JsonRpcError::INVALID_PARAMS,
        message: format!("push notification URL must be https:// (got {scheme}://)"),
        data: None,
    })
}

/// ADR-0008 §`tasks/pushNotificationConfig/get`: return the registered webhook
/// callback for a task, or `PUSH_NOTIFICATION_NOT_SUPPORTED` (`-32003`) when
/// none has been set. Tenant isolation is provided by the repo's `WHERE
/// tenant_id = $1` filter (until ADR-0020 RLS lands).
async fn handle_push_get(
    state: &AppState,
    auth: &AuthContext,
    env: JsonRpcRequest<serde_json::Value>,
) -> axum::response::Response {
    let params: ork_a2a::TaskPushNotificationGetParams = match parse_params(&env) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match state
        .a2a_push_repo
        .get(auth.tenant_id, params.task_id)
        .await
    {
        Ok(Some(row)) => {
            let cfg = ork_a2a::PushNotificationConfig {
                url: row.url,
                token: row.token,
                authentication: row
                    .authentication
                    .and_then(|v| serde_json::from_value(v).ok()),
            };
            Json(JsonRpcResponse::ok(env.id, cfg)).into_response()
        }
        Ok(None) => Json(JsonRpcResponse::<serde_json::Value>::err(
            env.id,
            JsonRpcError {
                code: JsonRpcError::PUSH_NOTIFICATION_NOT_SUPPORTED,
                message: "no push config registered for task".into(),
                data: None,
            },
        ))
        .into_response(),
        Err(e) => internal_err(env.id, e),
    }
}

// =====================================================================================
// Shared per-handler helpers
// =====================================================================================

/// Decode the typed `params` for an A2A method, returning a ready-to-send
/// `INVALID_PARAMS` (`-32602`) error response on failure. Handlers use the
/// `Result<P, Response>` pattern: `let params = parse_params(&env)?;`-style.
#[allow(clippy::result_large_err)]
fn parse_params<P: serde::de::DeserializeOwned>(
    env: &JsonRpcRequest<serde_json::Value>,
) -> Result<P, axum::response::Response> {
    let raw = match env.params.as_ref() {
        Some(v) => v,
        None => {
            return Err(Json(JsonRpcResponse::<serde_json::Value>::err(
                env.id.clone(),
                JsonRpcError::invalid_params("missing params"),
            ))
            .into_response());
        }
    };
    serde_json::from_value(raw.clone()).map_err(|e| {
        Json(JsonRpcResponse::<serde_json::Value>::err(
            env.id.clone(),
            JsonRpcError::invalid_params(e.to_string()),
        ))
        .into_response()
    })
}

/// JSON-RPC `INTERNAL_ERROR` (`-32603`) wrapper used when a port returns an error
/// after the dispatcher has accepted the request. The `data` field stays `None`
/// because per ADR-0008 we do not surface internal error structures over the wire.
fn internal_err(
    id: Option<serde_json::Value>,
    e: impl std::fmt::Display,
) -> axum::response::Response {
    Json(JsonRpcResponse::<serde_json::Value>::err(
        id,
        JsonRpcError {
            code: JsonRpcError::INTERNAL_ERROR,
            message: e.to_string(),
            data: None,
        },
    ))
    .into_response()
}

/// Build a [`CallerIdentity`] from the auth middleware extension. `user_id` is
/// best-effort: a non-UUID `sub` claim (e.g. the OIDC email format) becomes
/// `None` rather than failing the request — the JWT's `sub` is still recoverable
/// for audit via the auth middleware logs.
fn caller_identity(auth: &AuthContext) -> CallerIdentity {
    let user_id = uuid::Uuid::parse_str(&auth.user_id)
        .ok()
        .map(ork_common::types::UserId);
    // ADR-0020: forward the enriched JWT shape onto `CallerIdentity` so handlers
    // and downstream delegation see the trust tier / class and the tenant chain
    // the inbound JWT declared.
    CallerIdentity {
        tenant_id: auth.tenant_id,
        user_id,
        scopes: auth.scopes.clone(),
        tenant_chain: auth.tenant_chain.clone(),
        trust_tier: auth.trust_tier,
        trust_class: auth.trust_class,
        agent_id: auth.agent_id.clone(),
    }
}

/// True if `state` is a terminal A2A task state per the spec (cannot transition
/// further). Used by the streaming dispatcher to decide whether to fire the
/// ADR-0009 push-outbox publish during the post-stream coda.
const fn is_terminal_state(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
    )
}

/// Wire form of a [`Role`] (matches the SQL `a2a_messages.role` column comment).
const fn role_to_wire(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Agent => "agent",
    }
}

/// Inverse of [`role_to_wire`]. Unknown roles default to `Agent` so a
/// corrupt/legacy row is still returnable from `tasks/get`.
fn role_from_wire(s: &str) -> Role {
    match s {
        "user" => Role::User,
        _ => Role::Agent,
    }
}

/// Build the `Task` JSON shape returned by `message/send` and `tasks/get`. Pulls
/// the row + message log from the repo and stitches them into A2A 1.0 wire form.
async fn build_task_response(state: &AppState, auth: &AuthContext, task_id: TaskId) -> Task {
    build_task_response_with_limit(state, auth, task_id, None).await
}

/// Like [`build_task_response`] but caps the message history to the most-recent
/// `history_length` entries (passed through to the repo).
async fn build_task_response_with_limit(
    state: &AppState,
    auth: &AuthContext,
    task_id: TaskId,
    history_length: Option<u32>,
) -> Task {
    let row = state
        .a2a_task_repo
        .get_task(auth.tenant_id, task_id)
        .await
        .ok()
        .flatten();
    let messages = state
        .a2a_task_repo
        .list_messages(auth.tenant_id, task_id, history_length)
        .await
        .unwrap_or_default();
    let context_id = row.as_ref().map(|r| r.context_id).unwrap_or_default();
    let history: Vec<A2aMessage> = messages
        .into_iter()
        .map(|m| A2aMessage {
            role: role_from_wire(&m.role),
            parts: serde_json::from_value::<Vec<Part>>(m.parts).unwrap_or_default(),
            message_id: m.id,
            task_id: Some(task_id),
            context_id: Some(context_id),
            metadata: None,
        })
        .collect();
    let metadata = row.as_ref().and_then(|r| r.metadata.as_object()).cloned();
    Task {
        id: task_id,
        context_id,
        status: TaskStatus {
            state: row.as_ref().map(|r| r.state).unwrap_or(TaskState::Working),
            message: None,
        },
        history,
        artifacts: vec![],
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_push_url_accepts_https_in_prod() {
        let url = Url::parse("https://example.com/cb").unwrap();
        assert!(validate_push_url(&url, "prod").is_ok());
    }

    #[test]
    fn validate_push_url_rejects_http_in_prod() {
        let url = Url::parse("http://example.com/cb").unwrap();
        let err = validate_push_url(&url, "prod").unwrap_err();
        assert_eq!(err.code, JsonRpcError::INVALID_PARAMS);
    }

    #[test]
    fn validate_push_url_rejects_http_localhost_in_prod() {
        let url = Url::parse("http://127.0.0.1:8080/cb").unwrap();
        assert!(validate_push_url(&url, "staging").is_err());
    }

    #[test]
    fn validate_push_url_allows_http_localhost_in_dev() {
        for h in ["localhost", "127.0.0.1", "[::1]"] {
            let url = Url::parse(&format!("http://{h}:8080/cb")).unwrap();
            assert!(
                validate_push_url(&url, "dev").is_ok(),
                "dev should accept http://{h}:8080/cb"
            );
        }
    }

    #[test]
    fn validate_push_url_rejects_http_remote_in_dev() {
        let url = Url::parse("http://example.com/cb").unwrap();
        assert!(validate_push_url(&url, "dev").is_err());
    }

    #[test]
    fn validate_push_url_rejects_unknown_scheme() {
        let url = Url::parse("ftp://example.com/cb").unwrap();
        assert!(validate_push_url(&url, "dev").is_err());
        assert!(validate_push_url(&url, "prod").is_err());
    }
}
