//! ADR 0010 §`Server registration` config types.
//!
//! The actual type definitions live in
//! [`ork_common::mcp_config`](ork_common::mcp_config) so that
//! `ork-core::TenantSettings` and `ork-common::AppConfig` (both upstream
//! of `ork-mcp` in the dep graph) can reference the same shape without
//! introducing a cycle. We re-export them here so existing call sites
//! that say `ork_mcp::McpServerConfig` keep compiling and so the whole
//! "config models for MCP" surface still lives at `crate::config`.
//!
//! See the module docs of [`ork_common::mcp_config`] for the reasoning
//! behind the location of the canonical definitions.

pub use ork_common::mcp_config::{McpAuthConfig, McpServerConfig, McpTransportConfig};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Round-trips the YAML example in ADR 0010 §`Server registration`
    /// (atlassian streamable-http with oauth2 + local-fs stdio) so we
    /// don't regress the documented config syntax.
    #[test]
    fn parses_adr_0010_yaml_example() {
        let yaml = r#"
mcp_servers:
  - id: atlassian
    transport:
      type: streamable_http
      url: https://mcp-atlassian.tenant-a.example.com/
      auth:
        type: oauth2_client_credentials
        token_url: https://auth.tenant-a.example.com/oauth/token
        client_id_env: ATLASSIAN_MCP_CLIENT_ID
        client_secret_env: ATLASSIAN_MCP_SECRET
        scopes: ["read:jira", "write:jira"]
  - id: local-fs
    transport:
      type: stdio
      command: mcp-fs
      args: ["--root", "/tenants/a/files"]
"#;

        #[derive(Debug, serde::Deserialize)]
        struct Wrapper {
            mcp_servers: Vec<McpServerConfig>,
        }

        let parsed: Wrapper = serde_yaml::from_str(yaml).expect("yaml parses");
        assert_eq!(parsed.mcp_servers.len(), 2);

        let atlassian = &parsed.mcp_servers[0];
        assert_eq!(atlassian.id, "atlassian");
        match &atlassian.transport {
            McpTransportConfig::StreamableHttp { url, auth } => {
                assert_eq!(url.as_str(), "https://mcp-atlassian.tenant-a.example.com/");
                match auth {
                    McpAuthConfig::Oauth2ClientCredentials {
                        token_url,
                        client_id_env,
                        client_secret_env,
                        scopes,
                    } => {
                        assert_eq!(
                            token_url.as_str(),
                            "https://auth.tenant-a.example.com/oauth/token"
                        );
                        assert_eq!(client_id_env, "ATLASSIAN_MCP_CLIENT_ID");
                        assert_eq!(client_secret_env, "ATLASSIAN_MCP_SECRET");
                        assert_eq!(
                            scopes,
                            &vec!["read:jira".to_string(), "write:jira".to_string()]
                        );
                    }
                    other => panic!("expected oauth2_client_credentials, got {other:?}"),
                }
            }
            other => panic!("expected streamable_http transport, got {other:?}"),
        }

        let local = &parsed.mcp_servers[1];
        assert_eq!(local.id, "local-fs");
        match &local.transport {
            McpTransportConfig::Stdio { command, args, env } => {
                assert_eq!(command, "mcp-fs");
                assert_eq!(
                    args,
                    &vec!["--root".to_string(), "/tenants/a/files".to_string()]
                );
                assert!(env.is_empty(), "stdio env defaults to empty when omitted");
            }
            other => panic!("expected stdio transport, got {other:?}"),
        }
    }

    #[test]
    fn auth_config_defaults_to_none() {
        let yaml = r#"
id: legacy
transport:
  type: sse
  url: https://mcp-legacy.example.com/sse
"#;
        let parsed: McpServerConfig = serde_yaml::from_str(yaml).expect("parses");
        match parsed.transport {
            McpTransportConfig::Sse { auth, .. } => {
                assert_eq!(auth, McpAuthConfig::None);
            }
            other => panic!("expected sse, got {other:?}"),
        }
    }

    #[test]
    fn json_round_trip_preserves_all_fields() {
        // The same value gets persisted to a JSONB column in
        // `tenants.settings`; this test guards against silent serde
        // drift (e.g. a missing `#[serde(default)]` flipping a field to
        // required).
        let original = McpServerConfig {
            id: "everything".to_string(),
            transport: McpTransportConfig::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    "@modelcontextprotocol/server-everything".into(),
                ],
                env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: McpServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn api_key_auth_round_trip() {
        let yaml = r#"
id: vendor
transport:
  type: streamable_http
  url: https://mcp-vendor.example.com/
  auth:
    type: api_key
    header: X-API-Key
    value_env: VENDOR_MCP_KEY
"#;
        let parsed: McpServerConfig = serde_yaml::from_str(yaml).expect("parses");
        match parsed.transport {
            McpTransportConfig::StreamableHttp { auth, .. } => match auth {
                McpAuthConfig::ApiKey { header, value_env } => {
                    assert_eq!(header, "X-API-Key");
                    assert_eq!(value_env, "VENDOR_MCP_KEY");
                }
                other => panic!("expected api_key, got {other:?}"),
            },
            other => panic!("expected streamable_http, got {other:?}"),
        }
    }

    #[test]
    fn bearer_auth_round_trip() {
        let yaml = r#"
id: hosted
transport:
  type: streamable_http
  url: https://mcp.example.com/
  auth:
    type: bearer
    value_env: MCP_TOKEN
"#;
        let parsed: McpServerConfig = serde_yaml::from_str(yaml).expect("parses");
        match parsed.transport {
            McpTransportConfig::StreamableHttp { auth, .. } => match auth {
                McpAuthConfig::Bearer { value_env } => assert_eq!(value_env, "MCP_TOKEN"),
                other => panic!("expected bearer, got {other:?}"),
            },
            other => panic!("expected streamable_http, got {other:?}"),
        }
    }
}
