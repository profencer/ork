//! Boot helper: build an `Arc<McpClient>` straight from
//! [`ork_common::config::McpAppConfig`] for `ork-api`'s `main.rs`.
//!
//! Kept in its own module so the dependency on `ork-common`'s config
//! types stays a leaf — the rest of `ork-mcp` only needs the typed
//! [`McpServerConfig`](crate::McpServerConfig) (defined in `ork-common`
//! to avoid the historical circular dependency between `ork-mcp` and
//! `ork-core`).
//!
//! ## Behaviour
//!
//! - `enabled = false` ⇒ returns `None`. Callers wire the result with
//!   `if let Some(mcp) = mcp_client { composite.with_mcp(mcp) }`, so a
//!   disabled `[mcp]` block is a clean no-op rather than a panic.
//! - `enabled = true` and `servers` empty ⇒ still returns `Some(client)`
//!   so per-tenant `TenantSettings.mcp_servers` overlays (ADR 0010
//!   §`Server registration`, deferred follow-up) can drive the
//!   `mcp:` route at runtime even when no globals are registered.
//!
//! ## Why we build our own `reqwest::Client`
//!
//! `ork-mcp` is pinned to `reqwest = 0.13` to satisfy `rmcp 0.16`'s
//! `StreamableHttpClient` trait (impl'd only for that line), while the
//! rest of the workspace stays on `reqwest = 0.12`. Accepting a typed
//! `reqwest::Client` from `ork-api` would propagate the version skew
//! across the public API, so the helper builds its own client here
//! from a plain `user_agent: &str` instead. The HTTP/2 connection
//! pool inside `ork-mcp` is therefore separate from the one driving
//! the A2A remote-builder, which is acceptable because the two stacks
//! talk to disjoint sets of upstreams.
//!
//! The helper is intentionally synchronous because constructing the
//! client today only allocates: no IO until the first `execute` /
//! `refresh_*` call. The signature is `Result<_>` anyway so the
//! follow-up tenant-overlay path (which will call into the tenant
//! repo) can fail cleanly without breaking call sites.

use std::sync::Arc;

use anyhow::Context;
use ork_common::config::McpAppConfig;

use crate::client::{DEFAULT_DESCRIPTOR_TTL, McpClient};

/// Build an MCP client from the global `[mcp]` section of `AppConfig`.
///
/// Returns `Ok(None)` when the section is disabled so the boot path can
/// skip wiring `with_mcp` on the composite executor entirely.
///
/// `user_agent` is propagated to every outbound HTTP request so MCP
/// vendors see something more useful than `reqwest/<ver>` in their
/// access logs.
///
/// # Errors
///
/// Returns an `anyhow::Error` when the internal `reqwest::Client`
/// builder rejects the user agent (e.g. invalid header bytes).
pub fn build_from_config(
    cfg: &McpAppConfig,
    user_agent: &str,
) -> anyhow::Result<Option<Arc<McpClient>>> {
    if !cfg.enabled {
        return Ok(None);
    }
    let http = reqwest::Client::builder()
        .user_agent(user_agent)
        .build()
        .context("ADR-0010: build reqwest client for ork-mcp")?;
    let client = McpClient::from_global_servers(
        cfg.servers.clone(),
        cfg.session_idle_ttl(),
        DEFAULT_DESCRIPTOR_TTL,
        http,
    );
    Ok(Some(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_config_returns_none() {
        let cfg = McpAppConfig {
            enabled: false,
            ..McpAppConfig::default()
        };
        let result = build_from_config(&cfg, "ork-test/0.1").expect("infallible on disabled");
        assert!(
            result.is_none(),
            "ADR-0010 §`enabled = false` must yield no client so ork-api can skip with_mcp wiring"
        );
    }

    #[tokio::test]
    async fn enabled_with_no_servers_still_returns_client() {
        // Tenant-scoped servers (ADR 0010 follow-up) may register at
        // runtime even when the global section is empty, so we must
        // hand back a usable client.
        let cfg = McpAppConfig::default();
        assert!(cfg.enabled, "default must be enabled (ADR 0010)");
        assert!(cfg.servers.is_empty(), "default has no global servers");
        let result = build_from_config(&cfg, "ork-test/0.1").expect("default cfg must build");
        let client = result.expect("enabled config must yield Some(client)");
        assert_eq!(client.sources().global_len(), 0);
    }

    #[tokio::test]
    async fn invalid_user_agent_surfaces_as_error() {
        // Newlines in headers are forbidden by `reqwest::Client::builder`;
        // we want a clear `anyhow::Error` rather than a panic. We avoid
        // `Result::unwrap_err` here because `Option<Arc<McpClient>>` is
        // not `Debug`; pattern-match instead.
        let cfg = McpAppConfig::default();
        match build_from_config(&cfg, "bad\nagent") {
            Ok(_) => panic!("invalid user-agent must surface as Err"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("ADR-0010"),
                    "error must mention ADR-0010 for grep-ability; got: {msg}"
                );
            }
        }
    }
}
