//! [`OrkAppBuilder`](OrkAppBuilder): central registry wiring (ADR [`0049`](../../docs/adrs/0049-orkapp-central-registry.md)).

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use ork_common::error::OrkError;
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
use ork_tool::IntoToolDef;
use ork_workflow::ProgramOp;
use serde_json::Value;

use crate::app::OrkApp;
use crate::id::validate_id;
use crate::inner::OrkAppInner;
use crate::ports::server::Server;
use crate::types::{
    Environment, McpServerSpec, ObservabilityConfig, ScorerBinding, ScorerSpec, ScorerTarget,
    ServerConfig,
};

fn cfg(message: impl Into<String>) -> OrkError {
    OrkError::Configuration {
        message: message.into(),
    }
}

/// Builder that registers deployment components prior to [`OrkApp`] construction.
#[derive(Clone, Default)]
pub struct OrkAppBuilder {
    agents: Vec<Arc<dyn Agent>>,
    workflows: Vec<Arc<dyn WorkflowDef>>,
    code_first_programs: HashMap<String, Arc<Vec<ProgramOp>>>,
    snapshot_store: Option<Arc<dyn WorkflowSnapshotStore>>,
    tools: Vec<Arc<dyn ToolDef>>,
    mcp_servers: Vec<(String, McpServerSpec)>,
    memory: Option<Arc<dyn MemoryStore>>,
    storage: Option<Arc<dyn KvStorage>>,
    vectors: Option<Arc<dyn VectorStore>>,
    scorers: Vec<ScorerBinding>,
    observability: Option<ObservabilityConfig>,
    server_config: ServerConfig,
    /// HTTP stack injected by adapters (crate `ork-server`); [`OrkApp::serve`] requires this.
    serve_backend: Option<Arc<dyn Server>>,
    request_context_schema: Option<Value>,
    id_generator: Option<Arc<dyn IdGenerator>>,
    /// ADR-0020: tenant under which background-fired runs (cron, replay) execute.
    /// Required when any registered workflow has a cron trigger.
    system_tenant_id: Option<TenantId>,
    environment: Environment,
}

