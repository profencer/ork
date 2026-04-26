//! `[[gateways]]` `type = "webui"` factory (ADR-0013 + ADR-0017).

use std::sync::Arc;

use async_trait::async_trait;
use ork_a2a::AgentExtension;
use ork_common::config::GatewayConfig;
use ork_common::error::OrkError;
use ork_core::ports::gateway::GatewayCard;
use ork_gateways::bootstrap::GatewayBootstrapDeps;
use ork_gateways::noop_gateway::NoopGateway;
use ork_gateways::registry::{GatewayFactory, GatewayInstance};
use url::Url;

use crate::routes::protected_routes;
use crate::state::WebUiState;
use crate::static_assets::public_routes;

/// `GatewayFactory` for `type = "webui"`; registered from `ork-api` via `GatewayRegistry::add_factory`.
pub struct WebUiGatewayFactory;

fn webui_card(gateway_id: &str, config: &serde_json::Value) -> GatewayCard {
    let endpoint = config
        .get("public_base_url")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Url>().ok());
    GatewayCard {
        id: gateway_id.to_string(),
        gateway_type: "webui".to_string(),
        name: config
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(gateway_id)
            .to_string(),
        description: config
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("ork Web UI / chat client")
            .to_string(),
        version: config
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string(),
        endpoint,
        capabilities: vec!["webui".to_string()],
        default_input_modes: vec!["text".to_string()],
        default_output_modes: vec!["text".to_string(), "stream".to_string()],
        extensions: vec![AgentExtension {
            uri: "https://ork.dev/a2a/extensions/gateway-role/webui".to_string(),
            description: None,
            params: None,
        }],
    }
}

#[async_trait]
impl GatewayFactory for WebUiGatewayFactory {
    async fn build_instance(
        &self,
        cfg: &GatewayConfig,
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewayInstance, OrkError> {
        let card = webui_card(&cfg.id, &cfg.config);
        let gw: Arc<dyn ork_core::ports::gateway::Gateway> =
            Arc::new(NoopGateway::new(cfg.id.clone(), card));
        let st = WebUiState::from_bootstrap(deps, cfg);
        Ok(GatewayInstance {
            gateway: gw,
            router: public_routes(),
            protected_router: protected_routes(st),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webui_gateway_card_has_role_extension_for_discovery() {
        let card = webui_card("gw-test", &serde_json::json!({}));
        assert_eq!(card.gateway_type, "webui");
        assert!(
            card.extensions
                .iter()
                .any(|e| { e.uri == "https://ork.dev/a2a/extensions/gateway-role/webui" }),
            "expected gateway-role/webui extension for DevPortal / Kafka discovery"
        );
    }
}
