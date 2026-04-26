//! Bearer-token auth middleware (ADR-0008 §`Auth + tenant isolation`).
//!
//! Edge JWT validation lives in Kong (ADR-0004); this middleware re-decodes the
//! token with the same secret so handlers can read a typed [`AuthContext`] from
//! request extensions without re-parsing. The `tenant:admin` scope unlocks
//! `X-Tenant-Id` impersonation so the ops dashboard / break-glass tooling can
//! act on a target tenant without minting per-tenant tokens.

use axum::{
    Json,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use ork_common::auth::{ADMIN_IMPERSONATION_SCOPE, IMPERSONATION_HEADER, JwtClaims};
use ork_common::types::TenantId;
use uuid::Uuid;

pub use ork_common::auth::AuthContext;

pub async fn auth_middleware(mut req: Request, next: Next) -> Response {
    let jwt_secret =
        std::env::var("ORK__AUTH__JWT_SECRET").unwrap_or_else(|_| "change-me-in-production".into());

    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match auth_header {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "missing authorization header" })),
            )
                .into_response();
        }
    };

    let key = DecodingKey::from_secret(jwt_secret.as_bytes());
    let validation = Validation::default();

    let claims = match decode::<JwtClaims>(token, &key, &validation) {
        Ok(data) => data.claims,
        Err(e) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": format!("invalid token: {e}") })),
            )
                .into_response();
        }
    };

    let mut tenant_uuid = match Uuid::parse_str(&claims.tenant_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid tenant_id in token" })),
            )
                .into_response();
        }
    };

    // ADR-0008: admin callers may target a different tenant via `X-Tenant-Id`
    // (audit trails should still log the JWT's `sub` and the original
    // `claims.tenant_id`; that's left for ADR-0022 observability).
    let is_admin = claims.scopes.iter().any(|s| s == ADMIN_IMPERSONATION_SCOPE);
    if is_admin
        && let Some(impersonated) = req
            .headers()
            .get(IMPERSONATION_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| Uuid::parse_str(s).ok())
    {
        tenant_uuid = impersonated;
    }

    let ctx = AuthContext {
        tenant_id: TenantId(tenant_uuid),
        user_id: claims.sub,
        scopes: claims.scopes,
    };

    req.extensions_mut().insert(ctx);
    next.run(req).await
}

// Rate limiting was removed in favour of Kong per ADR-0004
// (`docs/adrs/0004-hybrid-kong-kafka-transport.md` §`Sync plane: Kong responsibilities`).
// If a dev-only loopback limiter is ever needed, add it behind a cargo feature flag — do
// not put it back on the main route stack.
