//! MCP streamable-HTTP bridge: `/api/gateways/mcp/{gateway_id}` (ADR-0013).

use std::borrow::Cow;
use std::sync::Arc;

use axum::Router;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::AgentMessage;
use ork_core::a2a::{AgentId, CallerIdentity};
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::ports::gateway::Gateway;
use ork_core::ports::gateway::GatewayAuthResolver;
use ork_core::ports::gateway::GatewayCard;
use ork_core::ports::gateway::GatewayClaim;
use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::model::JsonObject;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::StaticGatewayAuthResolver;
use crate::bootstrap::GatewayBootstrapDeps;
use crate::noop_gateway::NoopGateway;

const PREFIX: &str = "agent__";

/// MCP `ServerHandler` that exposes one tool per resolvable local agent: `agent__&lt;id&gt;`.
#[derive(Clone)]
pub struct OrkMcpServer {
    pub registry: Arc<ork_core::agent_registry::AgentRegistry>,
    pub auth: Arc<dyn GatewayAuthResolver>,
    pub tenant: TenantId,
    pub gateway_id: String,
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    pub artifact_public_base: Option<String>,
}

impl OrkMcpServer {
    fn input_schema() -> std::sync::Arc<JsonObject> {
        std::sync::Arc::new(serde_json::from_value(serde_json::json!({
            "type": "object",
            "required": ["message"],
            "properties": {
                "message": { "type": "string", "description": "User text to send to the agent" },
                "context_id": { "type": "string" },
                "metadata": { "type": "object" }
            }
        }))
        .unwrap_or_else(|_| JsonObject::new()))
    }

    async fn do_call(
        &self,
        name: &str,
        args: Option<JsonObject>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = name.strip_prefix(PREFIX).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("tool name must start with {PREFIX}"),
                None::<serde_json::Value>,
            )
        })?;
        let id: AgentId = id.to_string();
        let agent = self.registry.resolve(&id).await.ok_or_else(|| {
            ErrorData::resource_not_found(
                format!("no callable agent {id}"),
                None::<serde_json::Value>,
            )
        })?;

        #[derive(Debug, Deserialize)]
        struct ToolArgs {
            message: String,
            #[serde(default)]
            context_id: Option<String>,
            #[serde(default)]
            metadata: Option<serde_json::Value>,
        }

        let raw = args
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(JsonObject::new()));
        let tool_args: ToolArgs = serde_json::from_value(raw).map_err(|e| {
            ErrorData::invalid_params(
                format!("invalid tool arguments: {e}"),
                None::<serde_json::Value>,
            )
        })?;
        let context_id = if let Some(s) = tool_args.context_id {
            Some(s.parse::<ork_a2a::ContextId>().map_err(|e| {
                ErrorData::invalid_params(format!("context_id: {e}"), None::<serde_json::Value>)
            })?)
        } else {
            None
        };
        let claim = GatewayClaim {
            gateway_id: self.gateway_id.clone(),
            tenant_id: Some(self.tenant),
            subject: None,
            scopes: vec![],
            extra: tool_args
                .metadata
                .clone()
                .unwrap_or(serde_json::Value::Null),
        };
        let caller: CallerIdentity = self.auth.resolve(claim).await.map_err(map_ork_to_mcp)?;
        let message = AgentMessage::user_text(tool_args.message);
        let cancel = CancellationToken::new();
        let ctx = ork_core::a2a::AgentContext {
            tenant_id: caller.tenant_id,
            task_id: ork_a2a::TaskId::new(),
            parent_task_id: None,
            cancel: cancel.clone(),
            caller,
            push_notification_url: None,
            trace_ctx: None,
            context_id,
            workflow_input: tool_args.metadata.unwrap_or(serde_json::Value::Null),
            iteration: None,
            delegation_depth: 0,
            delegation_chain: vec![],
            step_llm_overrides: None,
            artifact_store: self.artifact_store.clone(),
            artifact_public_base: self.artifact_public_base.clone(),
        };
        let out = agent.send(ctx, message).await.map_err(map_ork_to_mcp)?;
        let body = response_text(&out);
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