impl OrkAppBuilder {
    /// Registers a typed agent satisfying the existing [`Agent`](ork_core::ports::agent::Agent) port.
    pub fn agent<A: Agent + 'static>(mut self, a: A) -> Self {
        self.agents.push(Arc::new(a));
        self
    }

    /// Registers a code-first workflow (ADR [`0050`](../../docs/adrs/0050-code-first-workflow-dsl.md)).
    pub fn workflow<W: WorkflowDef + Any + 'static>(mut self, w: W) -> Self {
        if let Some(cf) = (&w as &dyn Any).downcast_ref::<ork_workflow::Workflow>() {
            self.code_first_programs
                .insert(cf.id().to_string(), cf.program_arc());
        }
        self.workflows.push(Arc::new(w));
        self
    }

    /// Optional store for suspend/resume snapshots (ADR-0050).
    pub fn snapshot_store(mut self, store: Arc<dyn WorkflowSnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    pub fn tool(mut self, t: impl IntoToolDef) -> Self {
        self.tools.push(t.into_tool_def());
        self
    }

    /// Register a type-erased tool (e.g. from a shared [`Arc`](std::sync::Arc)).
    pub fn tool_dyn(mut self, t: Arc<dyn ToolDef>) -> Self {
        self.tools.push(t);
        self
    }

    pub fn mcp_server(mut self, id: impl Into<String>, spec: McpServerSpec) -> Self {
        self.mcp_servers.push((id.into(), spec));
        self
    }

    pub fn memory<M: MemoryStore + 'static>(mut self, m: M) -> Self {
        self.memory = Some(Arc::new(m));
        self
    }

    pub fn storage<S: KvStorage + 'static>(mut self, s: S) -> Self {
        self.storage = Some(Arc::new(s));
        self
    }

    pub fn vectors<V: VectorStore + 'static>(mut self, v: V) -> Self {
        self.vectors = Some(Arc::new(v));
        self
    }

    pub fn scorer(mut self, target: ScorerTarget, scorer: ScorerSpec) -> Self {
        self.scorers.push(ScorerBinding { target, scorer });
        self
    }

    pub fn observability(mut self, obs: ObservabilityConfig) -> Self {
        self.observability = Some(obs);
        self
    }

    pub fn server(mut self, server_cfg: ServerConfig) -> Self {
        self.server_config = server_cfg;
        self
    }

    /// Wires an HTTP backend (e.g. `ork_server::AxumServer`) — required before [`crate::OrkApp::serve`].
    pub fn serve_backend(mut self, backend: Arc<dyn Server>) -> Self {
        self.serve_backend = Some(backend);
        self
    }

    pub fn request_context_schema(mut self, schema: Value) -> Self {
        self.request_context_schema = Some(schema);
        self
    }

    pub fn id_generator<G: IdGenerator + 'static>(mut self, g: G) -> Self {
        self.id_generator = Some(Arc::new(g));
        self
    }

    pub fn environment(mut self, env: Environment) -> Self {
        self.environment = env;
        self
    }

    /// ADR-0020: tenant under which background-fired runs (cron, replay) execute.
    /// Mandatory when any registered workflow has a cron trigger; otherwise a
    /// scheduled run would have no real tenant id and would fail RLS / FK
    /// constraints once `TenantTxScope` is wired in.
    #[must_use]
    pub fn system_tenant(mut self, tenant: TenantId) -> Self {
        self.system_tenant_id = Some(tenant);
        self
    }

    /// Consume the builder after validating ids, duplicates, and workflow references.
    pub fn build(self) -> Result<OrkApp, OrkError> {
        validate_id_components(&self)?;

        let mut agents: HashMap<String, Arc<dyn Agent>> = HashMap::new();
        for agent in self.agents {
            let aid = agent.id().as_str().to_string();
            // ADR-0053: inject the registered MemoryStore (if any) into
            // every agent before sealing the registry. Each agent's
            // `inject_memory` is idempotent and the per-agent
            // `.memory(...)` override wins, so this is safe regardless
            // of authoring path.
            if let Some(memory) = self.memory.as_ref() {
                agent.inject_memory(memory.clone());
            }
            if agents.insert(aid.clone(), agent).is_some() {
                return Err(cfg(format!(
                    "duplicate agent id `{aid}`; ids must be unique within the agents category"
                )));
            }
        }

        let mut tools: HashMap<String, Arc<dyn ToolDef>> = HashMap::new();
        for tool in self.tools {
            let tid = tool.id().to_string();
            if tools.insert(tid.clone(), tool).is_some() {
                return Err(cfg(format!(
                    "duplicate tool id `{tid}`; ids must be unique within the tools category"
                )));
            }
        }

        let mut workflows: HashMap<String, Arc<dyn WorkflowDef>> = HashMap::new();
        for workflow in self.workflows {
            let wid = workflow.id().to_string();
            if workflows.insert(wid.clone(), workflow).is_some() {
                return Err(cfg(format!(
                    "duplicate workflow id `{wid}`; ids must be unique within the workflows category"
                )));
            }
        }

        let mut seen_mcp: HashSet<String> = HashSet::new();
        let mut mcp_servers = Vec::<(String, McpServerSpec)>::with_capacity(self.mcp_servers.len());
        for (mid, spec) in self.mcp_servers {
            validate_id(&mid)?;
            if !seen_mcp.insert(mid.clone()) {
                return Err(cfg(format!(
                    "duplicate MCP server id `{mid}`; ids must be unique within mcp_servers"
                )));
            }
            mcp_servers.push((mid, spec));
        }

        if self.system_tenant_id.is_none()
            && let Some(wf) = workflows.values().find(|w| w.cron_trigger().is_some())
        {
            return Err(cfg(format!(
                "workflow `{}` declares a cron trigger but no `system_tenant` is configured on the OrkAppBuilder; \
                 background-fired runs need a real tenant id (ADR-0020)",
                wf.id()
            )));
        }

        for wf in workflows.values() {
            for tid in wf.referenced_tool_ids() {
                if !tools.contains_key(tid) {
                    return Err(cfg(format!(
                        "workflow `{}` references tool `{tid}` which is not registered on this builder",
                        wf.id()
                    )));
                }
            }
            for aid in wf.referenced_agent_ids() {
                if !agents.contains_key(aid) {
                    return Err(cfg(format!(
                        "workflow `{}` references agent `{aid}` which is not registered on this builder",
                        wf.id()
                    )));
                }
            }
        }

        // ADR-0052: agents may reference peer agents (`agent_as_tool`),
        // workflows (`workflow_as_tool`), and MCP servers (`tool_server`).
        // Validate each ref resolves on the same builder; order is irrelevant
        // (forward refs are allowed because validation happens once after all
        // registrations).
        let mcp_ids: HashSet<&str> = mcp_servers.iter().map(|(id, _)| id.as_str()).collect();
        for agent in agents.values() {
            let aid = agent.id().as_str();
            for peer in agent.referenced_agent_ids() {
                if !agents.contains_key(peer.as_str()) {
                    return Err(cfg(format!(
                        "agent `{aid}` references agent `{peer}` which is not registered on this builder"
                    )));
                }
                if peer == aid {
                    return Err(cfg(format!(
                        "agent `{aid}` references itself via agent_as_tool; self-delegation cycles are rejected at build time"
                    )));
                }
            }
            for wid in agent.referenced_workflow_ids() {
                if !workflows.contains_key(wid.as_str()) {
                    return Err(cfg(format!(
                        "agent `{aid}` references workflow `{wid}` which is not registered on this builder"
                    )));
                }
            }
            for sid in agent.referenced_mcp_server_ids() {
                if !mcp_ids.contains(sid.as_str()) {
                    return Err(cfg(format!(
                        "agent `{aid}` references MCP server `{sid}` which is not registered on this builder"
                    )));
                }
            }
        }

        for binding in &self.scorers {
            let target_id = match &binding.target {
                ScorerTarget::Agent { id } | ScorerTarget::Workflow { id } => id,
            };
            validate_id(target_id)?;
            match &binding.target {
                ScorerTarget::Agent { id } => {
                    if !agents.contains_key(id) {
                        return Err(cfg(format!("scorer binds to unknown agent `{id}`")));
                    }
                }
                ScorerTarget::Workflow { id } => {
                    if !workflows.contains_key(id) {
                        return Err(cfg(format!("scorer binds to unknown workflow `{id}`")));
                    }
                }
            }
            validate_id(&binding.scorer.id)?;
        }

        let built_at = Utc::now();
        let agent_registry = Arc::new(AgentRegistry::from_agents(agents.values().cloned()));
        let inner = Arc::new(OrkAppInner {
            agents,
            agent_registry,
            workflows,
            code_first_programs: self.code_first_programs,
            snapshot_store: self.snapshot_store,
            tools,
            mcp_servers,
            memory: self.memory,
            storage: self.storage,
            vectors: self.vectors,
            scorers: self.scorers,
            observability: self.observability,
            server_config: self.server_config,
            request_context_schema: self.request_context_schema,
            id_generator: self.id_generator,
            environment: self.environment,
            system_tenant_id: self.system_tenant_id,
            built_at,
            ork_version: env!("CARGO_PKG_VERSION").into(),
        });

        Ok(OrkApp {
            inner,
            serve_backend: self.serve_backend,
        })
    }
}

fn validate_id_components(b: &OrkAppBuilder) -> Result<(), OrkError> {
    for a in &b.agents {
        validate_id(a.id().as_str())?;
    }
    for w in &b.workflows {
        validate_id(w.id())?;
        for tid in w.referenced_tool_ids() {
            validate_id(tid)?;
        }
        for aid in w.referenced_agent_ids() {
            validate_id(aid)?;
        }
    }
    for t in &b.tools {
        validate_id(t.id())?;
    }
    Ok(())
}
