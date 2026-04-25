//! Load [[gateways]] from config, mount routes, publish discovery, and run gateway lifecycles (ADR-0013).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::gateway::Gateway;
use ork_core::ports::gateway::GatewayDeps;
use ork_eventing::GatewayDiscoveryPublisher;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::state::AppState;

/// Built gateway instances plus merged public HTTP routes (each gateway may contribute `Router<()>`).
pub struct GatewayBoot {
    pub router: Router,
    pub gateways: Vec<Arc<dyn Gateway>>,
}

/// Parse config, merge gateway routers, spawn Kafka gateway-card publishers, and call [`Gateway::start`].
pub async fn build_and_start_gateways(
    state: &AppState,
    cancel: CancellationToken,
) -> Result<GatewayBoot, OrkError> {
    let core = GatewayDeps {
        agent_registry: state.agent_registry.clone(),
        a2a_repo: state.a2a_task_repo.clone(),
        auth_resolver: Arc::new(
            ork_gateways::auth::StaticGatewayAuthResolver::with_single_tenant(
                TenantId(Uuid::nil()),
            ),
        ),
        cancel: cancel.clone(),
    };
    let bootstrap = ork_gateways::bootstrap::GatewayBootstrapDeps {
        core,
        eventing: state.eventing.clone(),
        namespace: state.config.kafka.namespace.clone(),
        discovery_interval: Duration::from_secs(state.config.discovery.interval_secs.max(1)),
        tenant_service: state.tenant_service.clone(),
        workflow_service: state.workflow_service.clone(),
        engine: state.engine.clone(),
    };
    let registry = ork_gateways::registry::GatewayRegistry::with_builtins();
    let ork_gateways::GatewaysBuild { router, instances } = registry
        .build_from_config(&state.config.gateways, &bootstrap)
        .await?;

    let interval = bootstrap.discovery_interval;
    let namespace = state.config.kafka.namespace.clone();
    let producer = state.eventing.producer.clone();

    let mut gateways: Vec<Arc<dyn Gateway>> = Vec::new();
    for inst in instances {
        let gw = inst.gateway.clone();
        gateways.push(gw.clone());

        gw.start(bootstrap.core.clone()).await?;

        let gateway_id = gw.id().clone();
        let card_refresh = gw.clone();
        let card_provider: ork_eventing::GatewayCardProvider =
            Arc::new(move || card_refresh.card().clone());
        let publisher = GatewayDiscoveryPublisher::new(
            producer.clone(),
            namespace.clone(),
            gateway_id,
            interval,
            card_provider,
        );
        let c = cancel.clone();
        tokio::spawn(async move {
            publisher.run(c).await;
        });
    }

    Ok(GatewayBoot { router, gateways })
}
