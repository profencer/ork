//! ADR 0010 — MCP as the canonical external tool plane.
//!
//! `ork-mcp` is the Rust client side of MCP. It owns the connection pool to
//! configured MCP servers (stdio child processes, streamable-HTTP endpoints,
//! and the legacy SSE transport), exposes their tools through the
//! [`ork_core::workflow::engine::ToolExecutor`] surface under the
//! `mcp:<server>.<tool>` namespace, and caches tool descriptors so the
//! ADR-0011 native tool-calling loop has a stable list to render to the LLM.
//!
//! The crate is intentionally **client-only** for now. The `resources`,
//! `prompts`, and `sampling` capabilities of MCP are explicitly out of scope
//! per ADR 0010 §`Resources, prompts, and sampling`.

pub mod auth;
pub mod boot;
pub mod cache;
pub mod client;
pub mod config;
pub mod descriptor;
pub mod session;
pub mod transport;

pub use auth::{CcTokenCache, fetch_client_credentials_token};
pub use boot::build_from_config;
pub use cache::TtlCache;
pub use client::{DEFAULT_DESCRIPTOR_TTL, DescriptorKey, McpClient, McpConfigSources};
pub use config::{McpAuthConfig, McpServerConfig, McpTransportConfig};
pub use descriptor::{McpToolDescriptor, parse_mcp_tool_name};
pub use session::{GenericSessionPool, SessionPool};

impl ork_agents::tool_catalog::McpToolCatalog for McpClient {
    fn list_for_tenant(
        &self,
        tenant: ork_common::types::TenantId,
    ) -> Vec<ork_agents::tool_catalog::McpToolCatalogEntry> {
        self.list_tools_for_tenant(tenant)
            .into_iter()
            .map(|d| ork_agents::tool_catalog::McpToolCatalogEntry {
                server_id: d.server_id,
                tool_name: d.tool_name,
                description: d.description,
                input_schema: d.input_schema,
            })
            .collect()
    }
}
