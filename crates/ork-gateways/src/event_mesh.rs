//! Kafka topic → A2A bridge (ADR-0013).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use ork_a2a::Part;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::AgentMessage;
use ork_core::a2a::{AgentId, CallerIdentity};
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::ports::gateway::Gateway;
use ork_core::ports::gateway::GatewayAuthResolver;
use ork_core::ports::gateway::GatewayCard;
use ork_core::ports::gateway::GatewayClaim;
use ork_core::ports::gateway::GatewayDeps;
use ork_core::ports::gateway::GatewayId;
use ork_eventing::EventingClient;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::warn;
use url::Url;

use crate::auth::StaticGatewayAuthResolver;
use crate::bootstrap::GatewayBootstrapDeps;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PayloadMode {
    JsonData,
    Text,
}

/// Consumes one or more Kafka topics and calls a local agent for each message.
#[derive(Clone)]
pub struct EventMeshGateway {
    id: GatewayId,
    card: GatewayCard,
    eventing: EventingClient,
    topics: Vec<String>,
    target: AgentId,
    tenant: TenantId,
    payload_mode: PayloadMode,
    outbound_topic: Option<String>,
    auth: Arc<dyn GatewayAuthResolver>,
    agents: Arc<ork_core::agent_registry::AgentRegistry>,
    artifact_store: Option<Arc<dyn ArtifactStore>>,
    artifact_public_base: Option<String>,
}

impl EventMeshGateway {
    pub fn new(
        gateway_id: &str,
        config: &serde_json::Value,
        deps: &GatewayBootstrapDeps,
    ) -> Result<Self, OrkError> {
        let topics: Vec<String> = config
            .get("topics")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                OrkError::Validation(format!(
                    "gateways: event_mesh {gateway_id} requires string array `topics`"
                ))
            })?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if topics.is_empty() {
            return Err(OrkError::Validation(
                "gateways: event_mesh topics must be non-empty".into(),
            ));
        }
        let target: AgentId = config
            .get("default_agent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                OrkError::Validation(format!(
                    "gateways: event_mesh {gateway_id} needs default_agent"
                ))
            })?
            .to_string();
        let tenant: TenantId = config
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .map(TenantId)
            .ok_or_else(|| {
                OrkError::Validation(format!("gateways: event_mesh {gateway_id} needs tenant_id"))
            })?;
        let payload_mode = match config
            .get("payload_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("json_data")
        {
            "text" => PayloadMode::Text,
            _ => PayloadMode::JsonData,
        };
        let outbound_topic = config
            .get("outbound_topic")
            .and_then(|v| v.as_str())
            .map(String::from);
        let auth: Arc<dyn GatewayAuthResolver> =
            Arc::new(StaticGatewayAuthResolver::with_single_tenant(tenant));
        let card = mesh_card(gateway_id, config);
        Ok(Self {
            id: gateway_id.into(),
            card,
            eventing: deps.eventing.clone(),
            topics,
            target,
            tenant,
            payload_mode,
            outbound_topic,
            auth,
            agents: deps.core.agent_registry.clone(),
            artifact_store: deps.core.artifact_store.clone(),
            artifact_public_base: deps.core.artifact_public_base.clone(),
        })
    }
}

fn mesh_card(gateway_id: &str, config: &serde_json::Value) -> GatewayCard {
    GatewayCard {
        id: gateway_id.into(),
        gateway_type: "event_mesh".into(),
        name: config
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(gateway_id)
            .to_string(),
        description: config
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("ork Kafka event-mesh gateway")
            .to_string(),
        version: config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string(),
        endpoint: config
            .get("public_base_url")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Url>().ok()),
        capabilities: vec!["ingest".into(), "emit".into()],
        default_input_modes: vec!["application/json".into(), "text/plain".into()],
        default_output_modes: vec!["application/json".into()],
        extensions: vec![],
    }
}

#[async_trait]
impl Gateway for EventMeshGateway {
    fn id(&self) -> &GatewayId {
        &self.id
    }

