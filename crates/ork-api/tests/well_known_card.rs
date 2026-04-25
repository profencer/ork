//! ADR-0005 integration test for the two well-known card GET endpoints.
//!
//! Local cards are served straight from the registry; remote cards (TTL-cached) fall
//! back to the same handler with no special-casing. The default endpoint serves
//! `discovery.default_agent_id` and 404s when that is unset.
//!
//! We construct the routes with [`well_known_router`] (the AppState-free flavour) so this
//! test does not need a Postgres-backed `WorkflowRepository` to compile.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ork_a2a::extensions::{EXT_TRANSPORT_HINT, PARAM_KAFKA_REQUEST_TOPIC};
use ork_a2a::{AgentCard, Message};
use ork_api::routes::a2a::{WellKnownState, well_known_router};
use ork_common::error::OrkError;
use ork_core::a2a::card_builder::{CardEnrichmentContext, build_local_card};
use ork_core::a2a::{AgentContext, AgentId};
use ork_core::agent_registry::{AgentRegistry, RemoteAgentEntry, TransportHint};
use ork_core::models::agent::AgentConfig;
use ork_core::ports::agent::{Agent, AgentEventStream};
use serde_json::Value;
use tower::ServiceExt;
use url::Url;

/// Tiny test agent so we can exercise the registry without spinning up an LLM.
struct TestAgent {
    id: AgentId,
    card: AgentCard,
}

#[async_trait]
impl Agent for TestAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    async fn send_stream(
        &self,
        _ctx: AgentContext,
        _msg: Message,
    ) -> Result<AgentEventStream, OrkError> {
        Err(OrkError::Workflow("test agent does not run".into()))
    }
}

fn enriched_ctx() -> CardEnrichmentContext {
    CardEnrichmentContext {
        public_base_url: Some(Url::parse("https://api.example.com/").unwrap()),
        provider_organization: Some("Example".into()),
        devportal_url: Some(Url::parse("https://devportal.example.com/").unwrap()),
        namespace: "ork.a2a.v1".into(),
        include_tenant_required_ext: false,
        tenant_header: "X-Tenant-Id".into(),
    }
}

fn agent(id: &str) -> Arc<dyn Agent> {
    let cfg = AgentConfig {
        id: id.into(),
        name: format!("{id} agent"),
        description: "test".into(),
        system_prompt: "sys".into(),
        tools: vec![],
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 100,
        max_tool_iterations: ork_core::models::agent::default_max_tool_iterations(),
        max_parallel_tool_calls: ork_core::models::agent::default_max_parallel_tool_calls(),
        max_tool_result_bytes: ork_core::models::agent::default_max_tool_result_bytes(),
        expose_reasoning: false,
    };
    Arc::new(TestAgent {
        id: id.into(),
        card: build_local_card(&cfg, &enriched_ctx()),
    })
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn per_agent_endpoint_returns_local_card_with_transport_hint() {
    let registry = Arc::new(AgentRegistry::from_agents([agent("planner")]));
    let app = well_known_router(WellKnownState {
        agent_registry: registry,
        default_agent_id: None,
    });

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/a2a/agents/planner/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let card = body_json(resp).await;
    assert_eq!(card["name"], "planner agent");
    assert_eq!(
        card["url"], "https://api.example.com/a2a/agents/planner",
        "card.url is built from public_base_url + agent id (ADR-0005)"
    );
    let hint = card["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["uri"] == EXT_TRANSPORT_HINT)
        .expect("transport-hint extension present");
    assert_eq!(
        hint["params"][PARAM_KAFKA_REQUEST_TOPIC],
        "ork.a2a.v1.agent.request.planner"
    );
}

#[tokio::test]
async fn per_agent_endpoint_falls_back_to_remote_cache() {
    let registry = Arc::new(AgentRegistry::new());
    // Seed a remote card directly (as the discovery subscriber would).
    let cfg = AgentConfig {
        id: "writer".into(),
        name: "writer agent".into(),
        description: "test".into(),
        system_prompt: "sys".into(),
        tools: vec![],
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 100,
        max_tool_iterations: ork_core::models::agent::default_max_tool_iterations(),
        max_parallel_tool_calls: ork_core::models::agent::default_max_parallel_tool_calls(),
        max_tool_result_bytes: ork_core::models::agent::default_max_tool_result_bytes(),
        expose_reasoning: false,
    };
    let card = build_local_card(&cfg, &enriched_ctx());
    registry
        .upsert_remote(
            "writer".into(),
            RemoteAgentEntry {
                transport_hint: TransportHint::from_card(&card),
                card,
                last_seen: Instant::now(),
                ttl: Duration::from_secs(60),
                agent: None,
            },
        )
        .await;

    let app = well_known_router(WellKnownState {
        agent_registry: registry,
        default_agent_id: None,
    });

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/a2a/agents/writer/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let card = body_json(resp).await;
    assert_eq!(card["name"], "writer agent");
    assert_eq!(card["url"], "https://api.example.com/a2a/agents/writer");
}

#[tokio::test]
async fn per_agent_endpoint_returns_404_when_unknown() {
    let registry = Arc::new(AgentRegistry::new());
    let app = well_known_router(WellKnownState {
        agent_registry: registry,
        default_agent_id: None,
    });

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/a2a/agents/ghost/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn default_endpoint_serves_configured_agent() {
    let registry = Arc::new(AgentRegistry::from_agents([agent("planner")]));
    let app = well_known_router(WellKnownState {
        agent_registry: registry,
        default_agent_id: Some("planner".into()),
    });

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let card = body_json(resp).await;
    assert_eq!(card["name"], "planner agent");
}

#[tokio::test]
async fn default_endpoint_404s_when_default_agent_id_unset() {
    let registry = Arc::new(AgentRegistry::from_agents([agent("planner")]));
    let app = well_known_router(WellKnownState {
        agent_registry: registry,
        default_agent_id: None,
    });

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn default_endpoint_404s_when_default_agent_unregistered() {
    let registry = Arc::new(AgentRegistry::from_agents([agent("planner")]));
    let app = well_known_router(WellKnownState {
        agent_registry: registry,
        default_agent_id: Some("nonexistent".into()),
    });

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
