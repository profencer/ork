//! Webhook adapter: `/api/gateways/webhook/{gateway_id}` (ADR-0013).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::post;
use ork_a2a::Part;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::AgentMessage;
use ork_core::a2a::{AgentId, CallerIdentity};
use ork_core::models::workflow::WorkflowTrigger;
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::ports::gateway::Gateway;
use ork_core::ports::gateway::GatewayAuthResolver;
use ork_core::ports::gateway::GatewayCard;
use ork_core::ports::gateway::GatewayClaim;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use url::Url;

use crate::auth::StaticGatewayAuthResolver;
use crate::bootstrap::GatewayBootstrapDeps;
use crate::hmac_util::verify_hmac_sha256;
use crate::noop_gateway::NoopGateway;

const HDR_TENANT: &str = "x-tenant-id";
const HDR_SUBJECT: &str = "x-subject";
const DEFAULT_SIG: &str = "x-ork-signature";

/// Same shape as [`ork_api::routes::webhooks`]; triggers workflows by `WorkflowTrigger::Webhook`.
#[derive(Debug, Deserialize)]
pub struct PipelineShape {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub tenant_slug: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Shared implementation for the legacy route and the gateway.
pub async fn run_pipeline_webhook(
    tenant_service: &ork_core::services::tenant::TenantService,
    workflow_service: Arc<ork_core::services::workflow::WorkflowService>,
    engine: Arc<ork_core::workflow::engine::WorkflowEngine>,
    body: &PipelineShape,
) {
    if let Some(slug) = &body.tenant_slug {
        let tenants = match tenant_service.list_tenants().await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, %slug, "pipeline webhook: list_tenants failed");
                return;
            }
        };
        let Some(tenant) = tenants.iter().find(|t| t.slug == *slug) else {
            warn!(%slug, "pipeline webhook: no matching tenant");
            return;
        };
        let definitions = match workflow_service.list_definitions(tenant.id).await {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    error = %e,
                    tenant = %slug,
                    "pipeline webhook: list_definitions failed"
                );
                return;
            }
        };
        for def in definitions {
            if let WorkflowTrigger::Webhook { event } = &def.trigger
                && (event == &body.event || event == "pipeline_completed")
            {
                match workflow_service
                    .start_run(tenant.id, def.id, body.payload.clone())
                    .await
                {
                    Ok(run) => {
                        let engine = engine.clone();
                        let wf = workflow_service.clone();
                        let tid = tenant.id;
                        let run_exec = run.clone();
                        tokio::spawn(async move {
                            if let Err(e) = wf.run_workflow(engine, tid, run_exec).await {
                                error!(
                                    run_id = %run.id,
                                    error = %e,
                                    "workflow execution failed (gateway webhook pipeline)"
                                );
                            }
                        });
                    }
                    Err(e) => error!(
                        workflow = %def.name,
                        error = %e,
                        "webhook workflow start_run failed"
                    ),
                }
            }
        }
    }
}

#[derive(Clone)]
struct WebhookState {
    gateway_id: String,
    /// None = HMAC not configured (unsigned allowed if `allow_unsigned`).
    hmac_key: Option<Vec<u8>>,
    allow_unsigned: bool,
    signature_header: String,
    mode: WebhookMode,
    default_agent: Option<AgentId>,
    auth: Arc<dyn GatewayAuthResolver>,
    agents: Arc<ork_core::agent_registry::AgentRegistry>,
    tenant_service: Arc<ork_core::services::tenant::TenantService>,
    workflow_service: Arc<ork_core::services::workflow::WorkflowService>,
    engine: Arc<ork_core::workflow::engine::WorkflowEngine>,
    artifact_store: Option<Arc<dyn ArtifactStore>>,
    artifact_public_base: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WebhookMode {
    Agent,
    WorkflowTrigger,
}

pub fn build(
    gateway_id: &str,
    config: &serde_json::Value,
    deps: &GatewayBootstrapDeps,
) -> Result<(Router, Arc<dyn Gateway>), OrkError> {
    let hmac_key = if let Some(env_name) = config.get("secret_env").and_then(|v| v.as_str()) {
        let v = std::env::var(env_name).map_err(|_| {
            OrkError::Validation(format!(
                "gateways: webhook {gateway_id} requires env var {env_name} (secret_env)"
            ))
        })?;
        Some(v.into_bytes())
    } else {
        None
    };
    let allow_unsigned = config
        .get("allow_unsigned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let signature_header = config
        .get("signature_header")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_SIG)
        .to_ascii_lowercase();
    let mode = match config
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("agent")
    {
        "workflow_trigger" => WebhookMode::WorkflowTrigger,
        _ => WebhookMode::Agent,
    };
    let default_agent: Option<AgentId> = match mode {
        WebhookMode::Agent => Some(
            config
                .get("default_agent")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    OrkError::Validation(format!(
                        "gateways: webhook {gateway_id} in agent mode needs default_agent"
                    ))
                })?
                .to_string(),
        ),
        WebhookMode::WorkflowTrigger => config
            .get("default_agent")
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    let auth: Arc<dyn GatewayAuthResolver> =
        Arc::new(StaticGatewayAuthResolver::from_config(config)?);
    let state = Arc::new(WebhookState {
        gateway_id: gateway_id.to_string(),
        hmac_key,
        allow_unsigned,
        signature_header,
        mode,
        default_agent,
        auth,
        agents: deps.core.agent_registry.clone(),
        tenant_service: deps.tenant_service.clone(),
        workflow_service: deps.workflow_service.clone(),
        engine: deps.engine.clone(),
        artifact_store: deps.core.artifact_store.clone(),
        artifact_public_base: deps.core.artifact_public_base.clone(),
    });
    let router = Router::new().route(
        &format!("/api/gateways/webhook/{gateway_id}"),
        post(handle).layer(Extension(state)),
    );
    let card = webhook_card(gateway_id, config);
    let gw: Arc<dyn Gateway> = Arc::new(NoopGateway::new(gateway_id.into(), card));
    Ok((router, gw))
}

