//! ADR-0010 boot smoke test.
//!
//! Asserts the new `[mcp]` wiring in `ork-api` doesn't regress the
//! happy boot path:
//!
//! - With `enabled = false`, [`ork_mcp::build_from_config`] returns
//!   `None` and the rest of `main.rs` must skip the `with_mcp` arm
//!   without panicking.
//! - With the default `[mcp]` (enabled, no servers), the helper hands
//!   back a usable client and `list_tools_for_tenant` is empty
//!   (no servers ⇒ no descriptors).
//!
//! We don't stand up the full Postgres-backed `AppState` here — that
//! requires a database, mirroring `boot.rs`. The test exercises the
//! exact call chain `AppConfig::default() → ork_mcp::build_from_config`,
//! which is the seam introduced by ADR-0010 in `main.rs`.

use ork_common::config::{AppConfig, McpAppConfig};
use ork_common::types::TenantId;

#[tokio::test]
async fn build_from_default_config_returns_some_client() {
    let config = AppConfig::default();
    assert!(
        config.mcp.enabled,
        "ADR-0010: default `[mcp]` must boot the client so tenant overlays work even with no global servers"
    );
    assert!(
        config.mcp.servers.is_empty(),
        "default config has no global MCP servers"
    );

    let client = ork_mcp::build_from_config(&config.mcp, &config.a2a_client.user_agent)
        .expect("build_from_config must not fail on default config");
    let client = client.expect("default `[mcp] enabled = true` must yield Some(client)");
    assert_eq!(
        client.sources().global_len(),
        0,
        "default config has no global servers"
    );
    assert!(
        client.list_tools_for_tenant(TenantId::new()).is_empty(),
        "fresh client has no cached descriptors yet"
    );
}

#[tokio::test]
async fn build_from_disabled_config_returns_none() {
    let config = AppConfig {
        mcp: McpAppConfig {
            enabled: false,
            ..McpAppConfig::default()
        },
        ..AppConfig::default()
    };

    let client = ork_mcp::build_from_config(&config.mcp, &config.a2a_client.user_agent)
        .expect("disabled config must not error");
    assert!(
        client.is_none(),
        "ADR-0010 `[mcp] enabled = false` must yield None so main.rs skips with_mcp wiring"
    );
}
