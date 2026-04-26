//! Integration tests for [`ork_integrations::a2a_client::A2aRemoteAgent`] against
//! a `wiremock` HTTP stub. These complement the unit tests in
//! `crates/ork-integrations/src/a2a_client/{auth,sse,card_fetch,agent}.rs` by
//! exercising the full `Agent` trait wired through `reqwest`, the retry loop,
//! the SSE parser, and the cache adapter.
//!
//! Each test stands up its own `MockServer` so they can run in parallel.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentSkill, JsonRpcError, JsonRpcResponse, Message, MessageId,
    Part, Role, SendMessageResult, Task, TaskEvent, TaskId, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use ork_cache::InMemoryCache;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::context::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_integrations::a2a_client::{
    A2aAuth, A2aClientConfig, A2aRemoteAgent, A2aRemoteAgentBuilder, CardFetcher, RetryPolicy,
};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast_retry_cfg() -> A2aClientConfig {
    A2aClientConfig {
        request_timeout: Duration::from_secs(5),
        stream_idle_timeout: Duration::from_secs(5),
        retry: RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(10),
            factor: 2.0,
            max_delay: Duration::from_millis(50),
        },
        user_agent: "ork-it/0.0.0".to_string(),
        card_refresh_interval: Duration::from_secs(60),
        ..Default::default()
    }
}

fn vendor_card(base: &str) -> AgentCard {
    AgentCard {
        name: "vendor".into(),
        description: "test vendor".into(),
        version: "0.1.0".into(),
        url: Some(base.parse().expect("valid base url")),
        provider: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "default".into(),
            name: "vendor".into(),
            description: "x".into(),
            tags: vec![],
            examples: vec![],
            input_modes: None,
            output_modes: None,
        }],
        security_schemes: None,
        security: None,
        extensions: None,
    }
}

fn agent_with(
    id: &str,
    server: &MockServer,
    auth: A2aAuth,
    cfg: A2aClientConfig,
) -> A2aRemoteAgent {
    let base = server.uri();
    let mut card = vendor_card(&base);
    card.name = id.to_string();
    A2aRemoteAgent::new(
        id.to_string(),
        card,
        base.parse().expect("valid uri"),
        auth,
        reqwest::Client::new(),
        &cfg,
        None,
    )
}

fn ctx() -> AgentContext {
    let tenant = TenantId(Uuid::nil());
    AgentContext {
        tenant_id: tenant,
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: vec![],
        },
        push_notification_url: None,
        trace_ctx: Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into()),
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

fn user_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::text(text)],
        message_id: MessageId::new(),
        task_id: None,
        context_id: None,
        metadata: None,
    }
}

fn agent_text_reply(text: &str) -> Message {
    Message {
        role: Role::Agent,
        parts: vec![Part::text(text)],
        message_id: MessageId::new(),
        task_id: None,
        context_id: None,
        metadata: None,
    }
}

#[tokio::test]
async fn message_send_round_trip_attaches_auth_and_returns_text_reply() {
    let server = MockServer::start().await;

    let envelope = JsonRpcResponse::ok(
        Some(serde_json::Value::String("ignored".into())),
        SendMessageResult::Message(agent_text_reply("hello back")),
    );
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", "Bearer static-tok"))
        .and(header("x-tenant-id", TenantId(Uuid::nil()).to_string()))
        .and(header("user-agent", "ork-it/0.0.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&envelope))
        .expect(1)
        .mount(&server)
        .await;

    let agent = agent_with(
        "vendor",
        &server,
        A2aAuth::StaticBearer(SecretString::from("static-tok")),
        fast_retry_cfg(),
    );

    let reply = agent.send(ctx(), user_msg("hi")).await.expect("send ok");
    assert_eq!(reply.role, Role::Agent);
    let text = reply
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "hello back");
}

#[tokio::test]
async fn message_send_returns_last_history_message_when_result_is_task() {
    let server = MockServer::start().await;
    let task = Task {
        id: TaskId::new(),
        context_id: ork_a2a::ContextId::new(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
        },
        history: vec![user_msg("hi"), agent_text_reply("done")],
        artifacts: vec![],
        metadata: None,
    };
    let envelope = JsonRpcResponse::ok(
        Some(serde_json::Value::String("x".into())),
        SendMessageResult::Task(task),
    );
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&envelope))
        .expect(1)
        .mount(&server)
        .await;

    let agent = agent_with("vendor", &server, A2aAuth::None, fast_retry_cfg());
    let reply = agent.send(ctx(), user_msg("hi")).await.expect("send ok");
    let text: String = reply
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "done");
}

