//! Frozen application state built by [`crate::OrkAppBuilder`](super::builder::OrkAppBuilder).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ork_core::ports::agent::Agent;
use ork_core::ports::id_generator::IdGenerator;
use ork_core::ports::kv_storage::KvStorage;
use ork_core::ports::memory_store::MemoryStore;
use ork_core::ports::tool_def::ToolDef;
use ork_core::ports::vector_store::VectorStore;
use ork_core::ports::workflow_def::WorkflowDef;
use serde_json::Value;

use crate::types::{Environment, McpServerSpec, ObservabilityConfig, ScorerBinding, ServerConfig};

pub struct OrkAppInner {
    pub(crate) agents: HashMap<String, Arc<dyn Agent>>,
    pub(crate) workflows: HashMap<String, Arc<dyn WorkflowDef>>,
    pub(crate) tools: HashMap<String, Arc<dyn ToolDef>>,
    pub(crate) mcp_servers: Vec<(String, McpServerSpec)>,
    pub(crate) memory: Option<Arc<dyn MemoryStore>>,
    pub(crate) storage: Option<Arc<dyn KvStorage>>,
    pub(crate) vectors: Option<Arc<dyn VectorStore>>,
    pub(crate) scorers: Vec<ScorerBinding>,
    pub(crate) observability: Option<ObservabilityConfig>,
    pub(crate) server_config: ServerConfig,
    pub(crate) request_context_schema: Option<Value>,
    pub(crate) id_generator: Option<Arc<dyn IdGenerator>>,
    pub(crate) environment: Environment,
    pub(crate) built_at: DateTime<Utc>,
    pub(crate) ork_version: String,
}

impl OrkAppInner {
    #[must_use]
    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }

    #[must_use]
    pub fn environment(&self) -> &Environment {
        &self.environment
    }
}
