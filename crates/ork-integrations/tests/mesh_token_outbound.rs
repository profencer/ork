//! ADR-0020 §`Mesh trust — JWT claims and propagation`: end-to-end test
//! that an outbound `A2aRemoteAgent` carries the `X-Ork-Mesh-Token` header
//! and the decoded claims match what the caller's `AgentContext` declared.
//!
//! The cousin test on the inbound (server-side verify) lives in
//! `crates/ork-api/tests/`.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use ork_a2a::extensions::{EXT_MESH_TRUST, PARAM_ACCEPTED_SCOPES};
use ork_a2a::{
    AgentCapabilities, AgentCard, AgentExtension, AgentSkill, JsonRpcResponse, Message, MessageId,
    Part, Role, SendMessageResult, TaskId,
};
use ork_common::auth::{TrustClass, TrustTier};
use ork_common::types::TenantId;
use ork_core::a2a::context::{AgentContext, CallerIdentity};
use ork_core::ports::agent::Agent;
use ork_integrations::a2a_client::{A2aAuth, A2aClientConfig, A2aRemoteAgent};
use ork_security::{HmacMeshTokenSigner, MeshTokenSigner, mesh_token_header};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn signer() -> Arc<dyn MeshTokenSigner> {
    Arc::new(HmacMeshTokenSigner::new(
        SecretString::from("mesh-shared-secret"),
        "ork-mesh-test".into(),
        "ork-api-test".into(),
    ))
}

