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

/// Studio (ADR-0055) mount intent. v1 (ADR-0057) ships the gate only;
/// the `Enabled` arm is reserved for the follow-up ADR that adds the
/// actual `crates/ork-studio/` crate and the `--features
/// ork-webui/embed-spa` bundle build that `ork build` will eventually
/// drive. Today both arms are no-ops in the auto router; the field
/// exists so `ork start` and `ork dev` can plumb the user's intent.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StudioConfig {
    #[default]
    Disabled,
    Enabled,
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
    /// Mount `/swagger-ui` against the auto-generated `/api/openapi.json` (ADR-0056).
    /// Defaults to `true`; production deployments can flip it off via
    /// [`crate::OrkAppBuilder::server`] after constructing a [`Self::production`].
    #[serde(default = "default_swagger_ui")]
    pub swagger_ui: bool,
    /// Header name carrying the explicit caller tenant (ADR-0020).
    /// `400` is returned when missing unless [`Self::default_tenant`] is set.
    #[serde(default)]
    pub default_tenant: Option<String>,
    /// Studio (ADR-0055) mount gate. ADR-0057 wires the field through
    /// `ork dev`/`ork start`; the bundle and panel API land later.
    #[serde(default)]
    pub studio: StudioConfig,
}

fn default_swagger_ui() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            tls: None,
            auth: None,
            resume_on_startup: false,
            swagger_ui: true,
            default_tenant: None,
            studio: StudioConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Convenience: production defaults — bind 0.0.0.0:8080, `/swagger-ui` off,
    /// Studio off (ADR-0057 §`ork start`).
    #[must_use]
    pub fn production() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            tls: None,
            auth: None,
            resume_on_startup: true,
            swagger_ui: false,
            default_tenant: None,
            studio: StudioConfig::Disabled,
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

// ADR-0054: rich scorer registration types live in `ork-eval`. They
// are re-exported here so `OrkAppBuilder::scorer(...)` callers do not
// need to import `ork-eval` directly when they are not authoring
// scorers themselves.
pub use ork_eval::{ScorerSpec, ScorerTarget};

/// Runtime binding stored in [`crate::OrkApp`]: the `(target, spec)`
/// pair produced by [`crate::OrkAppBuilder::scorer`]. Owns the
/// `Arc<dyn Scorer>` (via `spec`), so this struct is **not**
/// `Serialize` — the manifest summary lives in [`crate::manifest`].
#[derive(Clone)]
pub struct ScorerBinding {
    pub target: ScorerTarget,
    pub spec: ScorerSpec,
}

/// Placeholder until ADR 0058 observability ships.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub traces: bool,
    #[serde(default)]
    pub metrics: bool,
}