    fn card(&self) -> &GatewayCard {
        &self.card
    }

    async fn start(&self, deps: GatewayDeps) -> Result<(), OrkError> {
        let parent = deps.cancel.clone();
        for topic in &self.topics {
            let topic = topic.clone();
            let g = self.clone();
            let cancel = parent.child_token();
            tokio::spawn(async move { g.run_topic(topic, cancel).await });
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), OrkError> {
        Ok(())
    }
}

impl EventMeshGateway {
    async fn run_topic(self, topic: String, cancel: CancellationToken) {
        let stream = match self.eventing.consumer.subscribe(&topic).await {
            Ok(s) => s,
            Err(e) => {
                error!(%topic, error = %e, "event_mesh: subscribe failed");
                return;
            }
        };
        let mut stream = std::pin::pin!(stream);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                n = stream.next() => {
                    let Some(Ok(msg)) = n else { break; };
                    if let Err(e) = Self::handle_message(
                        &self.id,
                        &msg.payload,
                        self.payload_mode,
                        &self.agents,
                        &self.auth,
                        &self.target,
                        self.tenant,
                        &self.eventing,
                        self.outbound_topic.as_deref(),
                        self.artifact_store.clone(),
                        self.artifact_public_base.clone(),
                    )
                    .await
                    {
                        warn!(error = %e, "event_mesh: message handle failed, continuing");
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_message(
        gateway_id: &str,
        payload: &[u8],
        mode: PayloadMode,
        agents: &ork_core::agent_registry::AgentRegistry,
        auth: &Arc<dyn GatewayAuthResolver>,
        target: &AgentId,
        tenant: TenantId,
        eventing: &EventingClient,
        outbound: Option<&str>,
        artifact_store: Option<Arc<dyn ArtifactStore>>,
        artifact_public_base: Option<String>,
    ) -> Result<(), OrkError> {
        let v: serde_json::Value = match mode {
            PayloadMode::JsonData => match serde_json::from_slice(payload) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "event_mesh: skip non-json");
                    return Ok(());
                }
            },
            PayloadMode::Text => {
                let s = String::from_utf8_lossy(payload);
                serde_json::Value::String(s.into_owned())
            }
        };
        let claim = GatewayClaim {
            gateway_id: gateway_id.to_string(),
            tenant_id: Some(tenant),
            subject: None,
            scopes: vec![],
            extra: serde_json::Value::Null,
        };
        let caller: CallerIdentity = auth.resolve(claim).await?;
        let message = if v.is_string() {
            AgentMessage::user_text(v.as_str().unwrap_or("").to_string())
        } else {
            AgentMessage::user(vec![
                Part::Text {
                    text: "event_mesh".into(),
                    metadata: None,
                },
                Part::Data {
                    data: v.clone(),
                    metadata: None,
                },
            ])
        };
        let agent = agents
            .resolve(target)
            .await
            .ok_or_else(|| OrkError::NotFound(target.clone()))?;
        let token = CancellationToken::new();
        let task_id = ork_a2a::TaskId::new();
        let ctx = ork_core::a2a::AgentContext {
            tenant_id: caller.tenant_id,
            task_id,
            parent_task_id: None,
            cancel: token,
            caller,
            push_notification_url: None,
            trace_ctx: None,
            context_id: None,
            workflow_input: v.clone(),
            iteration: None,
            delegation_depth: 0,
            delegation_chain: vec![],
            step_llm_overrides: None,
            artifact_store,
            artifact_public_base,
            resource_id: None,
            thread_id: None,
        };
        let out = agent.send(ctx, message).await?;
        if let Some(out_topic) = outbound {
            let key = task_id;
            let bytes = serde_json::to_vec(&out)
                .map_err(|e| OrkError::Internal(format!("event_mesh: serialize: {e}")))?;
            eventing
                .producer
                .publish(out_topic, Some(key.to_string().as_bytes()), &[], &bytes)
                .await
                .map_err(|e| OrkError::Internal(format!("event_mesh: publish: {e}")))?;
        }
        Ok(())
    }
}
