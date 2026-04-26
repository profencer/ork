//! Shared router state (ADR-0017).

use std::sync::Arc;

use ork_common::config::GatewayConfig;
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::ports::webui_store::WebuiStore;
use ork_gateways::bootstrap::GatewayBootstrapDeps;

use crate::in_memory_store::InMemoryWebuiStore;

/// Injected into `/webui/api/*` handlers.
#[derive(Clone)]
pub struct WebUiState {
    pub agent_registry: Arc<AgentRegistry>,
    pub webui: Arc<dyn WebuiStore>,
    /// Forwards to `POST {a2a_public_base}/a2a/agents/{agent_id}` (same JSON-RPC as ADR-0008).
    pub a2a_public_base: String,
    pub http: reqwest::Client,
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    pub artifact_public_base: String,
    pub max_upload_bytes: u64,
}

impl WebUiState {
    /// Build from gateway bootstrap; uses in-memory `WebuiStore` when the API did not pass one.
    pub fn from_bootstrap(deps: &GatewayBootstrapDeps, cfg: &GatewayConfig) -> Self {
        let webui: Arc<dyn WebuiStore> = deps
            .webui_store
            .clone()
            .unwrap_or_else(|| Arc::new(InMemoryWebuiStore::new()));
        let a2a_public_base = std::env::var("ORK_A2A_PUBLIC_BASE")
            .ok()
            .or_else(|| {
                cfg.config
                    .get("a2a_public_base")
                    .and_then(|v| v.as_str().map(String::from))
            })
            .unwrap_or_default();
        let max_upload_bytes = cfg
            .config
            .get("max_upload_bytes")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                std::env::var("WEBUI_UPLOAD_MAX_BYTES")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(25 * 1024 * 1024);
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .expect("webui: reqwest client");
        Self {
            agent_registry: deps.core.agent_registry.clone(),
            webui,
            a2a_public_base,
            http,
            artifact_store: deps.core.artifact_store.clone(),
            artifact_public_base: deps.core.artifact_public_base.clone().unwrap_or_default(),
            max_upload_bytes,
        }
    }
}

impl WebUiState {
    /// For tests: minimal state (in-memory `WebuiStore`, no A2A forward).
    #[must_use]
    pub fn test_stub() -> Self {
        Self {
            agent_registry: Arc::new(ork_core::agent_registry::AgentRegistry::new()),
            webui: Arc::new(InMemoryWebuiStore::new()),
            a2a_public_base: String::new(),
            http: reqwest::Client::new(),
            artifact_store: None,
            artifact_public_base: String::new(),
            max_upload_bytes: 1024 * 1024,
        }
    }
}
