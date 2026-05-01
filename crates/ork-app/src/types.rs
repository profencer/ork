//! Structural configuration types for [`crate::OrkAppBuilder`](super::OrkAppBuilder).

use serde::{Deserialize, Serialize};

/// Deployment environment tag (introspection / manifest).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Development,
    Staging,
    Production,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    /// Logical auth mode name (expanded in ADR 0056).
    pub mode: String,
}

/// HTTP listen + TLS + auth intent for the auto-generated server (ADR 0056).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// When true, [`crate::OrkApp::serve`] replays pending workflow snapshots (ADR-0050).
    #[serde(default)]
    pub resume_on_startup: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            tls: None,
            auth: None,
            resume_on_startup: false,
        }
    }
}

/// MCP server registration (transport details land in ADR 0051).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransportStub {
    #[default]
    Deferred,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerSpec {
    #[serde(default)]
    pub transport: McpTransportStub,
}

impl Default for McpServerSpec {
    fn default() -> Self {
        Self {
            transport: McpTransportStub::Deferred,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScorerTarget {
    Agent { id: String },
    Workflow { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScorerSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScorerBinding {
    pub target: ScorerTarget,
    pub scorer: ScorerSpec,
}

/// Placeholder until ADR 0058 observability ships.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub traces: bool,
    #[serde(default)]
    pub metrics: bool,
}
