//! Bearer-token auth middleware (ADR-0008 §`Auth + tenant isolation`).
//!
//! Edge JWT validation lives in Kong (ADR-0004); this middleware re-decodes the
//! token with the same secret so handlers can read a typed [`AuthContext`] from
//! request extensions without re-parsing. The `tenant:admin` scope unlocks
//! `X-Tenant-Id` impersonation so the ops dashboard / break-glass tooling can
//! act on a target tenant without minting per-tenant tokens.
//!
//! ADR-0020 §`Mesh trust — JWT claims and propagation`: when a request
//! carries `X-Ork-Mesh-Token` AND a [`MeshTokenSigner`] is configured, the
//! mesh claims **override** the bearer-derived `AuthContext` — `tenant_id`
//! moves to the mesh `tenant_id`, `tenant_chain` is replaced with the mesh
//! chain, scopes / trust_class come from the mesh body. The bearer is still
//! required (it authenticates the mesh peer); the mesh token attests to
//! the originator's identity at the moment the call was issued.

use std::sync::Arc;

use axum::{
    Json,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use ork_common::auth::{ADMIN_IMPERSONATION_SCOPE, IMPERSONATION_HEADER, JwtClaims, TrustClass};
use ork_common::types::TenantId;
use ork_security::{MeshTokenSigner, mesh_token_header};
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

    // ADR-0020: surface the enriched JWT shape to handlers. `tid_chain` is
    // translated to typed `TenantId`s; entries that fail to parse are dropped
    // (the chain is informational/audit, not an authorisation primitive on
    // its own) and a single warning is logged so the operator sees malformed
    // tokens. ADR-0020 §`Mesh trust — JWT claims and propagation` specifies
    // the canonical default for `tid_chain` is `[tenant_id]`; legacy tokens
    // that omit the field land here as an empty Vec, so we seed it so
    // downstream cross-tenant policy checks always see a non-empty chain
    // (`chain.len() == 1` ⇔ no trust-boundary crossing).
    let mut tenant_chain: Vec<TenantId> = claims
        .tid_chain
        .iter()
        .filter_map(|raw| match Uuid::parse_str(raw) {
            Ok(id) => Some(TenantId(id)),
            Err(_) => {
                tracing::warn!(value = %raw, "skipping unparseable tid_chain entry (ADR-0020)");
                None
            }
        })
        .collect();
    if tenant_chain.is_empty() {
        tenant_chain.push(TenantId(tenant_uuid));
    }

    let mut ctx = AuthContext {
        tenant_id: TenantId(tenant_uuid),
        user_id: claims.sub,
        scopes: claims.scopes,
        tenant_chain,
        trust_tier: claims.trust_tier,
        trust_class: claims.trust_class,
        agent_id: claims.agent_id,
    };

    // ADR-0020 §`Mesh trust`: if the request carries `X-Ork-Mesh-Token` and
    // we have a configured signer, verify it and prefer its claims over the
    // bearer-derived context. The bearer's role here is "this peer is
    // allowed to talk to us at all"; the mesh token attests to the
    // originator's identity at the time the call was issued.
    let signer_opt = req.extensions().get::<Arc<dyn MeshTokenSigner>>().cloned();
    let mesh_token_str = req
        .headers()
        .get(mesh_token_header())
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let (Some(signer), Some(token)) = (signer_opt, mesh_token_str) {
        match signer.verify(&token).await {
            Ok(mesh_claims) => {
                tracing::info!(
                    actor = %mesh_claims.sub,
                    tenant_id = %mesh_claims.tenant_id,
                    tid_chain = ?mesh_claims.tenant_chain,
                    agent_id = ?mesh_claims.agent_id,
                    scopes = ?mesh_claims.scopes,
                    result = "verified",
                    "ADR-0020 mesh-token audit"
                );
                ctx.tenant_id = mesh_claims.tenant_id;
                ctx.tenant_chain = mesh_claims.tenant_chain;
                ctx.scopes = mesh_claims.scopes;
                ctx.trust_tier = mesh_claims.trust_tier;
                ctx.trust_class = TrustClass::Agent;
                ctx.agent_id = mesh_claims.agent_id;
                // `user_id` is intentionally NOT overwritten so the audit
                // trail records both the immediate peer (bearer.sub) and
                // the originating principal (mesh_claims.sub via the
                // verified-claim audit event above).
            }
            Err(err) => {
                tracing::info!(
                    error = %err,
                    result = "rejected",
                    "ADR-0020 mesh-token audit"
                );
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": format!("invalid mesh token: {err}") })),
                )
                    .into_response();
            }
        }
    }

    req.extensions_mut().insert(ctx);
    next.run(req).await
}

// Rate limiting was removed in favour of Kong per ADR-0004
// (`docs/adrs/0004-hybrid-kong-kafka-transport.md` §`Sync plane: Kong responsibilities`).
// If a dev-only loopback limiter is ever needed, add it behind a cargo feature flag — do
// not put it back on the main route stack.
