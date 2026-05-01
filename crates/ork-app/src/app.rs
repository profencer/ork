//! [`OrkApp`]: central registry value (ADR [`0049`](../../docs/adrs/0049-orkapp-central-registry.md)).

use std::sync::Arc;

use ork_common::error::OrkError;
use ork_core::a2a::{AgentContext, AgentMessage};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::workflow_def::WorkflowDef;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::inner::OrkAppInner;
use crate::manifest::{self, AppManifest};
use crate::ports::server::{ServeHandle, Server};

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

    pub async fn serve(&self) -> Result<ServeHandle, OrkError> {
        let backend = self.serve_backend.as_ref().ok_or_else(|| {
            OrkError::Configuration {
                message: "OrkApp::serve: no HTTP backend registered; call OrkAppBuilder::serve_backend with an adapter (e.g. ork_server::AxumServer) before build()"
                    .into(),
            }
        })?;
        backend
            .start(Arc::new(self.inner.server_config().clone()))
            .await
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

        let cancel = ctx.cancel.clone();
        let wf_clone = Arc::clone(&wf);
        let join = tokio::spawn(async move { wf_clone.run(ctx, input).await });

        Ok(WorkflowRunHandle { join, cancel })
    }
}

/// Handle for a spawned workflow task; ADR [`0050`](../../docs/adrs/0050-code-first-workflow-dsl.md) will extend this.
pub struct WorkflowRunHandle {
    join: JoinHandle<Result<Value, OrkError>>,
    cancel: CancellationToken,
}

impl WorkflowRunHandle {
    pub async fn await_completion(self) -> Result<Value, OrkError> {
        self.join
            .await
            .map_err(|e| OrkError::Internal(format!("workflow task failed to join cleanly: {e}")))?
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
