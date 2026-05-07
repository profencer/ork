//! Frozen application state built by [`crate::OrkAppBuilder`](super::builder::OrkAppBuilder).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ork_common::types::TenantId;
use ork_core::agent_registry::AgentRegistry;
use ork_core::ports::agent::Agent;
use ork_core::ports::id_generator::IdGenerator;
use ork_core::ports::kv_storage::KvStorage;
use ork_core::ports::memory_store::MemoryStore;
use ork_core::ports::tool_def::ToolDef;
use ork_core::ports::vector_store::VectorStore;
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_snapshot::WorkflowSnapshotStore;
use ork_workflow::ProgramOp;
use serde_json::Value;

use crate::types::{Environment, McpServerSpec, ObservabilityConfig, ScorerBinding, ServerConfig};

pub struct OrkAppInner {
    pub(crate) agents: HashMap<String, Arc<dyn Agent>>,
    pub(crate) agent_registry: Arc<AgentRegistry>,
    pub(crate) workflows: HashMap<String, Arc<dyn WorkflowDef>>,
    /// ADR-0050: compiled graph per workflow id (only [`ork_workflow::Workflow`] keys are present).
    pub(crate) code_first_programs: HashMap<String, Arc<Vec<ProgramOp>>>,
    pub(crate) snapshot_store: Option<Arc<dyn WorkflowSnapshotStore>>,
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
    /// ADR-0020 §`Tenant id propagation`: tenant under which background-fired
    /// runs (cron triggers, replays) execute. Required when any registered
    /// workflow declares a cron trigger; absent installs without scheduled
    /// workflows leave it `None` and never need to fabricate a tenant id.
    pub(crate) system_tenant_id: Option<TenantId>,
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