fn webhook_card(gateway_id: &str, config: &serde_json::Value) -> GatewayCard {
    let endpoint = config
        .get("public_base_url")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Url>().ok())
        .and_then(|u| u.join(&format!("/api/gateways/webhook/{gateway_id}")).ok());
    GatewayCard {
        id: gateway_id.into(),
        gateway_type: "webhook".into(),
        name: config
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(gateway_id)
            .to_string(),
        description: config
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("ork webhook → A2A / workflow")
            .to_string(),
        version: config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string(),
        endpoint,
        capabilities: vec!["ingest".into()],
        default_input_modes: vec!["application/json".into()],
        default_output_modes: vec!["application/json".into()],
        extensions: vec![],
    }
}

fn header_tenant(map: &HeaderMap) -> Option<TenantId> {
    let raw = map.get(HDR_TENANT)?.to_str().ok()?;
    raw.parse().ok().map(TenantId)
}

async fn handle(
    Extension(st): Extension<Arc<WebhookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let sig = get_header_lower(&headers, st.signature_header.as_str());
    match (&st.hmac_key, st.allow_unsigned) {
        (Some(key), _) => {
            let Some(s) = sig else {
                return Err((StatusCode::UNAUTHORIZED, "missing signature".into()));
            };
            if !verify_hmac_sha256(key, &body, s.as_str()) {
                return Err((StatusCode::UNAUTHORIZED, "bad signature".into()));
            }
        }
        (None, false) => {
            return Err((
                StatusCode::UNAUTHORIZED,
                "signatures required (set secret_env or allow_unsigned)".into(),
            ));
        }
        (None, true) => {}
    }
    if st.mode == WebhookMode::WorkflowTrigger {
        let parsed: PipelineShape = serde_json::from_slice(&body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid json: {e}")))?;
        run_pipeline_webhook(
            st.tenant_service.as_ref(),
            st.workflow_service.clone(),
            st.engine.clone(),
            &parsed,
        )
        .await;
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "accepted" })),
        ));
    }
    // Agent mode: forward JSON to default agent
    let target = st.default_agent.clone().ok_or((
        StatusCode::UNPROCESSABLE_ENTITY,
        "default_agent required".into(),
    ))?;
    let claim = GatewayClaim {
        gateway_id: st.gateway_id.clone(),
        tenant_id: header_tenant(&headers),
        subject: headers
            .get(HDR_SUBJECT)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        scopes: vec![],
        extra: serde_json::Value::Null,
    };
    let caller: CallerIdentity = st.auth.resolve(claim).await.map_err(|e: OrkError| {
        (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            e.to_string(),
        )
    })?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let summary = v
        .get("event")
        .or_else(|| v.get("action"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("inbound");
    let text = format!("webhook event {summary}");
    let message = if v.is_object() {
        AgentMessage::user(vec![
            Part::Text {
                text,
                metadata: None,
            },
            Part::Data {
                data: v.clone(),
                metadata: None,
            },
        ])
    } else {
        AgentMessage::user_text(text)
    };
    let agent = st
        .agents
        .resolve(&target)
        .await
        .ok_or((StatusCode::NOT_FOUND, format!("no agent {target}")))?;
    let token = CancellationToken::new();
    let ctx = ork_core::a2a::AgentContext {
        tenant_id: caller.tenant_id,
        task_id: ork_a2a::TaskId::new(),
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
        artifact_store: st.artifact_store.clone(),
        artifact_public_base: st.artifact_public_base.clone(),
        resource_id: None,
        thread_id: None,
    };
    tokio::spawn(async move {
        if let Err(e) = agent.send(ctx, message).await {
            error!(error = %e, "webhook agent mode: agent send failed");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "accepted" })),
    ))
}

fn get_header_lower(headers: &HeaderMap, name_lower: &str) -> Option<String> {
    for (k, v) in headers.iter() {
        if k.as_str().eq_ignore_ascii_case(name_lower) {
            return v.to_str().ok().map(String::from);
        }
    }
    None
}
