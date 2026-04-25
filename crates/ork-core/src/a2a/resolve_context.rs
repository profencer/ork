//! Task-local resolution context (ADR 0012 §`Selection — separate provider + model fields`).
//!
//! [`crate::ports::llm::LlmProvider`] is a clean trait — no `tenant_id` argument
//! is threaded through `chat()` / `chat_stream()`. Tenant overrides for the LLM
//! provider catalog have to come from somewhere, and bolting an extra parameter
//! onto every call site (workflow engine, agent loop, tool catalog) would be
//! invasive enough to call its own ADR.
//!
//! Instead we put the resolution-time context into a tokio task-local. Callers
//! that want tenant-specific provider/model selection wrap their LLM call with
//! [`ResolveContext::scope`]; consumers in `ork-llm::router` read it via
//! [`ResolveContext::current`]. When the task-local is unset (CLI commands,
//! unit tests that don't care about tenancy) the router falls back to the
//! operator default catalog — which matches the pre-0012 behaviour.
//!
//! Domain concern: lives in `ork-core` so `ork-llm` can depend on it without
//! pulling either crate in the wrong direction (AGENTS.md §3.4 hexagonal
//! invariant — `ork-llm` already depends on `ork-core`).

use ork_common::types::TenantId;

/// Resolution context surfaced through a tokio task-local. See module docs.
#[derive(Clone, Copy, Debug)]
pub struct ResolveContext {
    pub tenant_id: TenantId,
}

tokio::task_local! {
    static RESOLVE_CTX: ResolveContext;
}

impl ResolveContext {
    /// Run `fut` with this context bound to the current task. The router
    /// reads it via [`Self::current`]; nested scopes shadow outer ones,
    /// matching `tokio::task_local`'s semantics.
    pub async fn scope<F, T>(self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        RESOLVE_CTX.scope(self, fut).await
    }

    /// Snapshot the current task-local context, or `None` when no caller
    /// has wrapped the future with [`Self::scope`]. Returning `None` is
    /// the explicit signal for the router to use the operator-default
    /// catalog only — never panic just because a CLI or test forgot to
    /// set the tenant.
    pub fn current() -> Option<Self> {
        RESOLVE_CTX.try_with(|ctx| *ctx).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::types::TenantId;
    use uuid::Uuid;

    #[tokio::test]
    async fn unset_returns_none() {
        assert!(ResolveContext::current().is_none());
    }

    #[tokio::test]
    async fn scope_propagates_tenant_id() {
        let tenant = TenantId(Uuid::nil());
        ResolveContext { tenant_id: tenant }
            .scope(async move {
                let got = ResolveContext::current().expect("scope sets the task-local");
                assert_eq!(got.tenant_id, tenant);
            })
            .await;
        // …and is unset again outside the scope.
        assert!(ResolveContext::current().is_none());
    }

    #[tokio::test]
    async fn nested_scopes_shadow() {
        let outer = TenantId(Uuid::from_u128(1));
        let inner = TenantId(Uuid::from_u128(2));
        ResolveContext { tenant_id: outer }
            .scope(async move {
                assert_eq!(ResolveContext::current().unwrap().tenant_id, outer);
                ResolveContext { tenant_id: inner }
                    .scope(async move {
                        assert_eq!(ResolveContext::current().unwrap().tenant_id, inner);
                    })
                    .await;
                assert_eq!(ResolveContext::current().unwrap().tenant_id, outer);
            })
            .await;
    }
}
