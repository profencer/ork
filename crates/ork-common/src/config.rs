use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub auth: AuthConfig,
    pub llm: LlmConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub repositories: Vec<RepositoryEntry>,
    #[serde(default)]
    pub kafka: KafkaConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Static `[[remote_agents]]` entries (ADR-0007). Each entry is materialised
    /// into a long-lived `A2aRemoteAgent` at boot and re-fetched every
    /// [`A2aClientToml::card_refresh_interval_secs`].
    #[serde(default)]
    pub remote_agents: Vec<RemoteAgentEntryToml>,
    /// Defaults applied to every `A2aRemoteAgent` constructed from
    /// `remote_agents`, the discovery subscriber, or workflow inline cards.
    #[serde(default)]
    pub a2a_client: A2aClientToml,
    /// Deployment environment selector. Values are free-form (`dev`, `staging`,
    /// `prod`); right now only `"dev"` is special — it relaxes the HTTPS-only
    /// check on `tasks/pushNotificationConfig/set`. Defaults to `"dev"` so
    /// local runs stay developer-friendly.
    #[serde(default = "default_env")]
    pub env: String,
    /// ADR-0009 push notifications.
    #[serde(default)]
    pub push: PushConfig,
    /// ADR-0010 MCP tool plane. Empty by default so existing dev
    /// deployments keep booting without an MCP server in sight.
    #[serde(default)]
    pub mcp: McpAppConfig,
    /// Generic ingress gateways (ADR-0013). Empty by default.
    #[serde(default)]
    pub gateways: Vec<GatewayConfig>,
    /// ADR-0016: blob storage (FS/S3) + Postgres metadata index. Disabled by
    /// default so tests and dev shells boot without a writable artifact root.
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
    /// ADR-0016: scheduled artifact retention (Postgres + blob delete).
    #[serde(default)]
    pub retention: RetentionConfig,
    /// ADR-0020 §`Secrets handling`: KMS provider selection for tenant
    /// envelope encryption. Defaults to the legacy adapter (KEK derived
    /// from `auth.jwt_secret`) so existing deployments keep working
    /// without configuration changes.
    #[serde(default)]
    pub security: SecurityConfig,
}

/// ADR-0020 §`Secrets handling`. Cloud-KMS adapters (AWS / GCP / Azure /
/// Vault) are deferred to follow-up ADRs per the user decision recorded
/// in `docs/adrs/0020-tenant-security-and-trust.md`. The shape is
/// intentionally extensible — new providers attach at the [`KmsConfig`]
/// enum level.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct SecurityConfig {
    #[serde(default)]
    pub kms: KmsConfig,
}

/// One-of provider selection. Today only `legacy` is implemented;
/// `aws`/`gcp`/`azure`/`vault` are reserved variants so config files can
/// reference them without breaking deserialisation when the adapters
/// land in a follow-up ADR.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum KmsConfig {
    /// HKDF-derived KEK from `auth.jwt_secret`. Default; preserves the
    /// pre-ADR-0020 envelope behaviour for `ork_push::encryption`.
    #[default]
    Legacy,
    /// AWS KMS (`aws-sdk-kms`). Adapter is a follow-up ADR.
    Aws { key_arn: String },
    /// Vault Transit. Adapter is a follow-up ADR.
    Vault { addr: String, key_name: String },
    /// GCP KMS. Adapter is a follow-up ADR.
    Gcp { key_resource_name: String },
    /// Azure Key Vault. Adapter is a follow-up ADR.
    Azure { vault_url: String, key_name: String },
}

/// One `[[gateways]]` static entry (ADR-0013). Adapter-specific options live in `config` JSON.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GatewayConfig {
    pub id: String,
    /// Adapter id: `rest`, `webhook`, `event_mesh`, `mcp`, or a plugin-registered name.
    #[serde(rename = "type")]
    pub gateway_type: String,
    #[serde(default = "default_gateway_enabled")]
    pub enabled: bool,
    /// Per-adapter configuration (TOML table → JSON for factories).
    #[serde(default)]
    pub config: serde_json::Value,
}

fn default_gateway_enabled() -> bool {
    true
}

/// ADR-0016: operator knobs for the `ArtifactStore` + chained backends.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArtifactsConfig {
    /// When `false`, the API does not build an `ArtifactStore` (artifact tools
    /// and file rewrites are unavailable).
    #[serde(default = "default_artifacts_enabled")]
    pub enabled: bool,
    /// Default backend for unprefixed logical names (currently only `fs` is
    /// bootstrapped; `s3` is an additional [`Self::s3`] scheme in the chain).
    #[serde(default = "default_artifact_default_backend")]
    pub default_backend: String,
    #[serde(default)]
    pub fs: ArtifactFsConfig,
    #[serde(default)]
    pub s3: Option<ArtifactS3Config>,
}

fn default_artifacts_enabled() -> bool {
    false
}
fn default_artifact_default_backend() -> String {
    "fs".into()
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            enabled: default_artifacts_enabled(),
            default_backend: default_artifact_default_backend(),
            fs: ArtifactFsConfig::default(),
            s3: None,
        }
    }
}

/// Local filesystem `ArtifactStore` root (ADR-0016).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArtifactFsConfig {
    /// Root directory; created at boot if missing when `[artifacts] enabled` is
    /// true.
    #[serde(default = "default_artifact_fs_root")]
    pub root: PathBuf,
}
fn default_artifact_fs_root() -> PathBuf {
    PathBuf::from("./data/artifacts")
}
impl Default for ArtifactFsConfig {
    fn default() -> Self {
        Self {
            root: default_artifact_fs_root(),
        }
    }
}

/// S3-compatible `ArtifactStore` (ADR-0016). Wires only when present.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArtifactS3Config {
    pub bucket: String,
    pub region: String,
    /// Custom endpoint (MinIO, R2, etc.); `force_path_style` is enabled in code.
    pub endpoint: Option<String>,
    /// Env var for access key (read at boot, value not stored in config).
    #[serde(default = "default_s3_key_env")]
    pub access_key_env: String,
    #[serde(default = "default_s3_secret_env")]
    pub secret_key_env: String,
}
fn default_s3_key_env() -> String {
    "AWS_ACCESS_KEY_ID".into()
}
fn default_s3_secret_env() -> String {
    "AWS_SECRET_ACCESS_KEY".into()
}

