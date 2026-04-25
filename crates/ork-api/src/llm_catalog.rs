//! Wires the tenant repository (`ork-persistence`) into the
//! [`ork_llm::router::TenantLlmCatalog`] surface so `LlmRouter` can read
//! per-tenant catalog overrides without `ork-llm` ever importing the DB
//! layer (AGENTS.md §3.4 hexagonal invariant). Lives in `ork-api`
//! because that's where the cross-crate wiring already happens.
//!
//! Naive impl for now: every `lookup` hits the tenant service. The
//! router caches the materialised
//! [`ork_llm::openai_compatible::OpenAiCompatibleProvider`] keyed by
//! `(tenant_id, provider_id)`, so the cost of this lookup is paid once
//! per tenant per provider until the cache is invalidated. A
//! tenant-settings TTL cache is a reasonable follow-up but out of scope
//! for ADR 0012.

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::services::tenant::TenantService;
use ork_llm::router::{TenantLlmCatalog, TenantLlmCatalogEntry};

pub struct ServiceTenantLlmCatalog {
    tenants: Arc<TenantService>,
}

impl ServiceTenantLlmCatalog {
    #[must_use]
    pub fn new(tenants: Arc<TenantService>) -> Self {
        Self { tenants }
    }
}

#[async_trait]
impl TenantLlmCatalog for ServiceTenantLlmCatalog {
    async fn lookup(&self, tenant_id: TenantId) -> Result<Option<TenantLlmCatalogEntry>, OrkError> {
        // Treat a missing tenant as "no override" rather than an error:
        // the router still has the operator catalog to fall back to and
        // we don't want a stale caller to bork the whole chat path.
        let tenant = match self.tenants.get_tenant(tenant_id).await {
            Ok(t) => t,
            Err(OrkError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let s = &tenant.settings;
        if s.llm_providers.is_empty() && s.default_provider.is_none() && s.default_model.is_none() {
            return Ok(None);
        }
        Ok(Some(TenantLlmCatalogEntry {
            providers: s.llm_providers.clone(),
            default_provider: s.default_provider.clone(),
            default_model: s.default_model.clone(),
        }))
    }
}
