//! REST → A2A bridge (`/api/gateways/rest/{gateway_id}`).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use ork_a2a::{ContextId, Part, Role};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use url::Url;

use ork_core::a2a::AgentMessage;
use ork_core::a2a::{AgentId, CallerIdentity};
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::gateway::Gateway;
use ork_core::ports::gateway::GatewayAuthResolver;
use ork_core::ports::gateway::GatewayCard;
use ork_core::ports::gateway::GatewayClaim;

use crate::auth::StaticGatewayAuthResolver;
use crate::bootstrap::GatewayBootstrapDeps;
use crate::noop_gateway::NoopGateway;

const HDR_TENANT: &str = "x-tenant-id";
const HDR_SUBJECT: &str = "x-subject";
const HDR_SCOPES: &str = "x-scopes";

#[derive(Debug, Deserialize)]
struct RestBody {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    parts: Option<Vec<Part>>,
    #[serde(default)]
    context_id: Option<ContextId>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

#[derive(Clone)]
struct RestState {
    gateway_id: String,
    default_agent: AgentId,
    allow_agent_override: bool,
    auth: Arc<dyn GatewayAuthResolver>,
    agents: Arc<AgentRegistry>,
}

/// Build a REST gateway router and matching [`NoopGateway`] for discovery and lifecycle.
pub fn build(
    gateway_id: &str,
    config: &serde_json::Value,
    deps: &GatewayBootstrapDeps,
) -> Result<(Router, Arc<dyn Gateway>), OrkError> {
    let default_agent: AgentId = config
        .get("default_agent")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            OrkError::Validation(format!(
                "gateways: rest {gateway_id} requires config string default_agent"
            ))
        })?
        .to_string();
    let allow_agent_override = config
        .get("allow_agent_override")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let auth: Arc<dyn GatewayAuthResolver> =
        Arc::new(StaticGatewayAuthResolver::from_config(config)?);
    let state = Arc::new(RestState {
        gateway_id: gateway_id.to_string(),
        default_agent,
        allow_agent_override,
        auth,
        agents: deps.core.agent_registry.clone(),
    });
    let router = Router::new().route(
        &format!("/api/gateways/rest/{gateway_id}"),
        post(handle_rest).layer(Extension(state)),
    );
    let card = rest_card(gateway_id, config);
    let gw: Arc<dyn Gateway> = Arc::new(NoopGateway::new(gateway_id.into(), card));
    Ok((router, gw))
}

fn rest_card(gateway_id: &str, config: &serde_json::Value) -> GatewayCard {
    let endpoint = config
        .get("public_base_url")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Url>().ok())
        .and_then(|u| u.join(&format!("/api/gateways/rest/{gateway_id}")).ok());
    GatewayCard {
        id: gateway_id.into(),
        gateway_type: "rest".into(),
        name: config
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(gateway_id)
            .to_string(),
        description: config
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("ork REST → A2A gateway")
            .to_string(),
        version: config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string(),
        endpoint,
        capabilities: vec!["invoke".into(), "a2a".into()],
        default_input_modes: vec!["text/plain".into(), "application/json".into()],
        default_output_modes: vec!["text/plain".into(), "application/json".into()],
        extensions: vec![],
    }
}

fn header_tenant(map: &HeaderMap) -> Option<TenantId> {
    let raw = map.get(HDR_TENANT)?.to_str().ok()?;
    uuid::Uuid::parse_str(raw).ok().map(TenantId)
}

fn header_scopes(map: &HeaderMap) -> Vec<String> {
    let Some(s) = map.get(HDR_SCOPES).and_then(|v| v.to_str().ok()) else {
        return vec![];
    };
    s.split([',', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}

async fn handle_rest(
    Extension(st): Extension<Arc<RestState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let parsed: RestBody = serde_json::from_slice(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid json: {e}")))?;
    let target = if st.allow_agent_override {
        parsed.agent.unwrap_or_else(|| st.default_agent.clone())
    } else {
        st.default_agent.clone()
    };
    let claim = GatewayClaim {
        gateway_id: st.gateway_id.clone(),
        tenant_id: header_tenant(&headers),
        subject: headers
            .get(HDR_SUBJECT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        scopes: header_scopes(&headers),
        extra: parsed.metadata.clone().unwrap_or(serde_json::Value::Null),
    };
    let caller: CallerIdentity = st.auth.resolve(claim).await.map_err(|e: OrkError| {
        (
            StatusCode::from_u16(e.status_code()).unwrap(),
            e.to_string(),
        )
    })?;
    let message = if let Some(parts) = parsed.parts.filter(|p| !p.is_empty()) {
        AgentMessage::new(Role::User, parts)
    } else if let Some(m) = parsed.message {
        AgentMessage::user_text(m)
    } else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "need `message` or non-empty `parts`".into(),
        ));
    };
    let agent = st
        .agents
        .resolve(&target)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no agent {target}")))?;
    let token = CancellationToken::new();
    let ctx = ork_core::a2a::AgentContext {
        tenant_id: caller.tenant_id,
        task_id: ork_a2a::TaskId::new(),
        parent_task_id: None,
        cancel: token,
        caller,
        push_notification_url: None,
        trace_ctx: None,
        context_id: parsed.context_id,
        workflow_input: parsed.metadata.unwrap_or(serde_json::Value::Null),
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
    };
    let out = match agent.send(ctx, message).await {
        Ok(m) => m,
        Err(e) => {
            return Err((
                StatusCode::from_u16(e.status_code()).unwrap(),
                e.to_string(),
            ));
        }
    };
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(&out)
                .unwrap_or_else(|_| serde_json::json!({ "raw": "message not serialised" })),
        ),
    ))
}
