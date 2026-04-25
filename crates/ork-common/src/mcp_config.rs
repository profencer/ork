//! Shared MCP server / transport / auth configuration types (ADR 0010).
//!
//! These types live in `ork-common` instead of `ork-mcp` because they are
//! the **persistence shape** for an MCP server entry and have to be
//! visible to:
//!
//! 1. `ork-core::models::tenant::TenantSettings` (per-tenant JSONB; ADR
//!    0010 §`Server registration` first source).
//! 2. `ork-common::config::AppConfig.mcp.servers` (the global toml
//!    config; ADR 0010 third source).
//! 3. `ork-mcp` (the runtime client, which re-exports these types from
//!    `ork_mcp::config` for ergonomic call sites).
//!
//! Hosting them here breaks the would-be cycle between `ork-core` and
//! `ork-mcp` (the runtime crate already depends on `ork-common`, so a
//! reverse arrow from `ork-core -> ork-mcp` is impossible). The same
//! `*_env` indirection convention as
//! [`A2aAuthToml`](crate::config::A2aAuthToml) (ADR 0007 §"Auth") is used
//! for secrets so storage layouts stay aligned across the two ADRs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use url::Url;

/// One MCP server entry, ready to be persisted (tenant settings) or read
/// from `config/default.toml`. The `id` is the namespace prefix used to
/// qualify the server's tools at runtime (`mcp:<id>.<tool_name>`; see
/// `ork_mcp::descriptor`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpServerConfig {
    /// Stable, human-friendly identifier. Forms the `<server>` half of
    /// the `mcp:<server>.<tool>` namespace (ADR 0010 §`Tool name
    /// convention`).
    pub id: String,

    pub transport: McpTransportConfig,
}

/// One of the three MCP transports listed in ADR 0010 §`New crate
/// crates/ork-mcp`. The variant names match the YAML example from the
/// ADR (`type: stdio | streamable_http | sse`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportConfig {
    /// Spawn an MCP server as a child process and speak the protocol
    /// over its stdio. Arguments are quoted as-is; environment variables
    /// are merged on top of the inherited environment of `ork-api` (so a
    /// tenant can pass e.g. `ATLASSIAN_TOKEN: ${TENANT_A_ATLASSIAN}`
    /// indirection upstream).
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },

    /// Connect to a remote MCP server speaking the
    /// [streamable-HTTP transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http).
    /// This is the modern HTTP variant in MCP 2025-03-26+; new
    /// deployments should pick this over `Sse`.
    StreamableHttp {
        url: Url,
        #[serde(default)]
        auth: McpAuthConfig,
    },

    /// Legacy MCP HTTP+SSE transport. Retained because some servers
    /// still only implement this older binding; new servers should
    /// prefer `StreamableHttp`.
    Sse {
        url: Url,
        #[serde(default)]
        auth: McpAuthConfig,
    },
}

/// Auth selector for an HTTP-flavoured MCP transport. Mirrors the
/// `*_env` convention from [`A2aAuthToml`](crate::config::A2aAuthToml)
/// so secrets handling is identical across A2A (ADR 0007) and MCP
/// (ADR 0010).
///
/// Per the ADR-0010 scope choice, *real* envelope encryption is
/// deferred to ADR 0020; today secrets sit in env vars referenced by
/// `*_env`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpAuthConfig {
    #[default]
    None,

    /// `Authorization: Bearer <env value>` on every request to the MCP
    /// server. Matches the simplest path used by hosted MCP servers
    /// like `mcp.atlassian.com`.
    Bearer { value_env: String },

    /// `<header>: <env value>` — used by MCP servers that gate on a
    /// vendor API key (e.g. `X-API-Key: …`) rather than the OAuth
    /// Bearer scheme.
    ApiKey { header: String, value_env: String },

    /// OAuth2 client-credentials grant. Cached per `(tenant_id,
    /// server_id)` using the same `CcTokenCache` path that ADR 0007
    /// already introduced.
    Oauth2ClientCredentials {
        token_url: Url,
        client_id_env: String,
        client_secret_env: String,
        #[serde(default)]
        scopes: Vec<String>,
    },
}
