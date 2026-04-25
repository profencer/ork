//! Config-driven gateway registry and built-in factories (ADR-0013).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use ork_common::config::GatewayConfig;
use ork_common::error::OrkError;
use ork_core::ports::gateway::Gateway;

use crate::bootstrap::GatewayBootstrapDeps;
use crate::event_mesh::EventMeshGateway;
use crate::mcp_gw;
use crate::rest;
use crate::webhook;

/// One loaded gateway: HTTP routes (may be empty) plus the [`Gateway`] handle for discovery and lifecycle.
pub struct GatewayInstance {
    pub gateway: Arc<dyn Gateway>,
    pub router: Router,
}

/// All gateways from a config slice with merged public HTTP routes (`Router<()>`).
pub struct GatewaysBuild {
    pub router: Router,
    pub instances: Vec<GatewayInstance>,
}

#[async_trait]
pub trait GatewayFactory: Send + Sync {
    async fn build_instance(
        &self,
        cfg: &GatewayConfig,
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewayInstance, OrkError>;
}

struct RestFactory;

#[async_trait]
impl GatewayFactory for RestFactory {
    async fn build_instance(
        &self,
        cfg: &GatewayConfig,
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewayInstance, OrkError> {
        let (router, gateway) = rest::build(&cfg.id, &cfg.config, deps)?;
        Ok(GatewayInstance { gateway, router })
    }
}

struct WebhookFactory;

#[async_trait]
impl GatewayFactory for WebhookFactory {
    async fn build_instance(
        &self,
        cfg: &GatewayConfig,
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewayInstance, OrkError> {
        let (router, gateway) = webhook::build(&cfg.id, &cfg.config, deps)?;
        Ok(GatewayInstance { gateway, router })
    }
}

struct EventMeshFactory;

#[async_trait]
impl GatewayFactory for EventMeshFactory {
    async fn build_instance(
        &self,
        cfg: &GatewayConfig,
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewayInstance, OrkError> {
        let gateway: Arc<dyn Gateway> =
            Arc::new(EventMeshGateway::new(&cfg.id, &cfg.config, deps)?);
        Ok(GatewayInstance {
            gateway,
            router: Router::new(),
        })
    }
}

struct McpFactory;

#[async_trait]
impl GatewayFactory for McpFactory {
    async fn build_instance(
        &self,
        cfg: &GatewayConfig,
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewayInstance, OrkError> {
        let (router, gateway) = mcp_gw::build(&cfg.id, &cfg.config, deps)?;
        Ok(GatewayInstance { gateway, router })
    }
}

/// Maps `gateway_type` strings to built-in factory implementations.
pub struct GatewayRegistry {
    factories: HashMap<String, Arc<dyn GatewayFactory>>,
}

impl GatewayRegistry {
    /// Registers `rest`, `webhook`, `event_mesh`, and `mcp` (lowercase).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut factories: HashMap<String, Arc<dyn GatewayFactory>> = HashMap::new();
        factories.insert("rest".to_string(), Arc::new(RestFactory));
        factories.insert("webhook".to_string(), Arc::new(WebhookFactory));
        factories.insert("event_mesh".to_string(), Arc::new(EventMeshFactory));
        factories.insert("mcp".to_string(), Arc::new(McpFactory));
        Self { factories }
    }

    /// Number of built-in gateway kinds (for tests / diagnostics).
    #[must_use]
    pub fn builtin_kind_count(&self) -> usize {
        self.factories.len()
    }

    /// Skip entries with `enabled = false`. Unknown `gateway_type` returns validation error.
    pub async fn build_from_config(
        &self,
        configs: &[GatewayConfig],
        deps: &GatewayBootstrapDeps,
    ) -> Result<GatewaysBuild, OrkError> {
        let mut router = Router::new();
        let mut instances = Vec::new();
        for cfg in configs {
            if !cfg.enabled {
                continue;
            }
            let t = cfg.gateway_type.to_lowercase();
            let factory = self.factories.get(&t).ok_or_else(|| {
                OrkError::Validation(format!(
                    "unknown gateway type {:?} for id {:?}",
                    cfg.gateway_type, cfg.id
                ))
            })?;
            let inst = factory.build_instance(cfg, deps).await?;
            router = router.merge(inst.router.clone());
            instances.push(inst);
        }
        Ok(GatewaysBuild { router, instances })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_cover_four_types() {
        let r = GatewayRegistry::with_builtins();
        assert_eq!(r.builtin_kind_count(), 4);
    }
}