fn response_text(out: &AgentMessage) -> String {
    let tool = out.to_tool_value();
    if let Some(t) = tool.get("text") {
        if let Some(s) = t.as_str() {
            return s.to_string();
        }
        if let serde_json::Value::String(s) = t {
            return s.clone();
        }
    }
    out.parts
        .iter()
        .filter_map(|p| match p {
            ork_a2a::Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_ork_to_mcp(e: OrkError) -> ErrorData {
    match e {
        OrkError::NotFound(s) => ErrorData::resource_not_found(s, None::<serde_json::Value>),
        OrkError::Unauthorized(s) => ErrorData::invalid_params(s, None::<serde_json::Value>),
        _ => ErrorData::internal_error(e.to_string(), None::<serde_json::Value>),
    }
}

impl ServerHandler for OrkMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "ork MCP gateway: invoke local agents as tools (agent__&lt;id&gt;)".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        let reg = self.registry.clone();
        async move {
            let mut tools = Vec::new();
            let schema = Self::input_schema();
            for id in reg.local_ids() {
                let Some(agent) = reg.resolve(&id).await else {
                    continue;
                };
                let card = agent.card();
                let name: Cow<'static, str> = Cow::Owned(format!("{PREFIX}{id}"));
                tools.push(Tool {
                    name,
                    title: Some(card.name.clone()),
                    description: Some(Cow::Owned(card.description.clone())),
                    input_schema: schema.clone(),
                    output_schema: None,
                    annotations: None,
                    execution: None,
                    icons: None,
                    meta: None,
                });
            }
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        let this = self.clone();
        let name = request.name.to_string();
        let args = request.arguments;
        async move { this.do_call(&name, args).await }
    }
}

fn mcp_card(gateway_id: &str, config: &serde_json::Value) -> GatewayCard {
    let endpoint = config
        .get("public_base_url")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Url>().ok())
        .and_then(|u| u.join(&format!("/api/gateways/mcp/{gateway_id}")).ok());
    GatewayCard {
        id: gateway_id.into(),
        gateway_type: "mcp".into(),
        name: config
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(gateway_id)
            .to_string(),
        description: config
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("ork MCP streamable-HTTP gateway")
            .to_string(),
        version: config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string(),
        endpoint,
        capabilities: vec!["mcp".into(), "tools".into()],
        default_input_modes: vec!["application/json".into()],
        default_output_modes: vec!["application/json".into()],
        extensions: vec![],
    }
}

/// Streamable MCP under `/api/gateways/mcp/{gateway_id}`.
pub fn build(
    gateway_id: &str,
    config: &serde_json::Value,
    deps: &GatewayBootstrapDeps,
) -> Result<(Router, Arc<dyn Gateway>), OrkError> {
    let tenant: TenantId = config
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .map(TenantId)
        .ok_or_else(|| {
            OrkError::Validation(format!(
                "gateways: mcp {gateway_id} requires config string tenant_id"
            ))
        })?;
    let auth: Arc<dyn GatewayAuthResolver> =
        Arc::new(StaticGatewayAuthResolver::from_config(config)?);
    let registry = deps.core.agent_registry.clone();
    let artifact_store = deps.core.artifact_store.clone();
    let artifact_public_base = deps.core.artifact_public_base.clone();
    let gw_id = gateway_id.to_string();
    let cancel = deps.core.cancel.clone();
    let service: StreamableHttpService<OrkMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let registry = registry.clone();
                let auth = auth.clone();
                let gateway_id = gw_id.clone();
                let artifact_store = artifact_store.clone();
                let artifact_public_base = artifact_public_base.clone();
                move || {
                    Ok(OrkMcpServer {
                        registry: registry.clone(),
                        auth: auth.clone(),
                        tenant,
                        gateway_id: gateway_id.clone(),
                        artifact_store: artifact_store.clone(),
                        artifact_public_base: artifact_public_base.clone(),
                    })
                }
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                sse_keep_alive: None,
                cancellation_token: cancel.child_token(),
                ..Default::default()
            },
        );
    let path = format!("/api/gateways/mcp/{gateway_id}");
    let router = Router::new().nest_service(&path, service);
    let card = mcp_card(gateway_id, config);
    let gw: Arc<dyn Gateway> = Arc::new(NoopGateway::new(gateway_id.to_string(), card));
    Ok((router, gw))
}
