//! Gateway ingress port (ADR [`0013`](../../../docs/adrs/0013-generic-gateway-abstraction.md)).
//!
//! `ork-core` holds protocol-neutral traits and DTOs only. HTTP routes, Kafka consumers,
//! and embedding live in `ork-gateways` and `ork-api` adapters per AGENTS.md hex rules.

use std::sync::Arc;

use async_trait::async_trait;
use ork_a2a::{ContextId, Message as AgentMessage, TaskId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};

use crate::a2a::{AgentContext, AgentEvent, AgentId, CallerIdentity};
use crate::agent_registry::AgentRegistry;
use crate::ports::a2a_task_repo::A2aTaskRepository;
use crate::ports::artifact_store::ArtifactStore;
use tokio_util::sync::CancellationToken;

/// Stable identifier for a gateway instance in config and discovery.
pub type GatewayId = String;

/// Published to Kafka `discovery.gatewaycards` (ADR-0004, ADR-0013) for DevPortal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayCard {
    pub id: GatewayId,
    pub gateway_type: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub endpoint: Option<url::Url>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub default_input_modes: Vec<String>,
    #[serde(default)]
    pub default_output_modes: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<ork_a2a::AgentExtension>,
}

/// Inbound wire payload before `translate_inbound` (HTTP body, Kafka record, etc.).
#[derive(Clone, Debug, Default)]
pub struct InboundRaw {
    pub body: Vec<u8>,
    /// Lowercase header names, raw byte values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// Adapter-specific key (e.g. Kafka record key) or routing hints.
    pub key: Option<String>,
    pub metadata: serde_json::Value,
}

/// Per-request context for translation (no transport handles in core).
#[derive(Clone, Debug)]
pub struct GatewayCtx {
    pub gateway_id: GatewayId,
    pub request_id: Option<String>,
    pub trace: Option<String>,
    pub extra: serde_json::Value,
}

/// Result of translating an inbound to an A2A `Message` for a target agent.
#[derive(Clone, Debug)]
pub struct TranslatedRequest {
    pub target_agent: AgentId,
    pub message: AgentMessage,
    pub context_id: Option<ContextId>,
    /// Carried into [`AgentContext::workflow_input`] for tool integrations.
    pub workflow_input: serde_json::Value,
}

impl TranslatedRequest {
    /// Build an [`AgentContext`] and retain the A2A message for `Agent::send` / `send_stream`.
    #[must_use]
    pub fn into_agent_context(
        self,
        tenant_id: TenantId,
        caller: CallerIdentity,
        cancel: CancellationToken,
        artifact_store: Option<Arc<dyn ArtifactStore>>,
        artifact_public_base: Option<String>,
    ) -> (AgentContext, AgentMessage) {
        let task_id = TaskId::new();
        let ctx = AgentContext {
            tenant_id,
            task_id,
            parent_task_id: None,
            cancel,
            caller,
            push_notification_url: None,
            trace_ctx: None,
            context_id: self.context_id,
            workflow_input: self.workflow_input,
            iteration: None,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
            step_llm_overrides: None,
            artifact_store,
            artifact_public_base,
        };
        (ctx, self.message)
    }
}

/// A chunk of wire output after translating an outbound agent event.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutboundChunk {
    Json(serde_json::Value),
    Text(String),
}

/// Auth claim assembled by an adapter (headers, signatures, config).
#[derive(Clone, Debug, Default)]
pub struct GatewayClaim {
    pub gateway_id: GatewayId,
    pub tenant_id: Option<TenantId>,
    pub subject: Option<String>,
    pub scopes: Vec<String>,
    pub extra: serde_json::Value,
}

/// Resolves a gateway claim to an ork [`CallerIdentity`].
#[async_trait]
pub trait GatewayAuthResolver: Send + Sync {
    async fn resolve(&self, claim: GatewayClaim) -> Result<CallerIdentity, OrkError>;
}