/// ADR-0016: `eligible_for_sweep` uses these day counts (see the ADR’s SQL).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RetentionConfig {
    #[serde(default = "default_retention_default_days")]
    pub default_days: u32,
    #[serde(default = "default_retention_task_artifacts_days")]
    pub task_artifacts_days: u32,
    #[serde(default = "default_retention_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
}
fn default_retention_default_days() -> u32 {
    90
}
fn default_retention_task_artifacts_days() -> u32 {
    7
}
fn default_retention_sweep_interval_secs() -> u64 {
    86_400
}
impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            default_days: default_retention_default_days(),
            task_artifacts_days: default_retention_task_artifacts_days(),
            sweep_interval_secs: default_retention_sweep_interval_secs(),
        }
    }
}

fn default_env() -> String {
    "dev".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RedisConfig {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub token_expiry_hours: u64,
    /// ADR-0020 §`Mesh trust`: HS256 secret used to sign / verify the
    /// `X-Ork-Mesh-Token` header on outbound A2A calls. Falls back to
    /// [`Self::jwt_secret`] when unset for dev parity (single-instance
    /// deployments where one secret covers both bearer + mesh).
    #[serde(default)]
    pub mesh_secret: Option<String>,
    /// ADR-0020 §`Mesh trust`: `iss` claim stamped onto minted mesh tokens
    /// and required on inbound verification. Defaults to
    /// `"ork-mesh"`.
    #[serde(default = "default_mesh_iss")]
    pub mesh_issuer: String,
    /// ADR-0020 §`Mesh trust`: `aud` claim stamped / required. Defaults to
    /// `"ork-api"`. For multi-instance topologies operators set this to a
    /// destination-specific name and configure a matching peer-side
    /// signer; for single-instance default deployments the value is
    /// shared across all hops.
    #[serde(default = "default_mesh_aud")]
    pub mesh_audience: String,
}

fn default_mesh_iss() -> String {
    "ork-mesh".to_string()
}

fn default_mesh_aud() -> String {
    "ork-api".to_string()
}

/// ADR 0012 §`Decision`. Operator-side LLM provider catalog plus the global
/// default selector. Per-tenant overrides live in
/// [`crate::types::TenantId`]-keyed [`TenantSettings`](#) (`ork-core`) and are
/// merged by `ork_llm::router::LlmRouter` at resolve time.
///
/// Mirrors the `mcp_servers` shape (ADR 0010): a flat `Vec` whose `id` field
/// is the lookup key; a tenant entry with the same `id` replaces — never
/// merges with — an operator entry. Operators get one mental model.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct LlmConfig {
    /// Provider id used when [`crate::types::TenantId`] settings,
    /// [`crate::config::LlmConfig`]-equivalent agent overrides and the
    /// `ChatRequest.provider` field are all `None`. Must match the `id` of
    /// one of [`Self::providers`]; the router fails loud at boot otherwise.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Operator-defined provider catalog. Empty is allowed (the router will
    /// hand back a `LlmProvider not configured` error on the first call) so
    /// `ork-api` can boot a dev instance without any LLM endpoint wired up.
    #[serde(default)]
    pub providers: Vec<LlmProviderConfig>,
}

/// A single OpenAI-compatible endpoint entry. The wire client is
/// `ork_llm::openai_compatible::OpenAiCompatibleProvider`; this struct is
/// the operator/tenant on-disk shape.
///
/// `id` is the catalog key (case-sensitive), used by `ChatRequest.provider`,
/// `AgentConfig.provider`, `WorkflowStep.provider`, and the `default_provider`
/// pointer above.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LlmProviderConfig {
    pub id: String,
    pub base_url: String,
    /// Model used when neither the request, the agent, the workflow step,
    /// nor the tenant default model is set. Per-provider so operators can
    /// configure `openai` → `gpt-4o-mini` and `anthropic` → `claude-…`
    /// without repeating themselves on every agent.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Header map sent on every request to this provider. Names are
    /// case-preserved (deserialised into a `BTreeMap<String, _>`); values
    /// are an untagged enum so toml/yaml authors can mix
    /// `Authorization = { env = "OPENAI_API_KEY" }` with literal-valued
    /// helper headers.
    #[serde(default)]
    pub headers: BTreeMap<String, HeaderValueSource>,
    /// Optional capability declarations indexed by model name. Consumed by
    /// `LlmRouter::capabilities` and the agent loop's tool-call gating.
    #[serde(default)]
    pub capabilities: Vec<ModelCapabilitiesEntry>,
}

/// Value for one entry of [`LlmProviderConfig::headers`]. The two variants
/// have disjoint key sets (`env` vs. `value`), so `#[serde(untagged)]` is
/// the right fit — no synthetic `kind` field needed in the toml/yaml.
///
/// `Env` resolution is eager (at `LlmRouter` construction time) per the ADR
/// "fail loud at boot" rule; missing env vars surface as a configuration
/// error during `ork-api`/`ork-cli` startup, not on the first chat request.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum HeaderValueSource {
    Env { env: String },
    Value { value: String },
}

/// Per-model capability declaration; ADR 0012 §`Capability negotiation`.
/// Consumed by [`crate::ports::llm::ModelCapabilities`]-equivalent lookups in
/// `ork_llm::router::LlmRouter`. Anything not declared here defaults to
/// "unknown" and the agent loop conservatively assumes no tool calling.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ModelCapabilitiesEntry {
    pub model: String,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_vision: bool,
    /// Inclusive max input tokens. `None` ⇒ unknown.
    #[serde(default)]
    pub max_context: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkspaceConfig {
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    #[serde(default = "default_clone_depth")]
    pub clone_depth: u32,
}

fn default_cache_dir() -> String {
    "~/.ork/workspaces".into()
}