#[tokio::test]
async fn message_stream_sse_yields_status_message_then_completes() {
    let server = MockServer::start().await;

    let task_id = TaskId::new();
    let working = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id,
        status: TaskStatus {
            state: TaskState::Working,
            message: Some("thinking".into()),
        },
        is_final: false,
    });
    let agent_msg = TaskEvent::Message(agent_text_reply("hello stream"));
    let completed = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id,
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
        },
        is_final: true,
    });

    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        serde_json::to_string(&working).unwrap(),
        serde_json::to_string(&agent_msg).unwrap(),
        serde_json::to_string(&completed).unwrap()
    );

    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("accept", "text/event-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let agent = agent_with("vendor", &server, A2aAuth::None, fast_retry_cfg());
    let mut stream = agent
        .send_stream(ctx(), user_msg("hi"))
        .await
        .expect("stream open");

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("frame ok"));
    }
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], TaskEvent::StatusUpdate(_)));
    assert!(matches!(events[1], TaskEvent::Message(_)));
    assert!(matches!(events[2], TaskEvent::StatusUpdate(_)));
}

#[tokio::test]
async fn tasks_cancel_posts_task_cancel_method_and_succeeds_on_2xx() {
    let server = MockServer::start().await;
    let task_id = TaskId::new();
    let task = Task {
        id: task_id,
        context_id: ork_a2a::ContextId::new(),
        status: TaskStatus {
            state: TaskState::Canceled,
            message: None,
        },
        history: vec![],
        artifacts: vec![],
        metadata: None,
    };
    let envelope = JsonRpcResponse::ok(Some(serde_json::Value::String(task_id.to_string())), task);
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&envelope))
        .expect(1)
        .mount(&server)
        .await;

    let agent = agent_with("vendor", &server, A2aAuth::None, fast_retry_cfg());
    agent.cancel(ctx(), &task_id).await.expect("cancel ok");
}

#[tokio::test]
async fn retry_on_5xx_then_success_keeps_one_request_alive() {
    let server = MockServer::start().await;

    let envelope = JsonRpcResponse::ok(
        Some(serde_json::Value::String("ok".into())),
        SendMessageResult::Message(agent_text_reply("eventually")),
    );

    // First call: 503. wiremock matchers consume responses in declaration order.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Second call: 200.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&envelope))
        .expect(1)
        .mount(&server)
        .await;

    let agent = agent_with("vendor", &server, A2aAuth::None, fast_retry_cfg());
    let reply = agent
        .send(ctx(), user_msg("retry me"))
        .await
        .expect("retry success");
    let txt: String = reply
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(txt, "eventually");
}

#[tokio::test]
async fn retry_after_header_caps_to_max_delay_and_is_honoured() {
    let server = MockServer::start().await;

    let envelope = JsonRpcResponse::ok(
        Some(serde_json::Value::String("ok".into())),
        SendMessageResult::Message(agent_text_reply("done")),
    );
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&envelope))
        .expect(1)
        .mount(&server)
        .await;

    // Cap max_delay at 100ms; the agent must wait at most that even though the
    // server asked for 1s. We assert wall-clock < 1s to prove the cap fires.
    let mut cfg = fast_retry_cfg();
    cfg.retry.max_delay = Duration::from_millis(100);
    cfg.retry.initial_delay = Duration::from_millis(10);

    let agent = agent_with("vendor", &server, A2aAuth::None, cfg);
    let started = Instant::now();
    let reply = agent
        .send(ctx(), user_msg("rate-limited"))
        .await
        .expect("retry-after success");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "max_delay cap not honoured: elapsed {elapsed:?}"
    );
    let txt: String = reply
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(txt, "done");
}

