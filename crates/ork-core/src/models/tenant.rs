use chrono::{DateTime, Utc};
use ork_common::mcp_config::McpServerConfig;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: TenantId,
    pub name: String,
    pub slug: String,
    pub settings: TenantSettings,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantSettings {
    pub llm_api_key_encrypted: Option<String>,
    pub github_token_encrypted: Option<String>,
    pub gitlab_token_encrypted: Option<String>,
    pub gitlab_base_url: Option<String>,
    pub default_repos: Vec<String>,
    /// Per-tenant MCP servers (ADR 0010 §`Server registration` — first
    /// source). `#[serde(default)]` ensures tenants persisted before
    /// ADR 0010 deserialise unchanged. Replaces the global `[mcp.servers]`
    /// entry for this tenant when an `id` matches; otherwise both stacks
    /// merge (tenant entries take precedence on collision).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

impl Default for TenantSettings {
    fn default() -> Self {
        Self {
            llm_api_key_encrypted: None,
            github_token_encrypted: None,
            gitlab_token_encrypted: None,
            gitlab_base_url: None,
            default_repos: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTenantSettingsRequest {
    pub llm_api_key: Option<String>,
    pub github_token: Option<String>,
    pub gitlab_token: Option<String>,
    pub gitlab_base_url: Option<String>,
    pub default_repos: Option<Vec<String>>,
    /// ADR 0010 §`Server registration`. `None` = "don't touch the
    /// existing list"; `Some([])` clears the tenant's MCP servers
    /// entirely (so operators can roll back without a schema change).
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServerConfig>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::mcp_config::McpTransportConfig;

    #[test]
    fn tenant_settings_round_trips_without_mcp_servers_field() {
        // Crucial back-compat guarantee: a tenant row written before
        // ADR 0010 (no `mcp_servers` key in its JSONB blob) must still
        // deserialise. `#[serde(default)]` should fill in an empty Vec.
        let json = serde_json::json!({
            "llm_api_key_encrypted": null,
            "github_token_encrypted": null,
            "gitlab_token_encrypted": null,
            "gitlab_base_url": null,
            "default_repos": []
        });
        let parsed: TenantSettings = serde_json::from_value(json).expect("legacy row parses");
        assert!(
            parsed.mcp_servers.is_empty(),
            "missing `mcp_servers` must default to an empty list (ADR-0010 back-compat)"
        );
    }

    #[test]
    fn tenant_settings_round_trips_with_mcp_servers() {
        let original = TenantSettings {
            llm_api_key_encrypted: None,
            github_token_encrypted: None,
            gitlab_token_encrypted: None,
            gitlab_base_url: None,
            default_repos: Vec::new(),
            mcp_servers: vec![McpServerConfig {
                id: "atlassian".into(),
                transport: McpTransportConfig::StreamableHttp {
                    url: url::Url::parse("https://mcp.example.com/").unwrap(),
                    auth: ork_common::mcp_config::McpAuthConfig::Bearer {
                        value_env: "MCP_TOKEN".into(),
                    },
                },
            }],
        };
        let json = serde_json::to_value(&original).unwrap();
        let back: TenantSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back.mcp_servers.len(), 1);
        assert_eq!(back.mcp_servers[0].id, "atlassian");
    }

    #[test]
    fn update_request_defaults_mcp_servers_to_none() {
        // `None` semantics for `mcp_servers`: "leave the existing list
        // alone". A request that touches only the github token must NOT
        // wipe the tenant's MCP server list.
        let json = serde_json::json!({
            "github_token": "ghp_xxx"
        });
        let parsed: UpdateTenantSettingsRequest = serde_json::from_value(json).unwrap();
        assert!(parsed.mcp_servers.is_none());
        assert_eq!(parsed.github_token.as_deref(), Some("ghp_xxx"));
    }

    #[test]
    fn update_request_supports_empty_list_to_clear() {
        // `Some([])` semantics: explicitly clear the tenant's MCP
        // servers. Tested so a future refactor doesn't accidentally
        // collapse `Some([])` into `None`.
        let json = serde_json::json!({ "mcp_servers": [] });
        let parsed: UpdateTenantSettingsRequest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.mcp_servers.as_deref().map(<[_]>::len), Some(0));
    }
}
