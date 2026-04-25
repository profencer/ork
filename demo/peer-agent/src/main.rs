//! Stub vendor A2A peer used by the ork demo (stage 6).
//!
//! Implements just enough of the A2A 1.0 wire protocol (ADR 0003) to make
//! ork's [`A2aRemoteAgent`] client (ADR 0007) think it's talking to a real
//! peer:
//!
//!   - `GET /.well-known/agent-card.json` returns an [`AgentCard`].
//!   - `POST /` accepts a JSON-RPC `message/send` envelope and returns a
//!     fixed [`Task`] envelope whose status is [`TaskState::Completed`] and
//!     whose history contains a single agent message.
//!   - `POST /` with `method = "message/stream"` returns an SSE response
//!     carrying a single `TaskEvent::Message` (the same canned reply) and
//!     a final `TaskEvent::StatusUpdate { final: true }`. The workflow
//!     engine in `ork-core` always uses `send_stream`, so without this the
//!     federation demo would see an empty agent reply.
//!
//! Anything else replies with JSON-RPC `MethodNotFound`. We do NOT
//! implement `tasks/get`, push notifications, or cancellation.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent},
    routing::{get, post},
};
use clap::Parser;
use futures::stream;
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentSkill, ContextId, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse, Message, MessageId, MessageSendParams, Part, Role, SendMessageResult, Task,
    TaskEvent, TaskId, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use serde_json::Value;
use tracing::info;

#[derive(Debug, Parser)]
#[command(about = "Demo A2A peer agent (stub vendor planner)")]
struct Args {
    /// Listen address (e.g. 127.0.0.1:8090).
    #[arg(long, env = "PEER_ADDR", default_value = "127.0.0.1:8090")]
    addr: SocketAddr,

    /// Agent id used in the card and route.
    #[arg(long, env = "PEER_ID", default_value = "vendor-planner")]
    id: String,

    /// Pretty name shown on the card.
    #[arg(long, env = "PEER_NAME", default_value = "Acme Vendor Planner")]
    name: String,
}

#[derive(Clone)]
struct AppState {
    card: AgentCard,
    id: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,demo_peer_agent=info".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let card = build_card(&args);
    let state = AppState {
        card: card.clone(),
        id: args.id.clone(),
    };

    let app = Router::new()
        .route("/.well-known/agent-card.json", get(get_card))
        .route("/", post(rpc_root))
        .with_state(state);

    info!(addr = %args.addr, id = %args.id, "demo peer-agent listening");
    info!(
        "card url    : http://{}/.well-known/agent-card.json",
        args.addr
    );
    info!("rpc endpoint: http://{}/  (POST JSON-RPC)", args.addr);

    let listener = tokio::net::TcpListener::bind(args.addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("peer-agent received Ctrl-C, shutting down");
}

fn build_card(args: &Args) -> AgentCard {
    let base: url::Url = format!("http://{}/", args.addr).parse().expect("base url");
    AgentCard {
        name: args.name.clone(),
        description:
            "Stub remote A2A peer used by the ork demo to exercise ADR 0007 federation."
                .to_string(),
        version: "0.1.0".to_string(),
        url: Some(base),
        provider: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text".into()],
        default_output_modes: vec!["text".into()],
        skills: vec![AgentSkill {
            id: "echo-plan".into(),
            name: "Echo plan".into(),
            description:
                "Returns a fixed acknowledgement so ork's delegate_to step has something to render."
                    .into(),
            tags: vec!["demo".into(), "stub".into()],
            examples: vec!["Plan migration".into()],
            input_modes: None,
            output_modes: None,
        }],
        security_schemes: None,
        security: None,
        extensions: None,
    }
}

async fn get_card(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.card.clone())
}