fn default_clone_depth() -> u32 {
    1
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            clone_depth: default_clone_depth(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RepositoryEntry {
    pub name: String,
    pub url: String,
    #[serde(default = "default_repo_branch")]
    pub default_branch: String,
}

fn default_repo_branch() -> String {
    "main".into()
}

/// Kafka client configuration (ADR-0004 hybrid transport, ADR-0020 §`Kafka trust`).
///
/// An empty `brokers` list is the dev-mode default and tells [`ork_eventing::build_client`]
/// to use the in-memory broadcast backend instead of attempting a real connection.
///
/// `transport` and `auth` carry the ADR-0020 security posture. The pre-ADR fields
/// `security_protocol` / `sasl_mechanism` are accepted for backwards-compat
/// deserialisation but no longer drive runtime behaviour — operators get a
/// log line at boot pointing them at the new keys.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct KafkaConfig {
    /// Bootstrap brokers (`host:port`). Empty = use in-memory backend (dev / tests).
    #[serde(default)]
    pub brokers: Vec<String>,

    /// Topic namespace prefix; matches [`ork_a2a::topics::DEFAULT_NAMESPACE`].
    #[serde(default = "default_kafka_namespace")]
    pub namespace: String,

    /// ADR-0020 §`Kafka trust`: transport-layer posture. Defaults to
    /// `Plaintext`. `RsKafkaBackend::connect` hard-errors when the runtime
    /// `env` is not `dev` and this resolves to `Plaintext`.
    #[serde(default)]
    pub transport: KafkaTransport,

    /// ADR-0020 §`Kafka trust`: SASL auth posture. Defaults to `None`
    /// (no SASL). `Oauthbearer` is the production target;
    /// `Scram` is the documented fallback.
    #[serde(default)]
    pub auth: KafkaAuth,

    /// Pre-ADR-0020 free-form security protocol (`PLAINTEXT`, `SASL_SSL`, ...).
    /// Accepted for backward-compat deserialisation only; runtime behaviour is
    /// driven by [`Self::transport`] + [`Self::auth`] now.
    #[serde(default)]
    pub security_protocol: Option<String>,

    /// Pre-ADR-0020 free-form SASL mechanism (`OAUTHBEARER`, `SCRAM-SHA-512`, ...).
    /// Accepted for backward-compat deserialisation only; runtime behaviour is
    /// driven by [`Self::auth`] now.
    #[serde(default)]
    pub sasl_mechanism: Option<String>,
}

/// ADR-0020 §`Kafka trust`: transport-layer posture for `RsKafkaBackend`.
///
/// `Plaintext` is allowed only when `ORK__ENV=dev`; `RsKafkaBackend::connect`
/// returns a [`crate::config`]-shaped error otherwise.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum KafkaTransport {
    #[default]
    Plaintext,
    /// TLS via `rustls`.
    ///
    /// - `ca_path` (optional): PEM bundle of trusted CA certs. **If unset,
    ///   no roots are loaded** and the broker handshake will fail with
    ///   "unknown issuer" — system / Mozilla roots are intentionally not
    ///   loaded so production posture is explicit. Operators wanting public
    ///   PKI must point this at a system bundle (e.g. `/etc/ssl/certs/ca-certificates.crt`).
    /// - `client_cert_path` + `client_key_path` (optional, both-or-neither):
    ///   PEM client cert + key for mTLS to the brokers.
    Tls {
        #[serde(default)]
        ca_path: Option<PathBuf>,
        #[serde(default)]
        client_cert_path: Option<PathBuf>,
        #[serde(default)]
        client_key_path: Option<PathBuf>,
    },
}

/// ADR-0020 §`Kafka trust`: SASL auth posture for `RsKafkaBackend`. Maps
/// 1:1 onto `rskafka::client::SaslConfig`. `*_env` fields name environment
/// variables that hold credentials; the literal value never lives in the
/// toml file (mirrors `[[remote_agents]]` auth shape).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KafkaAuth {
    #[default]
    None,
    /// SASL/SCRAM. `mechanism` selects SHA-256 vs SHA-512.
    Scram {
        username: String,
        password_env: String,
        #[serde(default)]
        mechanism: ScramMechanism,
    },
    /// SASL/OAUTHBEARER. The bearer token is read from `token_env` at
    /// connect time; rotation is the operator's responsibility today.
    Oauthbearer { token_env: String },
}

#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ScramMechanism {
    #[serde(rename = "sha-256", alias = "scram-sha-256", alias = "SHA-256")]
    Sha256,
    #[default]
    #[serde(rename = "sha-512", alias = "scram-sha-512", alias = "SHA-512")]
    Sha512,
}

fn default_kafka_namespace() -> String {
    "ork.a2a.v1".into()
}

impl Default for KafkaConfig {
    fn default() -> Self {
        Self {
            brokers: Vec::new(),
            namespace: default_kafka_namespace(),
            transport: KafkaTransport::default(),
            auth: KafkaAuth::default(),
            security_protocol: None,
            sasl_mechanism: None,
        }
    }
}

/// Per-agent discovery configuration (ADR-0005).
///
/// Every local agent publishes its [`ork_a2a::AgentCard`] to the discovery topic every
/// `interval_secs`; remote entries learned from the topic expire after
/// `ttl_multiplier * interval_secs` of silence. The remaining fields enrich the published
/// card so peers can dial back via Kong (`public_base_url`) and find the human-readable
/// catalog (`devportal_url`).
#[derive(Debug, Deserialize, Clone)]
pub struct DiscoveryConfig {
    #[serde(default = "default_discovery_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_ttl_multiplier")]
    pub ttl_multiplier: u32,
    /// Kong-fronted public base URL, e.g. `https://api.example.com/`. Used to build the
    /// per-agent `card.url`. Cards published without this field have `url = null` and are
    /// reachable only via the Kafka request topic.
    #[serde(default)]
    pub public_base_url: Option<Url>,
    /// Agent id served by `GET /.well-known/agent-card.json` (the bare default endpoint).
    /// Unset ⇒ the default endpoint returns 404; per-agent endpoints still work.
    #[serde(default)]
    pub default_agent_id: Option<String>,
    /// Operator organization placed in `card.provider.organization`. Both
    /// `provider_organization` and `devportal_url` must be set for `provider` to render.
    #[serde(default)]
    pub provider_organization: Option<String>,
    /// DevPortal home placed in `card.provider.url` (and used by the deferred sync ADR).
    #[serde(default)]
    pub devportal_url: Option<Url>,
    /// If true, every published card includes the ork `tenant-required` extension
    /// (ADR-0020 stub). Off by default in Phase 1.
    #[serde(default)]
    pub include_tenant_required_ext: bool,
}

