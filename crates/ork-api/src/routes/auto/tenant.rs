//! ADR-0056 §`Auth and tenant scoping`: tenant-header middleware shared
//! by every auto-generated route.
//!
//! Resolution order for the request's effective `TenantId`:
//!
//! 1. `X-Ork-Tenant` header (canonical).
//! 2. `X-Tenant-Id` header (deprecated alias, one-release migration).
//! 3. [`ServerConfig::default_tenant`].
//!
//! After resolving the header tenant, the middleware enforces
//! consistency with [`AuthContext::tenant_id`] (set by
//! [`crate::middleware::auth_middleware`] when `cfg.auth.is_some()`):
//!
//! - if no `AuthContext` is present (dev mode), the header tenant is
//!   accepted as-is;
//! - if the header tenant matches the JWT tenant, no further check;
//! - otherwise the caller MUST hold the
//!   [`ADMIN_IMPERSONATION_SCOPE`](ork_common::auth::ADMIN_IMPERSONATION_SCOPE)
//!   per ADR-0020 §`Tenant CRUD`. Without it, the request is rejected
//!   with `403 forbidden`.
//!
//! Stamps the resolved [`TenantId`] onto request extensions so handlers
//! read it through `Extension<TenantId>` without re-parsing the header.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ork_app::types::ServerConfig;
use ork_common::auth::{ADMIN_IMPERSONATION_SCOPE, AuthContext};
use ork_common::types::TenantId;
use ork_security::ScopeChecker;
use uuid::Uuid;

use crate::error::ApiError;

pub const TENANT_HEADER: &str = "X-Ork-Tenant";
pub const LEGACY_TENANT_HEADER: &str = "X-Tenant-Id";

fn parse_uuid_header(headers: &HeaderMap, name: &str) -> Option<TenantId> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(TenantId)
}

pub async fn tenant_middleware(mut req: Request, next: Next) -> Response {
    let cfg_default_tenant = req
        .extensions()
        .get::<Arc<ServerConfig>>()
        .and_then(|c| c.default_tenant.clone());
    let headers = req.headers();
    let header_tenant = parse_uuid_header(headers, TENANT_HEADER)
        .or_else(|| parse_uuid_header(headers, LEGACY_TENANT_HEADER));

    let auth = req.extensions().get::<AuthContext>().cloned();

    let tenant = match (header_tenant, auth.as_ref()) {
        // Dev mode (no auth) and no header → fall back to default_tenant.
        (None, None) => cfg_default_tenant
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(TenantId),
        // Auth present but no header → bind the request to the JWT tenant.
        (None, Some(a)) => Some(a.tenant_id),
        // Header but no auth (dev mode) → accept the header as-is.
        (Some(t), None) => Some(t),
        // Both present → must match unless the caller holds the admin
        // impersonation scope (ADR-0020 §`Tenant CRUD`).
        (Some(t), Some(a)) => {
            if t == a.tenant_id || ScopeChecker::allows(&a.scopes, ADMIN_IMPERSONATION_SCOPE) {
                Some(t)
            } else {
                return ApiError::forbidden(format!(
                    "X-Ork-Tenant `{t}` does not match the JWT tenant; \
                     cross-tenant access requires the `{ADMIN_IMPERSONATION_SCOPE}` scope"
                ))
                .into_response();
            }
        }
    };

    let Some(tenant) = tenant else {
        return ApiError::missing_tenant_header().into_response();
    };
    req.extensions_mut().insert(tenant);
    next.run(req).await
}
