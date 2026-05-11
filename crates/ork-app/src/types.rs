//! Structural configuration types for [`crate::OrkAppBuilder`](super::OrkAppBuilder).

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// ADR-0055 §`Mount mechanics`: bearer-token gate for Studio when the
/// server binds a non-loopback interface. The token is generated on
/// `ork dev` boot and printed once to stdout.
#[derive(Clone, Debug)]
pub struct StudioAuth {
    token: SecretString,
}

impl StudioAuth {
    /// Construct a new bearer-token gate. Returns an error when the
    /// token is empty — an empty configured token paired with an
    /// `Authorization: Bearer ` header (trailing space stripped to
    /// empty) would otherwise compare equal in [`Self::matches`]
    /// and authenticate the caller (reviewer finding m7).
    pub fn new(token: SecretString) -> Result<Self, &'static str> {
        if token.expose_secret().is_empty() {
            return Err("studio bearer token must be non-empty");
        }
        Ok(Self { token })
    }

    /// Compare against an incoming `Authorization: Bearer <token>` value
    /// in constant time. The `secrecy` wrapper guarantees the token
    /// does not appear in debug output or accidental logs.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        // Constant-time compare: `secrecy` exposes `&str`; defer to
        // `subtle`-equivalent equality through `ct_eq` via slice cmp on
        // equal-length inputs. The length-mismatch short circuit is
        // unavoidable but does not leak the secret itself.
        let expected = self.token.expose_secret();
        if expected.len() != candidate.len() {
            return false;
        }
        // Byte-wise XOR accumulator (constant time for equal length).
        let mut diff: u8 = 0;
        for (a, b) in expected.as_bytes().iter().zip(candidate.as_bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// Borrow the raw token string. Use sparingly — only at the wire
    /// boundary where the operator needs to copy it into a client.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.token.expose_secret()
    }
}

impl PartialEq for StudioAuth {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other.token.expose_secret())
    }
}

impl Eq for StudioAuth {}

impl Serialize for StudioAuth {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // ADR-0055 §`Authentication`: the bearer token is only printed
        // once on boot; manifests/configs round-tripping through serde
        // must redact the value so it doesn't leak via the manifest
        // endpoint or saved snapshots.
        ser.serialize_str("***")
    }
}

impl<'de> Deserialize<'de> for StudioAuth {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;
        let raw = String::deserialize(de)?;
        Self::new(SecretString::from(raw)).map_err(D::Error::custom)
    }
}

/// Studio (ADR-0055) mount intent.
///
/// - `Disabled`: do not mount `/studio` or `/studio/api/*`. Default in
///   production builds (see [`ServerConfig::production`]).
/// - `Enabled`: mount Studio. Refused at `OrkApp::serve()` time when the
///   server binds a non-loopback interface (ADR-0055 AC #4).
/// - `EnabledWithAuth(StudioAuth)`: mount Studio gated by a bearer
///   token on `/studio/api/*` and the static asset routes.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StudioConfig {
    #[default]
    Disabled,
    Enabled,
    EnabledWithAuth(StudioAuth),
}

impl StudioConfig {
    /// True when Studio is mounted (in either arm) — used by
    /// `router_for`'s composition layer to decide whether to merge
    /// the `ork_studio::router(...)` into the served axum app.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Borrow the configured bearer token, if any. Returns `None`
    /// for `Disabled` and `Enabled` (no auth).
    #[must_use]
    pub fn auth(&self) -> Option<&StudioAuth> {
        match self {
            Self::EnabledWithAuth(a) => Some(a),
            _ => None,
        }
    }
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
    /// **Caveat (reviewer m2):** this `Default` reads the process-wide
    /// `ORK_DEV` env var so a binary spawned by `ork dev` gets
    /// `StudioConfig::Enabled` without the user's `main.rs` doing
    /// anything explicit (`crates/ork-cli/src/dev/child.rs` sets the
    /// var). A workspace test that calls `std::env::set_var("ORK_DEV",
    /// "1")` inside the test process would flip every parallel test
    /// that calls `ServerConfig::default()`. Tests that care should
    /// construct an explicit `ServerConfig { studio: ..., .. }` rather
    /// than relying on `Default`.
    fn default() -> Self {
        // ADR-0055 AC #3: when the user binary runs under `ork dev`
        // (which sets `ORK_DEV=1`), default Studio to `Enabled` so the
        // dashboard mounts without forcing the user's `main.rs` to
        // toggle it manually. Outside `ork dev` the default stays
        // `Disabled` so `ork start` ships Studio off.
        let studio = if std::env::var_os("ORK_DEV").is_some() {
            StudioConfig::Enabled
        } else {
            StudioConfig::default()
        };
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            tls: None,
            auth: None,
            resume_on_startup: false,
            swagger_ui: true,
            default_tenant: None,
            studio,
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