/// Shared dependencies for [`Gateway::start`].
#[derive(Clone)]
pub struct GatewayDeps {
    pub agent_registry: Arc<AgentRegistry>,
    pub a2a_repo: Arc<dyn A2aTaskRepository>,
    pub auth_resolver: Arc<dyn GatewayAuthResolver>,
    pub cancel: CancellationToken,
    /// ADR-0016: same wiring as the primary A2A server when `[artifacts] enabled`.
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    /// Public API base for `Part::file` proxy URIs when `presign_get` is unavailable.
    pub artifact_public_base: Option<String>,
}

/// Long-lived ingress component (HTTP server mount, Kafka consumers, etc.).
#[async_trait]
pub trait Gateway: Send + Sync {
    fn id(&self) -> &GatewayId;
    fn card(&self) -> &GatewayCard;

    async fn start(&self, deps: GatewayDeps) -> Result<(), OrkError>;
    async fn shutdown(&self) -> Result<(), OrkError>;
}

/// Thin adapter: translate to/from A2A; hosting is in `ork-gateways`.
#[async_trait]
pub trait GenericGatewayAdapter: Send + Sync {
    fn id(&self) -> &GatewayId;
    fn card(&self) -> &GatewayCard;

    async fn translate_inbound(
        &self,
        raw: InboundRaw,
        ctx: GatewayCtx,
    ) -> Result<TranslatedRequest, OrkError>;

    async fn translate_outbound(
        &self,
        event: AgentEvent,
        ctx: GatewayCtx,
    ) -> Result<Vec<OutboundChunk>, OrkError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_card_roundtrips_json() {
        let card = GatewayCard {
            id: "g1".into(),
            gateway_type: "rest".into(),
            name: "Rest".into(),
            description: "d".into(),
            version: "0.0.1".into(),
            endpoint: None,
            capabilities: vec!["invoke".into()],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            extensions: vec![ork_a2a::AgentExtension {
                uri: "https://ork.dev/a2a/extensions/gateway".into(),
                description: None,
                params: None,
            }],
        };
        let json = serde_json::to_string(&card).expect("ser");
        let back: GatewayCard = serde_json::from_str(&json).expect("de");
        assert_eq!(back.id, card.id);
        assert_eq!(back.extensions[0].uri, card.extensions[0].uri);
    }

    struct TestAdapter {
        id: GatewayId,
    }
    #[async_trait]
    impl GenericGatewayAdapter for TestAdapter {
        fn id(&self) -> &GatewayId {
            &self.id
        }
        fn card(&self) -> &GatewayCard {
            panic!("test")
        }
        async fn translate_inbound(
            &self,
            _raw: InboundRaw,
            _ctx: GatewayCtx,
        ) -> Result<TranslatedRequest, OrkError> {
            Err(OrkError::Unsupported("test".into()))
        }
        async fn translate_outbound(
            &self,
            _event: AgentEvent,
            _ctx: GatewayCtx,
        ) -> Result<Vec<OutboundChunk>, OrkError> {
            Ok(vec![])
        }
    }

    /// Ensures `GenericGatewayAdapter` is object-safe.
    #[test]
    fn generic_gateway_adapter_is_object_safe() {
        let a: Arc<dyn GenericGatewayAdapter> = Arc::new(TestAdapter { id: "t".into() });
        assert_eq!(a.id(), "t");
    }

    #[test]
    fn translated_request_builds_agent_context() {
        let tenant = TenantId::new();
        let caller = CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: vec!["a2a.invoke".into()],
        };
        let cancel = CancellationToken::new();
        let (ctx, msg) = TranslatedRequest {
            target_agent: "planner".into(),
            message: AgentMessage::user_text("hi"),
            context_id: None,
            workflow_input: serde_json::json!({ "k": 1 }),
        }
        .into_agent_context(tenant, caller, cancel, None, None);
        assert_eq!(ctx.tenant_id, tenant);
        assert_eq!(ctx.workflow_input, serde_json::json!({ "k": 1 }));
        assert!(!ctx.caller.scopes.is_empty());
        assert_eq!(ctx.context_id, None);
        let preview = match &msg.parts[0] {
            ork_a2a::Part::Text { text, .. } => text,
            _ => panic!(),
        };
        assert_eq!(preview, "hi");
    }
}
