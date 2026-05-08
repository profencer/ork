//! ADR-0021 §`Audit`: canonical event names that every scope-check site
//! emits via `tracing::info!(event = ..., …)`. Pulled into one place so
//! a SIEM integration (or a `grep` over logs) doesn't have to chase
//! string drift across `ork-security`, `ork-api`, `ork-integrations`,
//! `ork-storage`, and `ork-webui`.

/// Emitted on every denied scope check. Fields the call site should
/// include where available: `scope`, `actor`, `tenant_id`, `tid_chain`,
/// `request_id`.
pub const SCOPE_DENIED_EVENT: &str = "audit.scope_denied";

/// Emitted on every successful grant of a sensitive scope: any
/// `tenant:admin` grant, any cross-tenant `agent:*:delegate`, any
/// `tenant:cross_delegate` use.
pub const SENSITIVE_GRANT_EVENT: &str = "audit.sensitive_grant";
