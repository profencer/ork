//! ADR-0021 scope guard for the auto-generated REST surface (ADR-0056).
//!
//! Each handler builds the required scope string with helpers from
//! [`ork_common::auth`] (e.g. [`agent_invoke_scope`](ork_common::auth::agent_invoke_scope))
//! and calls [`require_scope`] before doing any work. A request with an
//! [`AuthContext`] whose `scopes` cover the required scope passes; a
//! request without an `AuthContext` (dev mode, `ServerConfig::auth` is
//! `None`) also passes; otherwise the helper returns
//! [`crate::error::ApiError`] with `kind = "forbidden"`.
//!
//! The dev-mode bypass is intentional: ADR-0056 §`Auth and tenant
//! scoping` reads "in `ork dev` mode, all scopes default to allow unless
//! `ServerConfig::auth` is set." Production deployments wire
//! [`ServerConfig::auth`](ork_app::types::ServerConfig::auth) and the
//! auto-router applies the existing [`crate::middleware::auth_middleware`],
//! which guarantees an `AuthContext` exists by the time scope checks run.

use axum::extract::Request;
use axum::http::request::Parts;
use ork_common::auth::AuthContext;
use ork_security::ScopeChecker;

use crate::error::ApiError;

/// Returns `Ok(())` when the caller is authorised to perform `required_scope`.
///
/// `parts` is the borrowed [`Parts`] of an incoming request — the typical
/// access pattern is `let (parts, body) = req.into_parts();
/// require_scope(&parts, &agent_invoke_scope(&id))?;` from inside a
/// handler.
pub fn require_scope(parts: &Parts, required_scope: &str) -> Result<(), ApiError> {
    let Some(ctx) = parts.extensions.get::<AuthContext>() else {
        // Dev mode: no auth_middleware ran, no AuthContext stamped.
        return Ok(());
    };
    if ScopeChecker::allows(&ctx.scopes, required_scope) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "missing scope {required_scope}"
        )))
    }
}

/// Convenience for handlers that have a `&Request` rather than `Parts`.
pub fn require_scope_on_request(req: &Request, required_scope: &str) -> Result<(), ApiError> {
    let Some(ctx) = req.extensions().get::<AuthContext>() else {
        return Ok(());
    };
    if ScopeChecker::allows(&ctx.scopes, required_scope) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "missing scope {required_scope}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use ork_common::auth::{TrustClass, TrustTier};
    use ork_common::types::TenantId;

    fn parts_with_ctx(ctx: Option<AuthContext>) -> Parts {
        let mut req = Request::builder().body(()).unwrap();
        if let Some(c) = ctx {
            req.extensions_mut().insert(c);
        }
        req.into_parts().0
    }

    fn ctx(scopes: &[&str]) -> AuthContext {
        AuthContext {
            tenant_id: TenantId::new(),
            user_id: "u".into(),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            tenant_chain: vec![],
            trust_tier: TrustTier::default(),
            trust_class: TrustClass::default(),
            agent_id: None,
        }
    }

    #[test]
    fn dev_mode_no_context_allows() {
        let parts = parts_with_ctx(None);
        require_scope(&parts, "agent:weather:invoke").expect("dev mode allows");
    }

    #[test]
    fn missing_scope_is_forbidden() {
        let parts = parts_with_ctx(Some(ctx(&["agent:other:invoke"])));
        let err = require_scope(&parts, "agent:weather:invoke").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Forbidden);
    }

    #[test]
    fn matching_scope_allows() {
        let parts = parts_with_ctx(Some(ctx(&["agent:weather:invoke"])));
        require_scope(&parts, "agent:weather:invoke").expect("matching scope allows");
    }

    #[test]
    fn wildcard_scope_matches() {
        let parts = parts_with_ctx(Some(ctx(&["agent:*:invoke"])));
        require_scope(&parts, "agent:weather:invoke").expect("wildcard allows");
    }
}
