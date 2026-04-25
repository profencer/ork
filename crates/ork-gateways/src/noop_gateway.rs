//! Gateways that only contribute HTTP routes (no background tasks).

use async_trait::async_trait;
use ork_common::error::OrkError;

use ork_core::ports::gateway::Gateway;
use ork_core::ports::gateway::GatewayCard;
use ork_core::ports::gateway::GatewayDeps;
use ork_core::ports::gateway::GatewayId;

/// `start`/`shutdown` are no-ops; used for REST, MCP, and webhook (agent mode).
pub struct NoopGateway {
    id: GatewayId,
    card: GatewayCard,
}

impl NoopGateway {
    pub fn new(id: GatewayId, card: GatewayCard) -> Self {
        Self { id, card }
    }
}

#[async_trait]
impl Gateway for NoopGateway {
    fn id(&self) -> &GatewayId {
        &self.id
    }

    fn card(&self) -> &GatewayCard {
        &self.card
    }

    async fn start(&self, _deps: GatewayDeps) -> Result<(), OrkError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), OrkError> {
        Ok(())
    }
}
