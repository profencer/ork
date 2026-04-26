//! JWT claims and per-request principal shared by `ork-api` middleware and gateway routes
//! (e.g. ADR-0017 Web UI) so crates do not depend on `ork-api` for [`AuthContext`].

use serde::{Deserialize, Serialize};

use crate::types::TenantId;

/// JWT claims accepted by the API gateway. `scopes` is defaulted so older tokens
/// without the field deserialize with an empty list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub tenant_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub exp: usize,
}

/// Per-request principal inserted by auth middleware into request extensions.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub tenant_id: TenantId,
    pub user_id: String,
    pub scopes: Vec<String>,
}

impl AuthContext {
    /// `true` if `scope` is in this caller's scope set.
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// Scope that authorises `X-Tenant-Id` impersonation (ADR-0008 §`Auth`).
pub const ADMIN_IMPERSONATION_SCOPE: &str = "tenant:admin";

/// Header that carries the impersonation target when the caller has `tenant:admin`.
pub const IMPERSONATION_HEADER: &str = "X-Tenant-Id";