async fn rpc_root(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest<Value>>,
) -> Response {
    let id = req.id.clone();
    info!(method = %req.method, "incoming JSON-RPC request");

    match req.method.as_str() {
        "message/send" => match parse_send_params(&req) {
            Ok(params) => {
                let (task, _) = build_canned_task(&state, &params);
                let result = SendMessageResult::Task(task);
                json_ok(id, result)
            }
            Err(e) => json_err(id, e),
        },
        "message/stream" => match parse_send_params(&req) {
            Ok(params) => sse_stream_for(&state, id, &params),
            Err(e) => json_err(id, e),
        },
        other => json_err(
            id,
            JsonRpcError {
                code: JsonRpcError::METHOD_NOT_FOUND,
                message: format!("method `{other}` not implemented by demo peer-agent"),
                data: None,
            },
        ),
    }
}

fn parse_send_params(req: &JsonRpcRequest<Value>) -> Result<MessageSendParams, JsonRpcError> {
    let params = req
        .params
        .clone()
        .ok_or_else(|| JsonRpcError::invalid_params("missing params"))?;
    serde_json::from_value::<MessageSendParams>(params)
        .map_err(|e| JsonRpcError::invalid_params(e.to_string()))
}

fn build_canned_task(state: &AppState, params: &MessageSendParams) -> (Task, Message) {
    let context_id = params
        .message
        .context_id
        .clone()
        .unwrap_or_else(ContextId::new);
    let task_id = params.message.task_id.clone().unwrap_or_else(TaskId::new);

    let prompt_preview = first_text(&params.message)
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect::<String>();
    info!(prompt_preview = %prompt_preview, "vendor-planner replying");

    let reply_text = format!(
        "[{}] acknowledged the request. Stub vendor reply.",
        state.id
    );
    let agent_message = Message {
        role: Role::Agent,
        parts: vec![Part::text(reply_text)],
        message_id: MessageId::new(),
        task_id: Some(task_id.clone()),
        context_id: Some(context_id.clone()),
        metadata: None,
    };
    let task = Task {
        id: task_id,
        context_id,
        status: TaskStatus {
            state: TaskState::Completed,
            message: Some("done".into()),
        },
        history: vec![params.message.clone(), agent_message.clone()],
        artifacts: vec![],
        metadata: None,
    };
    (task, agent_message)
}

fn json_ok<T: serde::Serialize>(id: Option<Value>, result: T) -> Response {
    let payload = JsonRpcResponse::ok(id, result);
    (
        StatusCode::OK,
        Json(serde_json::to_value(payload).unwrap()),
    )
        .into_response()
}

fn json_err(id: Option<Value>, err: JsonRpcError) -> Response {
    let payload: JsonRpcResponse<Value> = JsonRpcResponse::err(id, err);
    (
        StatusCode::OK,
        Json(serde_json::to_value(payload).unwrap()),
    )
        .into_response()
}

/// Build the SSE stream returned for `message/stream`. Two events: the
/// agent's text message, then a final `Completed` status update with
/// `final: true` so the consumer's stream terminates cleanly.
///
/// The SSE `data:` payloads are bare [`TaskEvent`] JSON (NOT wrapped in a
/// JSON-RPC envelope) because that's what ork's
/// `ork_integrations::a2a_client::sse::parse_a2a_sse` expects: it decodes
/// each frame directly into `TaskEvent`. The `id` argument is therefore
/// unused — kept in the signature for symmetry with the `message/send`
/// path.
fn sse_stream_for(state: &AppState, _id: Option<Value>, params: &MessageSendParams) -> Response {
    let (task, agent_message) = build_canned_task(state, params);
    let task_id = task.id.clone();

    let evt_message = TaskEvent::Message(agent_message);
    let evt_status = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id,
        status: TaskStatus {
            state: TaskState::Completed,
            message: Some("done".into()),
        },
        is_final: true,
    });

    let events = vec![
        Ok::<SseEvent, std::convert::Infallible>(
            SseEvent::default().data(serde_json::to_string(&evt_message).unwrap()),
        ),
        Ok(SseEvent::default().data(serde_json::to_string(&evt_status).unwrap())),
    ];

    Sse::new(stream::iter(events)).into_response()
}

fn first_text(msg: &Message) -> Option<String> {
    msg.parts.iter().find_map(|p| match p {
        Part::Text { text, .. } => Some(text.clone()),
        _ => None,
    })
}
