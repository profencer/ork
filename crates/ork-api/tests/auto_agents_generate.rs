//! ADR-0056 acceptance §`Decision`: `POST /api/agents/:id/generate`.
//!
//! Covers:
//! - happy path: an echo agent returns a final message in
//!   `AgentGenerateOutput`.
//! - 404: unknown agent id.
//! - 422: a `request_context` that does not match the
//!   `OrkApp::request_context_schema` returns the validation envelope.

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ork_a2a::{AgentCapabilities, AgentCard, Message as A2aMessage, Part, Role};
use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_common::error::OrkError;
use ork_core::a2a::{AgentContext, AgentEvent, AgentId, AgentMessage};
use ork_core::ports::agent::{Agent, AgentEventStream};
use serde_json::{Value, json};
use tower::util::ServiceExt;
use uuid::Uuid;

struct EchoAgent {
    id: AgentId,
    card: AgentCard,
}

impl EchoAgent {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            card: AgentCard {
                name: id.into(),
                description: format!("{id} agent"),
                version: "0.1.0".into(),
                url: None,
                provider: None,
                capabilities: AgentCapabilities {
                    streaming: false,
                    push_notifications: false,
                    state_transition_history: false,
                },
                default_input_modes: vec![],
                default_output_modes: vec![],
                skills: vec![],
                security_schemes: None,
                security: None,
                extensions: None,
            },
        }
    }
}

#[async_trait]
impl Agent for EchoAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }
    fn card(&self) -> &AgentCard {
        &self.card
    }
    async fn send_stream(
        &self,
        _ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        // Echo back as an agent message.
        let reply = A2aMessage::agent(msg.parts);
        let s = futures::stream::iter(vec![Ok(AgentEvent::Message(reply))]);
        Ok(Box::pin(s))
    }
}

fn fixture_app(schema: Option<Value>) -> OrkApp {
    let mut b = OrkApp::builder().agent(EchoAgent::new("weather"));
    if let Some(s) = schema {
        b = b.request_context_schema(s);
    }
    b.build().expect("fixture app")
}

#[tokio::test]
async fn happy_path_returns_final_message() {
    let app = fixture_app(None);
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let body = json!({
        "message": A2aMessage::user(vec![Part::text("hi")]),
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/weather/generate")
                .header("X-Ork-Tenant", Uuid::new_v4().to_string())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["run_id"].is_string());
    assert_eq!(v["message"]["role"], "agent");
    assert_eq!(v["message"]["parts"][0]["text"], "hi");
}

#[tokio::test]
async fn unknown_agent_returns_404() {
    let app = fixture_app(None);
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let body = json!({ "message": A2aMessage::user(vec![Part::text("hi")]) });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/unknown/generate")
                .header("X-Ork-Tenant", Uuid::new_v4().to_string())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["kind"], "not_found");
}

#[tokio::test]
async fn request_context_schema_violation_returns_422() {
    // Schema requires { city: string }
    let schema = json!({
        "type": "object",
        "required": ["city"],
        "properties": { "city": { "type": "string" } },
        "additionalProperties": false
    });
    let app = fixture_app(Some(schema));
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let body = json!({
        "message": A2aMessage::user(vec![Part::text("hi")]),
        "request_context": { "wrong_field": 123 }
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/weather/generate")
                .header("X-Ork-Tenant", Uuid::new_v4().to_string())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["kind"], "validation");
    assert!(
        v["error"]["details"]["errors"].is_array(),
        "expected error.details.errors array, got {v}"
    );
}

#[tokio::test]
async fn agents_list_returns_summary() {
    let app = fixture_app(None);
    let cfg = ServerConfig::default();
    let router = ork_api::router_for(&app, &cfg);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/agents")
                .header("X-Ork-Tenant", Uuid::new_v4().to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v[0]["id"], "weather");
    assert_eq!(v[0]["card_name"], "weather");
}

// Suppress unused warnings for fixture pieces.
#[allow(dead_code)]
fn _suppress(role: Role, _id: AgentId) {
    let _ = role;
}
