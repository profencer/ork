//! Shared wiring context for built-in gateway factories (ADR-0013).

use std::sync::Arc;
use std::time::Duration;

use ork_core::ports::gateway::GatewayDeps;
use ork_core::services::tenant::TenantService;
use ork_core::services::workflow::WorkflowService;
use ork_core::workflow::engine::WorkflowEngine;
use ork_eventing::EventingClient;

/// Dependencies supplied by `ork-api` at boot (extends [`GatewayDeps`] for workflow mode).
/// Use [`GatewayDeps::cancel`](ork_core::ports::gateway::GatewayDeps) for I/O and MCP sessions.
#[derive(Clone)]
pub struct GatewayBootstrapDeps {
    pub core: GatewayDeps,
    pub eventing: EventingClient,
    pub namespace: String,
    pub discovery_interval: Duration,
    pub tenant_service: Arc<TenantService>,
    pub workflow_service: Arc<WorkflowService>,
    pub engine: Arc<WorkflowEngine>,
}
