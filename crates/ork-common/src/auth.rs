//! JWT claims and per-request principal shared by `ork-api` middleware and gateway routes
//! (e.g. ADR-0017 Web UI) so crates do not depend on `ork-api` for [`AuthContext`].

use serde::{Deserialize, Serialize};

use crate::types::TenantId;

/// JWT claims accepted by the API gateway. ADR-0020 §`Mesh trust — JWT claims
/// and propagation` enriched the shape with `tid_chain`, `trust_tier`,
/// `trust_class`, `agent_id`, `iat`, `iss`, `aud`. Every new field is
/// `#[serde(default)]` so dev tokens minted before ADR-0020 still deserialise
/// — ADR §`Negative / costs` notes one minor-version of back-compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub tenant_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub exp: usize,
    /// ADR-0020 §`Tenant id propagation across delegation`: ordered list of
    /// tenant ids whose trust boundaries this token has crossed. Defaults to
    /// `[tenant_id]` when the field is absent (legacy tokens).
    #[serde(default)]
    pub tid_chain: Vec<String>,
    /// ADR-0020 §`Mesh trust`: defaults to [`TrustTier::Internal`] for legacy
    /// tokens.
    #[serde(default)]
    pub trust_tier: TrustTier,
    /// ADR-0020 §`Mesh trust`: defaults to [`TrustClass::User`] for legacy
    /// tokens.
    #[serde(default)]
    pub trust_class: TrustClass,
    /// ADR-0020: present when `trust_class == Agent`; identifies the local
    /// agent on whose behalf ork minted this token during delegation.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Standard `iat` claim (ADR-0020 token shape). Defaulted so older tokens
    /// without it still parse.
    #[serde(default)]
    pub iat: Option<usize>,
    /// Standard `iss` claim (ADR-0020 token shape).
    #[serde(default)]
    pub iss: Option<String>,
    /// Standard `aud` claim (ADR-0020 token shape).
    #[serde(default)]
    pub aud: Option<String>,
}

/// Per-request principal inserted by auth middleware into request extensions.
/// ADR-0020 enriched the shape; new fields are populated from the JWT claims
/// when present and fall back to safe defaults so non-Kong dev calls still work.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub tenant_id: TenantId,
    pub user_id: String,
    pub scopes: Vec<String>,
    /// ADR-0020 §`Tenant id propagation across delegation`: the trust chain
    /// reconstructed from the inbound JWT's `tid_chain`. Empty when the token
    /// did not declare one (legacy tokens / single-hop calls).
    pub tenant_chain: Vec<TenantId>,
    /// ADR-0020 §`Mesh trust`: trust tier the inbound token claims.
    pub trust_tier: TrustTier,
    /// ADR-0020 §`Mesh trust`: principal kind the inbound token represents.
    pub trust_class: TrustClass,
    /// ADR-0020: present when `trust_class == Agent` — the upstream agent id.
    pub agent_id: Option<String>,
}