fn vendor_card_with_mesh(base: &str, accepted: &[&str]) -> AgentCard {
    AgentCard {
        name: "vendor".into(),
        description: "vendor".into(),
        version: "0.1.0".into(),
        url: Some(base.parse().expect("base")),
        provider: None,
        capabilities: AgentCapabilities {
            streaming: false,
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
        extensions: Some(vec![AgentExtension {
            uri: EXT_MESH_TRUST.into(),
            description: None,
            params: Some(
                serde_json::json!({
                    PARAM_ACCEPTED_SCOPES: accepted,
                })
                .as_object()
                .cloned()
                .expect("object"),
            ),
        }]),
    }
}

fn ctx_with_caller_scopes(tenant: TenantId, caller_scopes: &[&str]) -> AgentContext {
    AgentContext {
        tenant_id: tenant,
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: caller_scopes.iter().map(|s| (*s).to_string()).collect(),
            tenant_chain: vec![tenant],
            trust_tier: TrustTier::Internal,
            trust_class: TrustClass::User,
            agent_id: Some("planner".into()),
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

fn ack_response_body() -> serde_json::Value {
    let reply = Message {
        role: Role::Agent,
        parts: vec![Part::text("ack")],
        message_id: MessageId::new(),
        task_id: None,
        context_id: None,
        metadata: None,
    };
    let envelope = JsonRpcResponse::ok(
        Some(serde_json::Value::String("1".into())),
        SendMessageResult::Message(reply),
    );
    serde_json::to_value(envelope).expect("envelope")
}

#[tokio::test]
async fn outbound_request_carries_mesh_token_with_intersected_scopes() {
    let server = MockServer::start().await;
    let signer = signer();

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ack_response_body()))
        .expect(1)
        .mount(&server)
        .await;

    let card = vendor_card_with_mesh(&server.uri(), &["agent:reviewer:invoke"]);
    let cfg = A2aClientConfig {
        request_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let agent = A2aRemoteAgent::new(
        "vendor".into(),
        card,
        server.uri().parse().expect("uri"),
        A2aAuth::None,
        reqwest::Client::new(),
        &cfg,
        None,
        Some(signer.clone()),
    );

    let tenant = TenantId(Uuid::now_v7());
    let ctx = ctx_with_caller_scopes(
        tenant,
        &[
            "agent:reviewer:invoke",  // accepted by the card
            "agent:internal:invoke",  // NOT accepted — intersect drops it
            "tool:agent_call:invoke", // not accepted — intersect drops it
        ],
    );

    let _ = agent
        .send(
            ctx.clone(),
            Message {
                role: Role::User,
                parts: vec![Part::text("ping")],
                message_id: MessageId::new(),
                task_id: None,
                context_id: None,
                metadata: None,
            },
        )
        .await
        .expect("send must succeed");

    // Inspect the captured request — wiremock keeps each request body and
    // header set, so we can decode the mesh token and assert claim shape.
    let received = server.received_requests().await.expect("received");
    assert_eq!(received.len(), 1, "exactly one POST captured");
    let req = &received[0];
    let token = req
        .headers
        .get(mesh_token_header())
        .expect("X-Ork-Mesh-Token header present")
        .to_str()
        .expect("ascii token");

    let claims = signer.verify(token).await.expect("token verifies");
    assert_eq!(claims.tenant_id, tenant);
    assert_eq!(claims.tenant_chain, vec![tenant]);
    assert_eq!(
        claims.scopes,
        vec!["agent:reviewer:invoke".to_string()],
        "mesh scopes must be the caller × card-accepted intersection"
    );
    assert_eq!(claims.trust_class, TrustClass::Agent);
    assert_eq!(claims.agent_id, Some("planner".into()));
    assert_eq!(claims.iss, "ork-mesh-test");
    assert_eq!(claims.aud, "ork-api-test");
}

/// `post_sse` (the streaming entry point) must also stamp `X-Ork-Mesh-Token`.
/// Regression guard: a future refactor that drops the mint call from
/// `post_sse` would silently leave streaming traffic unattested. The
/// stub returns a single `[DONE]` SSE event so the call resolves
/// without us having to drive a real stream.
#[tokio::test]
async fn outbound_message_stream_carries_mesh_token() {
    let server = MockServer::start().await;
    let signer = signer();

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Card declares streaming + accepts a single scope so we also
    // confirm the intersect runs on this code path.
    let mut card = vendor_card_with_mesh(&server.uri(), &["agent:reviewer:invoke"]);
    card.capabilities.streaming = true;
    let cfg = A2aClientConfig {
        request_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let agent = A2aRemoteAgent::new(
        "vendor".into(),
        card,
        server.uri().parse().expect("uri"),
        A2aAuth::None,
        reqwest::Client::new(),
        &cfg,
        None,
        Some(signer.clone()),
    );

    let tenant = TenantId(Uuid::now_v7());
    let ctx = ctx_with_caller_scopes(tenant, &["agent:reviewer:invoke"]);

    // Drain the stream — `[DONE]` resolves immediately so this returns.
    let mut stream = agent
        .send_stream(
            ctx,
            Message {
                role: Role::User,
                parts: vec![Part::text("ping")],
                message_id: MessageId::new(),
                task_id: None,
                context_id: None,
                metadata: None,
            },
        )
        .await
        .expect("send_stream must succeed");
    while let Some(_evt) = futures::StreamExt::next(&mut stream).await {}

    let received = server.received_requests().await.expect("received");
    assert_eq!(received.len(), 1);
    let token = received[0]
        .headers
        .get(mesh_token_header())
        .expect("X-Ork-Mesh-Token must be present on streaming POST")
        .to_str()
        .expect("ascii");
    let claims = signer.verify(token).await.expect("verify");
    assert_eq!(claims.tenant_id, tenant);
    assert_eq!(claims.scopes, vec!["agent:reviewer:invoke".to_string()]);
}

#[tokio::test]
async fn outbound_request_omits_header_when_no_signer_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ack_response_body()))
        .expect(1)
        .mount(&server)
        .await;

    let card = vendor_card_with_mesh(&server.uri(), &[]);
    let cfg = A2aClientConfig::default();
    let agent = A2aRemoteAgent::new(
        "vendor".into(),
        card,
        server.uri().parse().expect("uri"),
        A2aAuth::None,
        reqwest::Client::new(),
        &cfg,
        None,
        None, // no mesh signer
    );

    let tenant = TenantId(Uuid::now_v7());
    let ctx = ctx_with_caller_scopes(tenant, &["agent:reviewer:invoke"]);

    let _ = agent
        .send(
            ctx,
            Message {
                role: Role::User,
                parts: vec![Part::text("ping")],
                message_id: MessageId::new(),
                task_id: None,
                context_id: None,
                metadata: None,
            },
        )
        .await
        .expect("send must succeed");

    let received = server.received_requests().await.expect("received");
    assert_eq!(received.len(), 1);
    assert!(
        received[0].headers.get(mesh_token_header()).is_none(),
        "X-Ork-Mesh-Token must NOT be present when no signer is wired"
    );
}
