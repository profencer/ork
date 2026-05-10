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

use ork_eval::live::{LiveSamplerHandle, ScorerResultSink};
use ork_eval::metrics::ScorerMetrics;

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
    /// ADR-0054: producer side of the live-sampling worker, populated
    /// by `OrkAppBuilder::build()` when at least one `Live`/`Both`
    /// binding fires on a registered agent. `None` means no live
    /// sampling is active for this deployment.
    pub(crate) live_sampler: Option<LiveSamplerHandle>,
    /// ADR-0054: scorer metrics handle (`scorer_dropped_total` and
    /// friends). Always present so consumers can register the
    /// counters in their global registry.
    pub(crate) scorer_metrics: Arc<ScorerMetrics>,
    /// ADR-0054 reviewer M1, deferred: durable sink for
    /// `scorer_results`. v1 defaults to an in-memory sink so the
    /// worker is observable end-to-end; the Postgres-backed sink
    /// lands as a follow-up driven by ADR-0058.
    pub(crate) scorer_sink: Arc<dyn ScorerResultSink>,
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