fn default_discovery_interval_secs() -> u64 {
    30
}

fn default_ttl_multiplier() -> u32 {
    3
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_discovery_interval_secs(),
            ttl_multiplier: default_ttl_multiplier(),
            public_base_url: None,
            default_agent_id: None,
            provider_organization: None,
            devportal_url: None,
            include_tenant_required_ext: false,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "0.0.0.0".into(),
                port: 8080,
            },
            database: DatabaseConfig {
                url: "postgres://localhost/ork".into(),
                max_connections: 10,
            },
            redis: RedisConfig {
                url: "redis://127.0.0.1/".into(),
            },
            auth: AuthConfig {
                jwt_secret: "change-me-in-production".into(),
                token_expiry_hours: 24,
                mesh_secret: None,
                mesh_issuer: default_mesh_iss(),
                mesh_audience: default_mesh_aud(),
            },
            llm: LlmConfig::default(),
            workspace: WorkspaceConfig::default(),
            repositories: Vec::new(),
            kafka: KafkaConfig::default(),
            discovery: DiscoveryConfig::default(),
            remote_agents: Vec::new(),
            a2a_client: A2aClientToml::default(),
            env: default_env(),
            push: PushConfig::default(),
            mcp: McpAppConfig::default(),
            gateways: Vec::new(),
            artifacts: ArtifactsConfig::default(),
            retention: RetentionConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

/// ADR-0010 §`Configuration` — global MCP tool-plane knobs surfaced to
/// operators. Empty defaults: `enabled=true` flips on the wiring but
/// `servers` is empty, so installing the crate without writing any
/// `[[mcp.servers]]` entries is a no-op.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct McpAppConfig {
    /// Master switch. When false, `ork-api` skips `McpClient` boot
    /// entirely so a misconfigured `[mcp]` section can't take down the
    /// process. Defaults to true so the happy path doesn't need an
    /// explicit opt-in.
    #[serde(default = "default_mcp_enabled")]
    pub enabled: bool,
    /// `[[mcp.servers]]` list (ADR 0010 third source). Tenant-scoped
    /// settings still win on id collision; see
    /// `ork_mcp::McpConfigSources` for the precedence order.
    #[serde(default)]
    pub servers: Vec<crate::mcp_config::McpServerConfig>,
    /// How often `McpClient::refresh_all` runs. The cache TTL on
    /// descriptors is lazily aligned to this value at boot.
    #[serde(default = "default_mcp_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    /// Drop an idle MCP session after this many seconds. For stdio
    /// transports this also kills the child process, freeing memory and
    /// fds. ADR 0010 §`Negative / costs`.
    #[serde(default = "default_mcp_session_idle_ttl_secs")]
    pub session_idle_ttl_secs: u64,
}

fn default_mcp_enabled() -> bool {
    true
}
fn default_mcp_refresh_interval_secs() -> u64 {
    300
}
fn default_mcp_session_idle_ttl_secs() -> u64 {
    300
}

impl Default for McpAppConfig {
    fn default() -> Self {
        Self {
            enabled: default_mcp_enabled(),
            servers: Vec::new(),
            refresh_interval_secs: default_mcp_refresh_interval_secs(),
            session_idle_ttl_secs: default_mcp_session_idle_ttl_secs(),
        }
    }
}

impl McpAppConfig {
    pub fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.refresh_interval_secs.max(1))
    }
    pub fn session_idle_ttl(&self) -> Duration {
        Duration::from_secs(self.session_idle_ttl_secs.max(1))
    }
}

/// ADR-0009 push notifications: knobs surfaced to operators so the delivery
/// worker, signing-key rotation, and per-tenant cap can be tuned without a
/// recompile. Defaults match the ADR text verbatim.
#[derive(Debug, Deserialize, Clone)]
pub struct PushConfig {
    /// Hard cap on registered push configs per tenant. Enforced by
    /// `tasks/pushNotificationConfig/set` before the upsert.
    #[serde(default = "default_push_max_per_tenant")]
    pub max_per_tenant: u32,
    /// Per-attempt HTTP timeout for the delivery worker. Subscribers that
    /// don't respond inside this window are retried per `retry_schedule_minutes`.
    #[serde(default = "default_push_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Maximum concurrent in-flight POSTs across the worker pool.
    #[serde(default = "default_push_max_concurrency")]
    pub max_concurrency: usize,
    /// Retry schedule (in minutes) for failed deliveries. After the final
    /// retry the payload is written to `a2a_push_dead_letter`.
    #[serde(default = "default_push_retry_schedule")]
    pub retry_schedule_minutes: Vec<u64>,
    /// Days between automatic key rotations. Each new key is published
    /// immediately and used for new signatures right away.
    #[serde(default = "default_push_key_rotation_days")]
    pub key_rotation_days: u32,
    /// Days the previous signing key stays in JWKS after a rotation so
    /// subscribers caching by `kid` finish verifying in-flight requests.
    #[serde(default = "default_push_key_overlap_days")]
    pub key_overlap_days: u32,
    /// Days a push config row stays around after the task hits a terminal
    /// state. Driven by the janitor.
    #[serde(default = "default_push_config_retention_days")]
    pub config_retention_days: u32,
}