#[tokio::test]
async fn jsonrpc_error_in_body_is_surfaced_as_a2a_client() {
    let server = MockServer::start().await;
    let err_envelope = JsonRpcResponse::<serde_json::Value>::err(
        Some(serde_json::Value::String("x".into())),
        JsonRpcError {
            code: JsonRpcError::TASK_NOT_FOUND,
            message: "no such task".into(),
            data: None,
        },
    );
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&err_envelope))
        .expect(1)
        .mount(&server)
        .await;

    let agent = agent_with("vendor", &server, A2aAuth::None, fast_retry_cfg());
    let err = agent
        .send(ctx(), user_msg("boom"))
        .await
        .expect_err("must surface RPC error");
    match err {
        OrkError::A2aClient(code, msg) => {
            assert_eq!(code, JsonRpcError::TASK_NOT_FOUND);
            assert!(msg.contains("no such task"));
        }
        other => panic!("expected A2aClient, got {other:?}"),
    }
}

#[tokio::test]
async fn card_fetch_caches_in_inmemory_cache_and_skips_second_http() {
    let server = MockServer::start().await;
    let mut card = vendor_card(&server.uri());
    card.name = "cached-vendor".into();
    let card_bytes = serde_json::to_vec(&card).unwrap();

    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(card_bytes.clone()))
        .expect(1) // only ONE network call expected — second must hit the cache
        .mount(&server)
        .await;

    let cache = Arc::new(InMemoryCache::new()) as Arc<dyn ork_cache::KeyValueCache>;
    let fetcher = CardFetcher::new(reqwest::Client::new(), cache, Duration::from_secs(60));

    let base: url::Url = server.uri().parse().unwrap();
    let first = fetcher.fetch(&base, &A2aAuth::None, None).await.unwrap();
    assert_eq!(first.name, "cached-vendor");
    let second = fetcher.fetch(&base, &A2aAuth::None, None).await.unwrap();
    assert_eq!(second.name, "cached-vendor");
}

#[tokio::test]
async fn card_fetch_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let cache = Arc::new(InMemoryCache::new()) as Arc<dyn ork_cache::KeyValueCache>;
    let fetcher = CardFetcher::new(reqwest::Client::new(), cache, Duration::from_secs(60));
    let base: url::Url = server.uri().parse().unwrap();
    let err = fetcher
        .fetch(&base, &A2aAuth::None, None)
        .await
        .expect_err("404 must surface");
    assert!(matches!(err, OrkError::NotFound(_)), "got: {err}");
}

#[tokio::test]
async fn builder_build_inline_walks_card_url_then_constructs_agent() {
    let server = MockServer::start().await;
    let mut card = vendor_card(&server.uri());
    card.name = "inline-vendor".into();
    let card_bytes = serde_json::to_vec(&card).unwrap();

    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(card_bytes))
        .expect(1)
        .mount(&server)
        .await;

    let builder = A2aRemoteAgentBuilder::new(
        reqwest::Client::new(),
        Arc::new(InMemoryCache::new()) as Arc<dyn ork_cache::KeyValueCache>,
        A2aAuth::None,
        fast_retry_cfg(),
        None,
    );
    let card_url: url::Url = format!("{}/.well-known/agent-card.json", server.uri())
        .parse()
        .unwrap();
    let agent = ork_core::ports::remote_agent_builder::RemoteAgentBuilder::build_inline(
        &builder, card_url, None,
    )
    .await
    .expect("inline build");
    assert_eq!(agent.id(), &"inline-vendor".to_string());
}
