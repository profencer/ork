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

use std::sync::{Arc, OnceLock};

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

/// ADR-0020 §`Edge trust`: Kong terminates TLS and stamps client-cert metadata
/// onto upstream requests. When `env == "production"` but the request
/// arrived without these headers, ork-api is almost certainly being reached
/// directly (Kong bypass). We log this exactly once per process so the
/// signal is visible without burying healthy traffic logs.
const KONG_CERT_SUBJECT_HEADER: &str = "X-Client-Cert-Subject";

static KONG_HEADERS_WARNING: OnceLock<()> = OnceLock::new();

/// Carries the resolved `AppConfig::env` selector into request extensions so
/// `auth_middleware` reads the same value that the rest of the runtime sees,
/// rather than re-reading `ORK__ENV` directly (which misses TOML-only
/// deployments). Inserted by `crate::routes::create_router_with_gateways`.
#[derive(Debug, Clone)]
pub struct RuntimeEnv(pub String);

pub async fn auth_middleware(mut req: Request, next: Next) -> Response {
    // ADR-0020 §`Edge trust`: warn once per process when production traffic
    // bypasses Kong (`X-Client-Cert-Subject` absent). Cheap & local — does
    // not block the request. Prefer the `RuntimeEnv` extension (canonical
    // `state.config.env` resolved at boot from TOML + env vars); fall back
    // to `std::env::var("ORK__ENV")` so unit-test apps that don't carry
    // `AppState` still trigger the warning when explicitly testing it.
    let env_value = req
        .extensions()
        .get::<RuntimeEnv>()
        .map(|e| e.0.clone())
        .unwrap_or_else(|| std::env::var("ORK__ENV").unwrap_or_default());
    if env_value == "production" && !req.headers().contains_key(KONG_CERT_SUBJECT_HEADER) {
        KONG_HEADERS_WARNING.get_or_init(|| {
            tracing::warn!(
                "ADR-0020 §`Edge trust`: env=production but no Kong-style headers ({KONG_CERT_SUBJECT_HEADER}) on request — ork-api may be receiving direct, un-fronted traffic"
            );
        });
    }

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

/// ADR-0021 §`Decision points`: short-circuit a handler with a 403 when
/// the [`AuthContext`] does not carry `$required`. Emits the
/// `audit.scope_denied` tracing event ADR-0021 §`Audit` mandates and
/// returns a JSON `{ "error": "missing scope <required>" }` body so the
/// existing handler error shape stays uniform.
///
/// The macro expects an [`AuthContext`] reference in scope as `$ctx`.
/// Use it from inside a route handler:
///
/// ```ignore
/// pub async fn cancel_task(
///     Extension(ctx): Extension<AuthContext>,
///     Path(agent_id): Path<String>,
///     ...
/// ) -> impl IntoResponse {
///     require_scope!(ctx, &ork_common::auth::agent_cancel_scope(&agent_id));
///     ...
/// }
/// ```
///
/// The audit event format mirrors the `tracing::info!` shape already
/// emitted by the tenant CRUD handlers (ADR-0020 §`Auditing`) so a single
/// log query covers both legacy and ADR-0021 deny lines.
#[macro_export]
macro_rules! require_scope {
    ($ctx:expr, $required:expr) => {{
        let __ctx: &$crate::middleware::AuthContext = &$ctx;
        let __required: &str = &$required;
        if !$crate::middleware::__scope_allows(&__ctx.scopes, __required) {
            tracing::info!(
                actor = %__ctx.user_id,
                tenant_id = %__ctx.tenant_id,
                tid_chain = ?__ctx.tenant_chain,
                scope = %__required,
                result = "forbidden",
                event = ::ork_security::audit::SCOPE_DENIED_EVENT,
                "ADR-0021 audit"
            );
            return (
                ::axum::http::StatusCode::FORBIDDEN,
                ::axum::Json(::serde_json::json!({
                    "error": format!("missing scope {}", __required),
                })),
            )
                .into_response();
        }
    }};
}

/// Implementation hook for [`require_scope!`]. The macro inlines a check
/// against this function so call sites don't need to import
/// [`ork_security::ScopeChecker`] directly. Keep it `#[doc(hidden)]`-flavoured
/// (the leading underscore signals "macro support").
#[must_use]
pub fn __scope_allows(scopes: &[String], required: &str) -> bool {
    ork_security::ScopeChecker::allows(scopes, required)
}