fn default_push_max_per_tenant() -> u32 {
    100
}
fn default_push_request_timeout_secs() -> u64 {
    10
}
fn default_push_max_concurrency() -> usize {
    32
}
fn default_push_retry_schedule() -> Vec<u64> {
    vec![1, 5, 30]
}
fn default_push_key_rotation_days() -> u32 {
    30
}
fn default_push_key_overlap_days() -> u32 {
    7
}
fn default_push_config_retention_days() -> u32 {
    14
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            max_per_tenant: default_push_max_per_tenant(),
            request_timeout_secs: default_push_request_timeout_secs(),
            max_concurrency: default_push_max_concurrency(),
            retry_schedule_minutes: default_push_retry_schedule(),
            key_rotation_days: default_push_key_rotation_days(),
            key_overlap_days: default_push_key_overlap_days(),
            config_retention_days: default_push_config_retention_days(),
        }
    }
}

/// Static `[[remote_agents]]` entry from `config/default.toml` (ADR-0007). The
/// `auth` payload is `serde(tag = "kind")` with one of the `A2aAuthToml`
/// variants below.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RemoteAgentEntryToml {
    pub id: String,
    pub card_url: Url,
    #[serde(default)]
    pub auth: A2aAuthToml,
}

/// Auth selector for a `[[remote_agents]]` entry. `*_env` fields name the
/// environment variable that holds the secret — the literal value never lives
/// in the toml file (mirrors ADR-0007 §"Auth").
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum A2aAuthToml {
    #[default]
    None,
    StaticBearer {
        value_env: String,
    },
    StaticApiKey {
        header: String,
        value_env: String,
    },
    #[serde(rename = "oauth2_client_credentials")]
    OAuth2ClientCredentials {
        token_url: Url,
        client_id_env: String,
        client_secret_env: String,
        #[serde(default)]
        scopes: Vec<String>,
    },
    Mtls {
        cert_path: PathBuf,
        key_path: PathBuf,
    },
}

/// Defaults for the A2A client (timeouts + card refresh cadence). Mirrors
/// `A2aClientConfig` in `ork-integrations` so operators see identical knobs in
/// toml and code.
#[derive(Debug, Deserialize, Clone)]
pub struct A2aClientToml {
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    #[serde(default = "default_card_refresh_interval_secs")]
    pub card_refresh_interval_secs: u64,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default)]
    pub retry: RetryPolicyToml,
}

fn default_request_timeout_secs() -> u64 {
    30
}

fn default_stream_idle_timeout_secs() -> u64 {
    300
}

fn default_card_refresh_interval_secs() -> u64 {
    3600
}

fn default_user_agent() -> String {
    format!("ork/{}", env!("CARGO_PKG_VERSION"))
}

