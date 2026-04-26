use chrono::{DateTime, Utc};
use ork_common::config::LlmProviderConfig;
use ork_common::mcp_config::McpServerConfig;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};

/// Tenant override for one entry in the [`ork_common::config::LlmConfig`]
/// catalog. Same on-disk shape as the operator-side
/// [`LlmProviderConfig`] — by design, per ADR 0012 §`Tenant overrides`:
/// operators get one mental model. A tenant entry with the same `id` as an
/// operator entry replaces it (mirrors `mcp_servers` from ADR 0010).
pub type TenantLlmProviderConfig = LlmProviderConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: TenantId,
    pub name: String,
    pub slug: String,
    pub settings: TenantSettings,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TenantSettings {
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
    /// Per-tenant LLM provider catalog overrides (ADR 0012
    /// §`Tenant overrides`). Same `id`-collision-replaces semantics as
    /// `mcp_servers`. `#[serde(default)]` so rows persisted before
    /// ADR 0012 deserialise unchanged.
    #[serde(default)]
    pub llm_providers: Vec<TenantLlmProviderConfig>,
    /// Tenant default provider id; overrides
    /// [`ork_common::config::LlmConfig::default_provider`] when set.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Tenant default model name; resolved after the provider chain. When
    /// set, beats the resolved provider's `default_model` but loses to
    /// step/agent/request-level model overrides per ADR 0012 §`Selection`.
    #[serde(default)]
    pub default_model: Option<String>,
    /// ADR-0016: optional per-tenant override for artifact retention (days).
    /// `None` = use operator `[retention]` defaults in the sweep worker.
    #[serde(default)]
    pub artifact_retention_days: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTenantSettingsRequest {
    pub github_token: Option<String>,
    pub gitlab_token: Option<String>,
    pub gitlab_base_url: Option<String>,
    pub default_repos: Option<Vec<String>>,
    /// ADR 0010 §`Server registration`. `None` = "don't touch the
    /// existing list"; `Some([])` clears the tenant's MCP servers
    /// entirely (so operators can roll back without a schema change).
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServerConfig>>,
    /// ADR 0012 §`Tenant overrides`. Same `None` / `Some([])` semantics
    /// as [`Self::mcp_servers`]: missing field leaves the tenant's
    /// catalog alone, an explicit empty list clears it.
    #[serde(default)]
    pub llm_providers: Option<Vec<TenantLlmProviderConfig>>,
    /// ADR 0012 §`Selection`. Use `Some("")` is *not* special-cased —
    /// pass `None` to leave the existing default alone.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// ADR 0012 §`Selection`. Same `None` semantics as
    /// [`Self::default_provider`].
    #[serde(default)]
    pub default_model: Option<String>,
    /// ADR-0016. `None` = leave existing setting; `Some(0)` can mean
    /// "clear override" at the API layer if we add explicit semantics later.
    #[serde(default)]
    pub artifact_retention_days: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::config::HeaderValueSource;
    use ork_common::mcp_config::McpTransportConfig;

    #[test]
    fn tenant_settings_round_trips_without_optional_fields() {
        // Crucial back-compat guarantee: a tenant row written before
        // ADR 0010/0012 (no `mcp_servers`/`llm_providers` keys in its
        // JSONB blob) must still deserialise. `#[serde(default)]` should
        // fill in empty Vecs / None.
        let json = serde_json::json!({
            "github_token_encrypted": null,
            "gitlab_token_encrypted": null,
            "gitlab_base_url": null,
            "default_repos": []
        });
        let parsed: TenantSettings = serde_json::from_value(json).expect("legacy row parses");
        assert!(parsed.mcp_servers.is_empty());
        assert!(parsed.llm_providers.is_empty());
        assert!(parsed.default_provider.is_none());
        assert!(parsed.default_model.is_none());
        assert!(parsed.artifact_retention_days.is_none());
    }

    #[test]
    fn tenant_settings_round_trips_with_mcp_servers() {
        let original = TenantSettings {
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
            llm_providers: Vec::new(),
            default_provider: None,
            default_model: None,
            artifact_retention_days: None,
        };
        let json = serde_json::to_value(&original).unwrap();
        let back: TenantSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back.mcp_servers.len(), 1);
        assert_eq!(back.mcp_servers[0].id, "atlassian");
    }

    #[test]
    fn tenant_settings_round_trip_with_llm_providers() {
        // ADR 0012: tenant catalog overrides round-trip cleanly through
        // serde with the same shape as the operator catalog.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            HeaderValueSource::Env {
                env: "TENANT_KEY".into(),
            },
        );
        let original = TenantSettings {
            llm_providers: vec![TenantLlmProviderConfig {
                id: "openai".into(),
                base_url: "https://tenant.example.com/v1".into(),
                default_model: Some("gpt-4o".into()),
                headers,
                capabilities: Vec::new(),
            }],
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o-mini".into()),
            ..TenantSettings::default()
        };
        let json = serde_json::to_value(&original).unwrap();
        let back: TenantSettings = serde_json::from_value(json).unwrap();
        assert_eq!(back.llm_providers.len(), 1);
        assert_eq!(back.llm_providers[0].id, "openai");
        assert_eq!(back.default_provider.as_deref(), Some("openai"));
        assert_eq!(back.default_model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn update_request_defaults_optional_lists_to_none() {
        // `None` semantics for `mcp_servers` / `llm_providers`: "leave
        // the existing list alone". A request that touches only the
        // github token must NOT wipe either tenant catalog.
        let json = serde_json::json!({
            "github_token": "ghp_xxx"
        });
        let parsed: UpdateTenantSettingsRequest = serde_json::from_value(json).unwrap();
        assert!(parsed.mcp_servers.is_none());
        assert!(parsed.llm_providers.is_none());
        assert!(parsed.default_provider.is_none());
        assert!(parsed.default_model.is_none());
        assert_eq!(parsed.github_token.as_deref(), Some("ghp_xxx"));
    }

    #[test]
    fn update_request_supports_empty_list_to_clear() {
        // `Some([])` semantics: explicitly clear the tenant's MCP
        // servers. Tested so a future refactor doesn't accidentally
        // collapse `Some([])` into `None`.
        let json = serde_json::json!({ "mcp_servers": [], "llm_providers": [] });
        let parsed: UpdateTenantSettingsRequest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.mcp_servers.as_deref().map(<[_]>::len), Some(0));
        assert_eq!(parsed.llm_providers.as_deref().map(<[_]>::len), Some(0));
    }
}
