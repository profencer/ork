//! ADR 0010 §`Tool name convention`. MCP tools are exposed under
//! `mcp:<server_id>.<tool_name>` so [`CompositeToolExecutor`](
//! ../../ork-integrations/src/tools.rs) can route by prefix without
//! colliding with the existing `agent_call`, `code_*`, and integration
//! tool names.
//!
//! This module owns:
//!
//! - [`McpToolDescriptor`]: the shape we cache after `tools/list` against
//!   each MCP server. Mirrors `rmcp::model::Tool` but carries the
//!   `server_id` so a single flat `Vec<McpToolDescriptor>` can describe a
//!   whole tenant's catalog.
//! - [`parse_mcp_tool_name`]: the *strict* parser. Callers (notably
//!   `CompositeToolExecutor`) gate on the `mcp:` prefix themselves and
//!   only invoke this once they're already on the MCP arm; the parser
//!   refuses non-prefixed names and missing-dot inputs loudly so
//!   misrouted calls don't silently hit the wrong executor.

use ork_common::error::OrkError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cached descriptor for one MCP tool. Populated from `tools/list` against
/// each registered MCP server (see [`crate::client::McpClient::refresh_all`]).
///
/// `input_schema` is left as a free-form `serde_json::Value` so we can ship
/// whatever JSON Schema the server returns to ADR 0011's tool-calling loop
/// without round-tripping through a typed model that would discard
/// vendor-specific keywords.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpToolDescriptor {
    /// Identifier of the MCP server this tool came from. Matches
    /// [`McpServerConfig::id`](crate::config::McpServerConfig::id).
    pub server_id: String,

    /// Name as advertised by the MCP server in `tools/list`. **Not**
    /// prefixed with `mcp:<server>.`; build the qualified name via
    /// [`Self::qualified_name`].
    pub tool_name: String,

    /// Optional human-facing description from the MCP server, intended for
    /// the LLM tool catalog rendered in ADR 0011.
    #[serde(default)]
    pub description: Option<String>,

    /// JSON-Schema-shaped object describing the tool's parameters. Stored
    /// as opaque JSON to avoid lossy round-tripping (see module docs).
    pub input_schema: Value,
}

impl McpToolDescriptor {
    /// Returns the namespaced tool name as exposed to the rest of ork.
    /// E.g. `McpToolDescriptor { server_id: "atlassian", tool_name:
    /// "search_jira", .. }.qualified_name() == "mcp:atlassian.search_jira"`.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("mcp:{}.{}", self.server_id, self.tool_name)
    }
}

/// Strict parser for the `mcp:<server_id>.<tool_name>` namespace.
///
/// **Rejects** anything that doesn't begin with the literal `mcp:` prefix —
/// the routing decision lives in `CompositeToolExecutor`, not here. This
/// keeps the parser composable with future routing layers (e.g. the
/// per-agent `tools:` allow-list described in ADR 0010 §`Tool discovery`).
///
/// Returns `(server_id, tool_name)` on success.
///
/// # Errors
///
/// - [`OrkError::Validation`] when the input is missing the `mcp:` prefix,
///   has an empty `<server_id>` or `<tool_name>` half, or lacks the `.`
///   separator.
pub fn parse_mcp_tool_name(qualified: &str) -> Result<(String, String), OrkError> {
    let body = qualified.strip_prefix("mcp:").ok_or_else(|| {
        OrkError::Validation(format!(
            "MCP tool name must start with `mcp:`, got `{qualified}` (ADR-0010)"
        ))
    })?;

    let (server, tool) = body.split_once('.').ok_or_else(|| {
        OrkError::Validation(format!(
            "MCP tool name `{qualified}` is missing the `.<tool>` suffix (expected `mcp:<server>.<tool>`)"
        ))
    })?;

    if server.is_empty() {
        return Err(OrkError::Validation(format!(
            "MCP tool name `{qualified}` has an empty <server> segment"
        )));
    }
    if tool.is_empty() {
        return Err(OrkError::Validation(format!(
            "MCP tool name `{qualified}` has an empty <tool> segment"
        )));
    }

    Ok((server.to_string(), tool.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_simple_name() {
        let (server, tool) = parse_mcp_tool_name("mcp:atlassian.search_jira").unwrap();
        assert_eq!(server, "atlassian");
        assert_eq!(tool, "search_jira");
    }

    #[test]
    fn parses_tool_name_with_inner_dot() {
        // MCP tools may contain dots in their name; only the FIRST dot
        // separates server from tool. This guards against a regression
        // where someone "improves" the parser to use rsplit_once / splitn
        // and silently breaks vendor-specific tool names.
        let (server, tool) = parse_mcp_tool_name("mcp:gh.repos.list_pulls").unwrap();
        assert_eq!(server, "gh");
        assert_eq!(tool, "repos.list_pulls");
    }

    #[test]
    fn rejects_missing_prefix() {
        let err = parse_mcp_tool_name("github_recent_activity").unwrap_err();
        match err {
            OrkError::Validation(msg) => assert!(msg.contains("mcp:")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_dot() {
        let err = parse_mcp_tool_name("mcp:atlassian").unwrap_err();
        match err {
            OrkError::Validation(msg) => assert!(msg.contains("missing the `.<tool>`")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_server() {
        let err = parse_mcp_tool_name("mcp:.echo").unwrap_err();
        assert!(matches!(err, OrkError::Validation(_)));
    }

    #[test]
    fn rejects_empty_tool() {
        let err = parse_mcp_tool_name("mcp:atlassian.").unwrap_err();
        assert!(matches!(err, OrkError::Validation(_)));
    }

    #[test]
    fn descriptor_qualified_name_matches_parser() {
        let descriptor = McpToolDescriptor {
            server_id: "atlassian".into(),
            tool_name: "search_jira".into(),
            description: Some("search Jira".into()),
            input_schema: json!({"type": "object"}),
        };
        let qualified = descriptor.qualified_name();
        assert_eq!(qualified, "mcp:atlassian.search_jira");
        let (server, tool) = parse_mcp_tool_name(&qualified).unwrap();
        assert_eq!(server, descriptor.server_id);
        assert_eq!(tool, descriptor.tool_name);
    }
}