impl Default for A2aClientToml {
    fn default() -> Self {
        Self {
            request_timeout_secs: default_request_timeout_secs(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
            card_refresh_interval_secs: default_card_refresh_interval_secs(),
            user_agent: default_user_agent(),
            retry: RetryPolicyToml::default(),
        }
    }
}

impl A2aClientToml {
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }
    pub fn stream_idle_timeout(&self) -> Duration {
        Duration::from_secs(self.stream_idle_timeout_secs)
    }
    pub fn card_refresh_interval(&self) -> Duration {
        Duration::from_secs(self.card_refresh_interval_secs)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RetryPolicyToml {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    #[serde(default = "default_factor")]
    pub factor: f32,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_max_attempts() -> u32 {
    3
}
fn default_initial_delay_ms() -> u64 {
    100
}
fn default_factor() -> f32 {
    2.0
}
fn default_max_delay_ms() -> u64 {
    5_000
}

impl Default for RetryPolicyToml {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_delay_ms: default_initial_delay_ms(),
            factor: default_factor(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

impl RetryPolicyToml {
    pub fn initial_delay(&self) -> Duration {
        Duration::from_millis(self.initial_delay_ms)
    }
    pub fn max_delay(&self) -> Duration {
        Duration::from_millis(self.max_delay_ms)
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, config::ConfigError> {
        let mut b = config::Config::builder()
            .add_source(config::File::with_name("config/default").required(false));
        if let Ok(extra) = std::env::var("ORK_CONFIG_EXTRA") {
            b = b.add_source(config::File::from(std::path::Path::new(&extra)).required(true));
        }
        let cfg = b
            .add_source(config::Environment::with_prefix("ORK").separator("__"))
            .build()?;

        cfg.try_deserialize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kafka_config_default_uses_in_memory() {
        let cfg = KafkaConfig::default();
        assert!(cfg.brokers.is_empty());
        assert_eq!(cfg.namespace, "ork.a2a.v1");
        assert!(matches!(cfg.transport, KafkaTransport::Plaintext));
        assert!(matches!(cfg.auth, KafkaAuth::None));
        assert!(cfg.security_protocol.is_none());
        assert!(cfg.sasl_mechanism.is_none());
    }

    /// ADR-0020 §`Kafka trust`: TLS section parses with all paths optional.
    #[test]
    fn kafka_transport_tls_section_parses() {
        let toml_src = r#"
            [kafka]
            brokers = ["broker1:9093"]
            [kafka.transport]
            kind = "tls"
            ca_path = "/etc/ork/ca.pem"
        "#;
        let parsed: KafkaSectionWrapper = toml::from_str(toml_src).expect("parse");
        match parsed.kafka.transport {
            KafkaTransport::Tls {
                ca_path,
                client_cert_path,
                client_key_path,
            } => {
                assert_eq!(
                    ca_path.as_deref(),
                    Some(std::path::Path::new("/etc/ork/ca.pem"))
                );
                assert!(client_cert_path.is_none());
                assert!(client_key_path.is_none());
            }
            other => panic!("expected tls, got {other:?}"),
        }
    }

    /// ADR-0020 §`Kafka trust`: OAUTHBEARER auth section.
    #[test]
    fn kafka_auth_oauthbearer_parses() {
        let toml_src = r#"
            [kafka]
            brokers = ["broker:9093"]
            [kafka.auth]
            kind = "oauthbearer"
            token_env = "ORK_KAFKA_TOKEN"
        "#;
        let parsed: KafkaSectionWrapper = toml::from_str(toml_src).expect("parse");
        match parsed.kafka.auth {
            KafkaAuth::Oauthbearer { token_env } => assert_eq!(token_env, "ORK_KAFKA_TOKEN"),
            other => panic!("expected oauthbearer, got {other:?}"),
        }
    }

    /// ADR-0020 §`Kafka trust`: SCRAM with default mechanism (SHA-512).
    #[test]
    fn kafka_auth_scram_defaults_to_sha512() {
        let toml_src = r#"
            [kafka]
            brokers = ["broker:9093"]
            [kafka.auth]
            kind = "scram"
            username = "ork-svc"
            password_env = "ORK_KAFKA_PASSWORD"
        "#;
        let parsed: KafkaSectionWrapper = toml::from_str(toml_src).expect("parse");
        match parsed.kafka.auth {
            KafkaAuth::Scram {
                username,
                password_env,
                mechanism,
            } => {
                assert_eq!(username, "ork-svc");
                assert_eq!(password_env, "ORK_KAFKA_PASSWORD");
                assert_eq!(mechanism, ScramMechanism::Sha512);
            }
            other => panic!("expected scram, got {other:?}"),
        }
    }

    /// Pre-ADR-0020 free-form `security_protocol` / `sasl_mechanism` keep
    /// parsing so an upgrade does not break a config file mid-flight; they
    /// just no longer drive runtime behaviour. The new structured
    /// `transport` / `auth` fields stay at their defaults so a refactor
    /// that re-introduced runtime coupling between the two would surface
    /// here.
    #[test]
    fn kafka_section_legacy_protocol_keys_still_parse() {
        let toml_src = r#"
            [kafka]
            brokers = ["broker:9093"]
            security_protocol = "SASL_SSL"
            sasl_mechanism = "SCRAM-SHA-512"
        "#;
        let parsed: KafkaSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.kafka.security_protocol.as_deref(), Some("SASL_SSL"));
        assert_eq!(
            parsed.kafka.sasl_mechanism.as_deref(),
            Some("SCRAM-SHA-512")
        );
        assert!(matches!(parsed.kafka.transport, KafkaTransport::Plaintext));
        assert!(matches!(parsed.kafka.auth, KafkaAuth::None));
    }

    /// ADR-0020 §`Secrets handling`: an unconfigured `[security.kms]`
    /// must default to the legacy adapter so existing dev deployments
    /// keep booting without configuration changes. Pinned to catch a
    /// future refactor that adds a new variant and forgets the
    /// `#[default]` attribute placement.
    #[test]
    fn kms_config_defaults_to_legacy() {
        assert!(matches!(SecurityConfig::default().kms, KmsConfig::Legacy));
        assert!(matches!(KmsConfig::default(), KmsConfig::Legacy));
    }

    #[test]
    fn kafka_section_parses_with_defaults() {
        let toml_src = r#"[kafka]"#;
        let parsed: KafkaSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert!(parsed.kafka.brokers.is_empty());
        assert_eq!(parsed.kafka.namespace, "ork.a2a.v1");
    }

    #[test]
    fn kafka_section_honours_overrides() {
        let toml_src = r#"
            [kafka]
            brokers = ["broker1:9092", "broker2:9092"]
            namespace = "ork.eu-west.v1"
            security_protocol = "SASL_SSL"
            sasl_mechanism = "OAUTHBEARER"
        "#;
        let parsed: KafkaSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.kafka.brokers, vec!["broker1:9092", "broker2:9092"]);
        assert_eq!(parsed.kafka.namespace, "ork.eu-west.v1");
        assert_eq!(parsed.kafka.security_protocol.as_deref(), Some("SASL_SSL"));
        assert_eq!(parsed.kafka.sasl_mechanism.as_deref(), Some("OAUTHBEARER"));
    }

    #[test]
    fn discovery_defaults_match_adr_0005() {
        let cfg = DiscoveryConfig::default();
        assert_eq!(cfg.interval_secs, 30);
        assert_eq!(cfg.ttl_multiplier, 3);
        assert!(cfg.public_base_url.is_none());
        assert!(cfg.default_agent_id.is_none());
        assert!(cfg.provider_organization.is_none());
        assert!(cfg.devportal_url.is_none());
        assert!(!cfg.include_tenant_required_ext);
    }

    #[test]
    fn discovery_section_parses_full_payload() {
        let toml_src = r#"
            [discovery]
            interval_secs = 15
            ttl_multiplier = 4
            public_base_url = "https://api.example.com/"
            default_agent_id = "planner"
            provider_organization = "Example Corp"
            devportal_url = "https://devportal.example.com/"
            include_tenant_required_ext = true
        "#;
        let parsed: DiscoverySectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.discovery.interval_secs, 15);
        assert_eq!(parsed.discovery.ttl_multiplier, 4);
        assert_eq!(
            parsed.discovery.public_base_url.as_ref().map(Url::as_str),
            Some("https://api.example.com/")
        );
        assert_eq!(
            parsed.discovery.default_agent_id.as_deref(),
            Some("planner")
        );
        assert_eq!(
            parsed.discovery.provider_organization.as_deref(),
            Some("Example Corp")
        );
        assert!(parsed.discovery.include_tenant_required_ext);
    }

    #[derive(Deserialize)]
    struct DiscoverySectionWrapper {
        #[serde(default)]
        discovery: DiscoveryConfig,
    }

    #[derive(Deserialize)]
    struct KafkaSectionWrapper {
        #[serde(default)]
        kafka: KafkaConfig,
    }

    #[test]
    fn remote_agents_section_parses_oauth_and_static_bearer() {
        let toml_src = r#"
            [[remote_agents]]
            id = "vendor-cc"
            card_url = "https://vendor.example.com/.well-known/agent-card.json"
            [remote_agents.auth]
            kind = "oauth2_client_credentials"
            token_url = "https://auth.vendor.example.com/oauth/token"
            client_id_env = "VENDOR_CLIENT_ID"
            client_secret_env = "VENDOR_CLIENT_SECRET"
            scopes = ["a2a.invoke"]

            [[remote_agents]]
            id = "vendor-bearer"
            card_url = "https://other.example.com/.well-known/agent-card.json"
            [remote_agents.auth]
            kind = "static_bearer"
            value_env = "OTHER_BEARER"
        "#;
        let parsed: RemoteAgentsWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.remote_agents.len(), 2);
        match &parsed.remote_agents[0].auth {
            A2aAuthToml::OAuth2ClientCredentials {
                client_id_env,
                scopes,
                ..
            } => {
                assert_eq!(client_id_env, "VENDOR_CLIENT_ID");
                assert_eq!(scopes, &vec!["a2a.invoke".to_string()]);
            }
            other => panic!("expected oauth2_client_credentials, got {other:?}"),
        }
        match &parsed.remote_agents[1].auth {
            A2aAuthToml::StaticBearer { value_env } => assert_eq!(value_env, "OTHER_BEARER"),
            other => panic!("expected static_bearer, got {other:?}"),
        }
    }

    #[test]
    fn a2a_client_section_uses_defaults_when_absent() {
        let toml_src = r#"[a2a_client]"#;
        let parsed: A2aClientWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.a2a_client.request_timeout_secs, 30);
        assert_eq!(parsed.a2a_client.card_refresh_interval_secs, 3600);
        assert_eq!(parsed.a2a_client.retry.max_attempts, 3);
    }

    #[derive(Deserialize)]
    struct RemoteAgentsWrapper {
        #[serde(default)]
        remote_agents: Vec<RemoteAgentEntryToml>,
    }

    #[derive(Deserialize)]
    struct A2aClientWrapper {
        #[serde(default)]
        a2a_client: A2aClientToml,
    }

    #[test]
    fn push_config_defaults_match_adr_0009() {
        let cfg = PushConfig::default();
        assert_eq!(cfg.max_per_tenant, 100);
        assert_eq!(cfg.request_timeout_secs, 10);
        assert_eq!(cfg.max_concurrency, 32);
        assert_eq!(cfg.retry_schedule_minutes, vec![1, 5, 30]);
        assert_eq!(cfg.key_rotation_days, 30);
        assert_eq!(cfg.key_overlap_days, 7);
        assert_eq!(cfg.config_retention_days, 14);
    }

    #[derive(Deserialize)]
    struct PushSectionWrapper {
        #[serde(default)]
        push: PushConfig,
    }

    #[test]
    fn push_section_uses_defaults_when_absent() {
        let toml_src = r#"[push]"#;
        let parsed: PushSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.push.max_per_tenant, 100);
        assert_eq!(parsed.push.retry_schedule_minutes, vec![1, 5, 30]);
    }

    #[test]
    fn push_section_honours_overrides() {
        let toml_src = r#"
            [push]
            max_per_tenant = 25
            request_timeout_secs = 20
            retry_schedule_minutes = [2, 10, 60]
            key_rotation_days = 45
            key_overlap_days = 14
        "#;
        let parsed: PushSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.push.max_per_tenant, 25);
        assert_eq!(parsed.push.request_timeout_secs, 20);
        assert_eq!(parsed.push.retry_schedule_minutes, vec![2, 10, 60]);
        assert_eq!(parsed.push.key_rotation_days, 45);
        assert_eq!(parsed.push.key_overlap_days, 14);
    }

