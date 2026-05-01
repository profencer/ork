//! [`OrkApp`]: central registry value (ADR [`0049`](../../docs/adrs/0049-orkapp-central-registry.md)).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, AgentMessage, CallerIdentity, TaskId};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_run::WorkflowRunDeps;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::inner::OrkAppInner;
use crate::manifest::{self, AppManifest};
use crate::ports::server::{ServeHandle, Server};

pub use ork_core::ports::workflow_run::WorkflowRunHandle;

/// A2A-shaped user message (see [`Agent::send_stream`]).
pub type ChatMessage = AgentMessage;

/// Central registry for an ork deployment.
#[derive(Clone)]
pub struct OrkApp {
    pub(crate) inner: Arc<OrkAppInner>,
    pub(crate) serve_backend: Option<Arc<dyn Server>>,
}

impl OrkApp {
    pub fn agent(&self, id: &str) -> Option<Arc<dyn Agent>> {
        self.inner.agents.get(id).cloned()
    }

    pub fn workflow(&self, id: &str) -> Option<Arc<dyn WorkflowDef>> {
        self.inner.workflows.get(id).cloned()
    }

    pub fn tool(&self, id: &str) -> Option<Arc<dyn ork_core::ports::tool_def::ToolDef>> {
        self.inner.tools.get(id).cloned()
    }

    pub fn agents(&self) -> impl Iterator<Item = (&str, &Arc<dyn Agent>)> {
        self.inner.agents.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn workflows(&self) -> impl Iterator<Item = (&str, &Arc<dyn WorkflowDef>)> {
        self.inner.workflows.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn tools(
        &self,
    ) -> impl Iterator<Item = (&str, &Arc<dyn ork_core::ports::tool_def::ToolDef>)> {
        self.inner.tools.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn memory(&self) -> Option<&Arc<dyn ork_core::ports::memory_store::MemoryStore>> {
        self.inner.memory.as_ref()
    }

    pub fn storage(&self) -> Option<&Arc<dyn ork_core::ports::kv_storage::KvStorage>> {
        self.inner.storage.as_ref()
    }

    pub fn vectors(&self) -> Option<&Arc<dyn ork_core::ports::vector_store::VectorStore>> {
        self.inner.vectors.as_ref()
    }

    pub fn id_generator(&self) -> Option<&Arc<dyn ork_core::ports::id_generator::IdGenerator>> {
        self.inner.id_generator.as_ref()
    }

    pub fn agent_cards(&self) -> impl Iterator<Item = &ork_core::a2a::AgentCard> + '_ {
        let mut ids: Vec<_> = self.inner.agents.keys().cloned().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(move |id| self.inner.agents.get(&id).map(|a| a.card()))
    }

    pub fn manifest(&self) -> AppManifest {
        manifest::build_manifest(&self.inner)
    }

    /// Per-run dependencies for ADR-0050 code-first workflows.
    #[must_use]
    pub fn workflow_run_deps(&self) -> WorkflowRunDeps {
        WorkflowRunDeps {
            snapshot_store: self.inner.snapshot_store.clone(),
            agents: Some(self.inner.agent_registry.clone()),
            workflow_repo: None,
            tool_executor: None,
        }
    }

    pub async fn serve(&self) -> Result<ServeHandle, OrkError> {
        self.resume_pending_workflows_on_startup().await;
        self.spawn_cron_scheduler_if_needed();

        let backend = self.serve_backend.as_ref().ok_or_else(|| {
            OrkError::Configuration {
                message: "OrkApp::serve: no HTTP backend registered; call OrkAppBuilder::serve_backend with an adapter (e.g. ork_server::AxumServer) before build()"
                    .into(),
            }
        })?;
        backend
            .start(Arc::new(self.inner.server_config.clone()))
            .await
    }

    async fn resume_pending_workflows_on_startup(&self) {
        if !self.inner.server_config.resume_on_startup {
            return;
        }
        let Some(store) = self.inner.snapshot_store.clone() else {
            tracing::warn!("resume_on_startup enabled but no snapshot_store configured");
            return;
        };
        let pending = match store.list_pending().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "resume_on_startup: list_pending failed");
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        let code_first_pending: Vec<&str> = pending
            .iter()
            .filter(|r| {
                self.inner
                    .code_first_programs
                    .contains_key(&r.key.workflow_id)
            })
            .map(|r| r.key.workflow_id.as_str())
            .collect();
        tracing::warn!(
            count = pending.len(),
            code_first_ids = ?code_first_pending,
            "resume_on_startup: pending workflow snapshot row(s) found; automatic OS-level replay is not wired yet — use WorkflowRunHandle::resume after reconnecting clients (ADR-0050 follow-up).",
        );
    }

    fn spawn_cron_scheduler_if_needed(&self) {
        let mut sched = ork_workflow::SchedulerService::new();
        for (id, wf) in &self.inner.workflows {
            if let Some((expr, tz)) = wf.cron_trigger() {
                if !tz.eq_ignore_ascii_case("utc") {
                    tracing::warn!(
                        workflow_id = %id,
                        tz = %tz,
                        "cron trigger timezone is not applied yet; evaluating expression in UTC"
                    );
                }
                if let Err(e) = sched.register_cron(id.clone(), &expr) {
                    tracing::warn!(workflow_id = %id, error = %e, "invalid cron expression on workflow");
                }
            }
        }
        if sched.is_empty() {
            return;
        }
        let sched = Arc::new(Mutex::new(sched));
        let app = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let now = Utc::now();
                let fired = {
                    let g = sched.lock().await;
                    g.tick(now).await
                };
                for wid in fired {
                    let wf = match app.workflow(&wid) {
                        Some(w) => w,
                        None => continue,
                    };
                    let ctx = synthetic_workflow_agent_context();
                    let deps = app.workflow_run_deps();
                    match wf.run(ctx, serde_json::Value::Null, deps).await {
                        Ok(_) => tracing::info!(workflow_id = %wid, "cron trigger fired workflow"),
                        Err(e) => {
                            tracing::warn!(workflow_id = %wid, error = %e, "cron workflow run failed")
                        }
                    }
                }
            }
        });
    }

    pub async fn run_agent(
        &self,
        agent_id: &str,
        ctx: AgentContext,
        prompt: ChatMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let agent = self
            .agent(agent_id)
            .ok_or_else(|| OrkError::NotFound(format!("unknown agent id `{agent_id}`")))?;
        if ctx.cancel.is_cancelled() {
            return Err(OrkError::Internal(
                "agent run cancelled before start".into(),
            ));
        }
        agent.send_stream(ctx, prompt).await
    }

    pub async fn run_workflow(
        &self,
        workflow_id: &str,
        ctx: AgentContext,
        input: Value,
    ) -> Result<WorkflowRunHandle, OrkError> {
        let wf = self
            .workflow(workflow_id)
            .ok_or_else(|| OrkError::NotFound(format!("unknown workflow id `{workflow_id}`")))?;

        if ctx.cancel.is_cancelled() {
            return Err(OrkError::Internal(
                "workflow run cancelled before start".into(),
            ));
        }

        let deps = self.workflow_run_deps();
        wf.run(ctx, input, deps).await
    }
}

fn synthetic_workflow_agent_context() -> AgentContext {
    let tenant_id = TenantId::new();
    AgentContext {
        tenant_id,
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id,
            user_id: None,
            scopes: vec![],
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
    }
}
