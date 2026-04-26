//! End-to-end test against the upstream A2A reference server (ADR-0007
//! §`A2A reference-server integration test`).
//!
//! Off by default — flip on with `--features reference-server-it`. The test
//! launches the [google/a2a](https://github.com/google/a2a) reference server
//! via [`testcontainers`], waits for the well-known card to come up, then
//! exercises both the synchronous (`message/send`) and the streaming
//! (`message/stream`) hot paths through [`A2aRemoteAgent`].
//!
//! Why the gate? The container needs a working Docker daemon, which is not
//! available in every developer sandbox; CI runs with `--features
//! reference-server-it` while default `cargo test` skips this file. The
//! `wiremock` integration tests (`tests/a2a_client.rs`) cover the protocol
//! surface — this test exists to catch upstream A2A 1.0 spec drifts that
//! `wiremock` cannot model (real SSE framing, real auth challenges, real
//! header handling).
//!
//! Image used: `ghcr.io/a2aproject/a2a-reference:latest`. Override via the
//! `A2A_REFERENCE_IMAGE` env var when pinning to a release tag in CI.

#![cfg(feature = "reference-server-it")]

use std::time::Duration;

use futures::StreamExt;
use ork_a2a::{Message, MessageId, Part, Role, TaskEvent, TaskId};
use ork_cache::InMemoryCache;
use ork_common::types::TenantId;
use ork_core::a2a::context::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_integrations::a2a_client::{
    A2aAuth, A2aClientConfig, A2aRemoteAgent, CardFetcher, RetryPolicy,
};
use std::sync::Arc;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::{GenericImage, runners::AsyncRunner};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const REF_PORT: u16 = 8000;

fn cfg() -> A2aClientConfig {
    A2aClientConfig {
        request_timeout: Duration::from_secs(30),
        stream_idle_timeout: Duration::from_secs(30),
        retry: RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            factor: 2.0,
            max_delay: Duration::from_secs(2),
        },
        user_agent: "ork-reference-it/0.0.0".to_string(),
        card_refresh_interval: Duration::from_secs(60),
        ..Default::default()
    }
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

async fn boot_reference() -> (testcontainers::ContainerAsync<GenericImage>, url::Url) {
    let image_name = std::env::var("A2A_REFERENCE_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/a2aproject/a2a-reference".to_string());
    let (repo, tag) = image_name
        .rsplit_once(':')
        .map(|(r, t)| (r.to_string(), t.to_string()))
        .unwrap_or_else(|| (image_name.clone(), "latest".to_string()));

    let image = GenericImage::new(repo, tag)
        .with_exposed_port(ContainerPort::Tcp(REF_PORT))
        .with_wait_for(WaitFor::message_on_stderr("Application startup complete"));
    let container = image
        .start()
        .await
        .expect("start a2a reference container (Docker required)");
    let host_port = container
        .get_host_port_ipv4(REF_PORT)
        .await
        .expect("mapped port");
    let base = url::Url::parse(&format!("http://127.0.0.1:{host_port}/")).expect("valid base url");
    (container, base)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_server_message_send_round_trip() {
    let (_guard, base) = boot_reference().await;

    let cache = Arc::new(InMemoryCache::new()) as Arc<dyn ork_cache::KeyValueCache>;
    let fetcher = CardFetcher::new(reqwest::Client::new(), cache, Duration::from_secs(60));
    let card = fetcher
        .fetch(&base, &A2aAuth::None, None)
        .await
        .expect("fetch reference server card");

    let agent = A2aRemoteAgent::new(
        card.name.clone(),
        card,
        base,
        A2aAuth::None,
        reqwest::Client::new(),
        &cfg(),
        None,
    );

    let reply = agent
        .send(ctx(), user_msg("hello reference server"))
        .await
        .expect("message/send must succeed against the reference server");
    assert!(matches!(reply.role, Role::Agent));
    assert!(
        !reply.parts.is_empty(),
        "reference server must produce a non-empty reply"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_server_message_stream_round_trip() {
    let (_guard, base) = boot_reference().await;

    let cache = Arc::new(InMemoryCache::new()) as Arc<dyn ork_cache::KeyValueCache>;
    let fetcher = CardFetcher::new(reqwest::Client::new(), cache, Duration::from_secs(60));
    let card = fetcher
        .fetch(&base, &A2aAuth::None, None)
        .await
        .expect("fetch reference server card");
    assert!(
        card.capabilities.streaming,
        "reference server must advertise streaming for this test"
    );

    let agent = A2aRemoteAgent::new(
        card.name.clone(),
        card,
        base,
        A2aAuth::None,
        reqwest::Client::new(),
        &cfg(),
        None,
    );

    let mut stream = agent
        .send_stream(ctx(), user_msg("stream me"))
        .await
        .expect("message/stream must open against the reference server");

    let mut got_message = false;
    let mut got_status = false;
    while let Some(ev) = stream.next().await {
        match ev.expect("frame ok") {
            TaskEvent::Message(_) => got_message = true,
            TaskEvent::StatusUpdate(_) => got_status = true,
            TaskEvent::ArtifactUpdate(_) => {}
        }
    }
    assert!(
        got_message || got_status,
        "reference server must yield at least one Message or StatusUpdate"
    );
}