impl AuthContext {
    /// `true` if `scope` is in this caller's scope set.
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// Scope that authorises `X-Tenant-Id` impersonation (ADR-0008 §`Auth`) and,
/// per ADR-0020, gates tenant CRUD (`POST/GET/DELETE /api/tenants`) plus
/// cross-tenant delegation. Aliased as [`TENANT_ADMIN_SCOPE`] for ADR-0020
/// readability — both names resolve to the same string.
pub const ADMIN_IMPERSONATION_SCOPE: &str = "tenant:admin";

/// ADR-0020 §`Tenant CRUD restricted`. Same string as [`ADMIN_IMPERSONATION_SCOPE`].
pub const TENANT_ADMIN_SCOPE: &str = ADMIN_IMPERSONATION_SCOPE;

/// ADR-0020 §`Tenant CRUD restricted`: the default scope on a tenant-issued
/// token. Authorises `read self`, `update self settings`.
pub const TENANT_SELF_SCOPE: &str = "tenant:self";

/// Header that carries the impersonation target when the caller has `tenant:admin`.
pub const IMPERSONATION_HEADER: &str = "X-Tenant-Id";

/// Build the scope string that authorises invoking `agent_id` (ADR-0020 §2;
/// formal vocabulary in ADR-0021).
#[must_use]
pub fn agent_invoke_scope(agent_id: &str) -> String {
    format!("agent:{agent_id}:invoke")
}

/// Build the scope string that authorises delegating to `agent_id` (ADR-0021).
#[must_use]
pub fn agent_delegate_scope(agent_id: &str) -> String {
    format!("agent:{agent_id}:delegate")
}

/// Build the scope string that authorises cancelling a task running on
/// `agent_id` (ADR-0021 §`Vocabulary`).
#[must_use]
pub fn agent_cancel_scope(agent_id: &str) -> String {
    format!("agent:{agent_id}:cancel")
}

/// Build the scope string that authorises invoking a built-in or
/// integration tool by name (ADR-0021 §`Vocabulary`). For MCP tools
/// reach for [`tool_mcp_invoke_scope`] instead so the MCP server origin
/// is preserved in the scope shape.
#[must_use]
pub fn tool_invoke_scope(tool_id: &str) -> String {
    format!("tool:{tool_id}:invoke")
}

/// Build the scope string that authorises invoking a specific MCP tool
/// (ADR-0021 §`Vocabulary` row `tool:mcp:<server>.<name>:invoke`). MCP
/// tools land in the catalog as `mcp:<server>.<name>`; the matching
/// scope keeps the server prefix so an operator can grant
/// `tool:mcp:atlassian.*:invoke` without granting other servers.
#[must_use]
pub fn tool_mcp_invoke_scope(server: &str, name: &str) -> String {
    format!("tool:mcp:{server}.{name}:invoke")
}

/// Build the scope string that authorises an artifact action (ADR-0021
/// §`Vocabulary`). `scope` is `tenant` or `context-<id>`; `action` is
/// one of `read | write | delete`.
#[must_use]
pub fn artifact_scope(scope: &str, action: &str) -> String {
    format!("artifact:{scope}:{action}")
}

/// Build the scope string that authorises invoking a specific LLM model
/// (ADR-0021 §`Vocabulary`). Only enforced when
/// `[security.enforce_model_scopes] = true`.
#[must_use]
pub fn model_invoke_scope(provider: &str, model: &str) -> String {
    format!("model:{provider}:{model}:invoke")
}

/// Build the scope string that authorises sourcing events from a
/// gateway (ADR-0021 §`Vocabulary`).
#[must_use]
pub fn gateway_invoke_scope(gateway_id: &str) -> String {
    format!("gateway:{gateway_id}:invoke")
}

/// ADR-0021 §`Decision points` (cross-tenant): cross-tenant delegation
/// requires `agent:<target>:delegate` AND this scope. Carrying it on a
/// regular tenant token is a flagged event in the audit stream.
pub const TENANT_CROSS_DELEGATE_SCOPE: &str = "tenant:cross_delegate";

/// ADR-0021 §`Vocabulary`: operator-only root admin sentinel. Reserved;
/// not the same as the `*` wildcard (which `ScopeChecker::validate_format`
/// rejects).
pub const TENANT_ROOT_SCOPE: &str = "tenant:root";

/// ADR-0021 §`Vocabulary`: required for [`crate::auth`]-protected Web UI
/// gateway routes (ADR-0017).
pub const WEBUI_ACCESS_SCOPE: &str = "webui:access";

/// ADR-0021 §`Vocabulary`: required for non-spec admin views such as
/// `GET /a2a/agents/{id}/tasks` and `GET /a2a/tasks/{task_id}`.
pub const OPS_READ_SCOPE: &str = "ops:read";

/// ADR-0021 §`Vocabulary`: schedule CRUD (read).
pub const SCHEDULE_READ_SCOPE: &str = "schedule:read";

/// ADR-0021 §`Vocabulary`: schedule CRUD (write). Wired into the
/// `routes/schedules.rs` HTTP surface when ADR-0019 lands.
pub const SCHEDULE_WRITE_SCOPE: &str = "schedule:write";

/// ADR-0020 §`Mesh trust — JWT claims and propagation`: the trust tier the
/// caller's token carries. Drives cross-tier audit boundaries; finer-grained
/// per-tier policies are deferred to a follow-up ADR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Tokens minted for ork's own components and trusted internal callers.
    #[default]
    Internal,
    /// Tokens minted for known external partner organisations.
    Partner,
    /// Anonymous / public traffic (e.g. unauthenticated webhook endpoints).
    Public,
}

/// ADR-0020 §`Mesh trust — JWT claims and propagation`: identifies what kind
/// of principal owns the token. `Agent` is set by [`crate::auth::TrustClass`]
/// when ork mints a downstream JWT during delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TrustClass {
    /// Token represents a human user.
    #[default]
    User,
    /// Token represents a service account / machine-to-machine caller.
    Service,
    /// Token minted by ork on behalf of one of its agents during delegation.
    Agent,
}