    #[test]
    fn env_defaults_to_dev() {
        assert_eq!(AppConfig::default().env, "dev");
    }

    #[test]
    fn gateways_default_empty() {
        assert!(AppConfig::default().gateways.is_empty());
    }

    #[test]
    fn gateways_section_parses_rest_entry() {
        let toml_src = r#"
            [[gateways]]
            id = "rest-local"
            type = "rest"
            enabled = true
            [gateways.config]
            default_agent = "planner"
            tenant_id = "00000000-0000-0000-0000-000000000000"
        "#;
        let parsed: GatewaysOnly = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.gateways.len(), 1);
        let g = &parsed.gateways[0];
        assert_eq!(g.id, "rest-local");
        assert_eq!(g.gateway_type, "rest");
        assert!(g.enabled);
        assert_eq!(g.config["default_agent"], "planner");
    }

    #[test]
    fn gateway_enabled_defaults_true_when_omitted() {
        let toml_src = r#"
            [[gateways]]
            id = "x"
            type = "mcp"
        "#;
        let parsed: GatewaysOnly = toml::from_str(toml_src).expect("parse");
        assert!(parsed.gateways[0].enabled);
    }

    #[derive(Deserialize)]
    struct GatewaysOnly {
        #[serde(default)]
        gateways: Vec<GatewayConfig>,
    }

    #[derive(Deserialize)]
    struct McpSectionWrapper {
        #[serde(default)]
        mcp: McpAppConfig,
    }

    #[test]
    fn mcp_section_defaults_match_adr_0010() {
        // No `[mcp]` block at all must yield the ADR-stated defaults.
        let toml_src = r#""#;
        let parsed: McpSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert!(parsed.mcp.enabled, "MCP wiring must default to enabled");
        assert!(parsed.mcp.servers.is_empty());
        assert_eq!(parsed.mcp.refresh_interval_secs, 300);
        assert_eq!(parsed.mcp.session_idle_ttl_secs, 300);
    }

    #[test]
    fn mcp_section_present_but_empty_uses_defaults() {
        let toml_src = r#"[mcp]"#;
        let parsed: McpSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert!(parsed.mcp.enabled);
        assert!(parsed.mcp.servers.is_empty());
    }

    #[test]
    fn mcp_section_parses_streamable_http_oauth_server() {
        // Mirror of the ADR-0010 YAML example, expressed in toml so it
        // exercises the same `tag = "type"` serde path the runtime uses.
        let toml_src = r#"
            [mcp]
            enabled = true
            refresh_interval_secs = 60
            session_idle_ttl_secs = 120

            [[mcp.servers]]
            id = "atlassian"
            [mcp.servers.transport]
            type = "streamable_http"
            url = "https://mcp-atlassian.example.com/"
            [mcp.servers.transport.auth]
            type = "oauth2_client_credentials"
            token_url = "https://auth.example.com/oauth/token"
            client_id_env = "ATLASSIAN_MCP_CLIENT_ID"
            client_secret_env = "ATLASSIAN_MCP_SECRET"
            scopes = ["read:jira"]
        "#;
        let parsed: McpSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert!(parsed.mcp.enabled);
        assert_eq!(parsed.mcp.refresh_interval_secs, 60);
        assert_eq!(parsed.mcp.session_idle_ttl_secs, 120);
        assert_eq!(parsed.mcp.servers.len(), 1);
        let srv = &parsed.mcp.servers[0];
        assert_eq!(srv.id, "atlassian");
        match &srv.transport {
            crate::mcp_config::McpTransportConfig::StreamableHttp { url, auth } => {
                assert_eq!(url.as_str(), "https://mcp-atlassian.example.com/");
                match auth {
                    crate::mcp_config::McpAuthConfig::Oauth2ClientCredentials {
                        client_id_env,
                        scopes,
                        ..
                    } => {
                        assert_eq!(client_id_env, "ATLASSIAN_MCP_CLIENT_ID");
                        assert_eq!(scopes, &vec!["read:jira".to_string()]);
                    }
                    other => panic!("expected oauth2_client_credentials, got {other:?}"),
                }
            }
            other => panic!("expected streamable_http, got {other:?}"),
        }
    }

    #[test]
    fn mcp_section_parses_stdio_server() {
        let toml_src = r#"
            [[mcp.servers]]
            id = "local-fs"
            [mcp.servers.transport]
            type = "stdio"
            command = "mcp-fs"
            args = ["--root", "/tenants/a/files"]
            [mcp.servers.transport.env]
            FS_TOKEN = "from-env"
        "#;
        let parsed: McpSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.mcp.servers.len(), 1);
        match &parsed.mcp.servers[0].transport {
            crate::mcp_config::McpTransportConfig::Stdio { command, args, env } => {
                assert_eq!(command, "mcp-fs");
                assert_eq!(
                    args,
                    &vec!["--root".to_string(), "/tenants/a/files".to_string()]
                );
                assert_eq!(env.get("FS_TOKEN"), Some(&"from-env".to_string()));
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[derive(Deserialize)]
    struct LlmSectionWrapper {
        #[serde(default)]
        llm: LlmConfig,
    }

    #[test]
    fn llm_section_defaults_to_empty_catalog() {
        let parsed: LlmSectionWrapper = toml::from_str(r#""#).expect("parse");
        assert!(parsed.llm.default_provider.is_none());
        assert!(parsed.llm.providers.is_empty());
    }

    #[test]
    fn llm_provider_parses_env_and_literal_headers() {
        // Mirrors the catalog example in ADR 0012 §`Decision`. The two
        // header variants (`env`, `value`) are untagged because their keys
        // are disjoint — no `kind` discriminator needed.
        let toml_src = r#"
            [llm]
            default_provider = "openai"

            [[llm.providers]]
            id = "openai"
            base_url = "https://api.openai.com/v1"
            default_model = "gpt-4o-mini"

            [llm.providers.headers]
            Authorization = { env = "OPENAI_API_KEY" }
            X-Trace-Tag = { value = "ork-edge" }

            [[llm.providers.capabilities]]
            model = "gpt-4o-mini"
            supports_tools = true
            supports_streaming = true
            max_context = 128000
        "#;
        let parsed: LlmSectionWrapper = toml::from_str(toml_src).expect("parse");
        assert_eq!(parsed.llm.default_provider.as_deref(), Some("openai"));
        assert_eq!(parsed.llm.providers.len(), 1);
        let p = &parsed.llm.providers[0];
        assert_eq!(p.id, "openai");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.default_model.as_deref(), Some("gpt-4o-mini"));

        // Header keys are case-preserved (BTreeMap with String key, not
        // a HeaderName-based alphabetical lowercase hammer).
        let auth = p.headers.get("Authorization").expect("Authorization key");
        match auth {
            HeaderValueSource::Env { env } => assert_eq!(env, "OPENAI_API_KEY"),
            other => panic!("expected env-form, got {other:?}"),
        }
        let tag = p.headers.get("X-Trace-Tag").expect("trace tag header");
        match tag {
            HeaderValueSource::Value { value } => assert_eq!(value, "ork-edge"),
            other => panic!("expected literal value, got {other:?}"),
        }

        assert_eq!(p.capabilities.len(), 1);
        let c = &p.capabilities[0];
        assert_eq!(c.model, "gpt-4o-mini");
        assert!(c.supports_tools);
        assert!(c.supports_streaming);
        assert!(!c.supports_vision);
        assert_eq!(c.max_context, Some(128_000));
    }

    #[test]
    fn llm_provider_capabilities_are_optional() {
        // `[[llm.providers.capabilities]]` is optional; when absent the
        // router falls through to the conservative "unknown caps" path.
        let toml_src = r#"
            [[llm.providers]]
            id = "anthropic"
            base_url = "https://api.anthropic.com/v1"
            [llm.providers.headers]
            x-api-key = { env = "ANTHROPIC_API_KEY" }
        "#;
        let parsed: LlmSectionWrapper = toml::from_str(toml_src).expect("parse");
        let p = &parsed.llm.providers[0];
        assert!(p.capabilities.is_empty());
        assert!(p.default_model.is_none());
        // Header names that look like raw HTTP headers stay verbatim.
        assert!(p.headers.contains_key("x-api-key"));
    }

    #[test]
    fn mcp_app_config_helpers_clamp_zero_to_one_second() {
        // Defensive: a misconfigured zero on either knob would otherwise
        // turn `tokio::time::interval` into a tight CPU loop. The
        // helpers floor at 1 second so the runtime stays sane even if
        // the toml is hostile.
        let cfg = McpAppConfig {
            enabled: true,
            servers: Vec::new(),
            refresh_interval_secs: 0,
            session_idle_ttl_secs: 0,
        };
        assert_eq!(cfg.refresh_interval(), Duration::from_secs(1));
        assert_eq!(cfg.session_idle_ttl(), Duration::from_secs(1));
    }
}
