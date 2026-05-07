//! Config-driven [`GatewayAuthResolver`](ork_core::ports::gateway::GatewayAuthResolver) (ADR-0013).

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, UserId};

use ork_core::a2a::CallerIdentity;
use ork_core::ports::gateway::GatewayAuthResolver;
use ork_core::ports::gateway::GatewayClaim;

/// Resolve tenant and caller from a [`GatewayClaim`] and optional operator defaults.
pub struct StaticGatewayAuthResolver {
    /// Only used as fallback when the claim has no `tenant_id`.
    default_tenant: Option<TenantId>,
    /// Always merged (deduplicated) with `claim.scopes`.
    default_scopes: Vec<String>,
}

impl StaticGatewayAuthResolver {
    /// Single-tenant event-mesh and similar gateways with no header claims.
    #[must_use]
    pub fn with_single_tenant(tenant_id: TenantId) -> Self {
        Self {
            default_tenant: Some(tenant_id),
            default_scopes: vec![],
        }
    }

    pub fn from_config(value: &serde_json::Value) -> Result<Self, OrkError> {
        let default_tenant = value
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .map(|s| {
                s.parse::<uuid::Uuid>().map(TenantId).map_err(|e| {
                    OrkError::Validation(format!("gateways: invalid tenant_id uuid: {e}"))
                })
            })
            .transpose()?;
        let default_scopes: Vec<String> = value
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            default_tenant,
            default_scopes,
        })
    }

    fn merge_scopes(claim: &GatewayClaim) -> Vec<String> {
        claim.scopes.clone()
    }
}

#[async_trait]
impl GatewayAuthResolver for StaticGatewayAuthResolver {
    async fn resolve(&self, claim: GatewayClaim) -> Result<CallerIdentity, OrkError> {
        let tenant_id = match claim.tenant_id {
            Some(t) => t,
            None if self.default_tenant.is_some() => self.default_tenant.expect("just checked"),
            None => {
                return Err(OrkError::Unauthorized(
                    "gateway: no tenant (set config tenant_id or send X-Tenant-Id)".into(),
                ));
            }
        };
        let mut scopes = Self::merge_scopes(&claim);
        for s in &self.default_scopes {
            if !scopes.iter().any(|e| e == s) {
                scopes.push(s.clone());
            }
        }
        let user_id = claim
            .subject
            .as_deref()
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(UserId);
        Ok(CallerIdentity {
            tenant_id,
            user_id,
            scopes,
            ..CallerIdentity::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_core::ports::gateway::GatewayId;

    #[tokio::test]
    async fn configured_tenant_resolves() {
        let tenant = TenantId::new();
        let r = StaticGatewayAuthResolver {
            default_tenant: Some(tenant),
            default_scopes: vec!["a2a.invoke".into()],
        };
        let c = r
            .resolve(GatewayClaim {
                gateway_id: "g1".into(),
                tenant_id: None,
                subject: Some("subj".into()),
                scopes: vec!["extra".into()],
                extra: serde_json::Value::Null,
            })
            .await
            .expect("ok");
        assert_eq!(c.tenant_id, tenant);
        assert!(c.scopes.contains(&"a2a.invoke".to_string()));
        assert!(c.scopes.contains(&"extra".to_string()));
    }

    #[tokio::test]
    async fn missing_tenant_rejects() {
        let r = StaticGatewayAuthResolver {
            default_tenant: None,
            default_scopes: vec![],
        };
        let e = r
            .resolve(GatewayClaim {
                gateway_id: GatewayId::from("g"),
                tenant_id: None,
                subject: None,
                scopes: vec![],
                extra: serde_json::Value::Null,
            })
            .await
            .expect_err("no tenant");
        assert!(matches!(e, OrkError::Unauthorized(_)));
    }
}
