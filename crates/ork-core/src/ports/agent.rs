use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::Stream;
use ork_common::error::OrkError;

use crate::a2a::{AgentCard, AgentContext, AgentEvent, AgentId, AgentMessage, TaskId};
use crate::ports::memory_store::MemoryStore;

pub type AgentEventStream =
    Pin<Box<dyn Stream<Item = Result<AgentEvent, OrkError>> + Send + 'static>>;

#[async_trait]
pub trait Agent: Send + Sync {
    fn id(&self) -> &AgentId;
    fn card(&self) -> &AgentCard;

    async fn send(&self, ctx: AgentContext, msg: AgentMessage) -> Result<AgentMessage, OrkError> {
        let mut stream = self.send_stream(ctx, msg).await?;
        let mut last_message: Option<AgentMessage> = None;
        while let Some(item) = stream.next().await {
            match item? {
                AgentEvent::Message(m) => last_message = Some(m),
                AgentEvent::StatusUpdate(_) | AgentEvent::ArtifactUpdate(_) => {}
            }
        }
        last_message.ok_or_else(|| {
            OrkError::LlmProvider("agent stream ended without a final message".into())
        })
    }

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: AgentMessage,
    ) -> Result<AgentEventStream, OrkError>;

    async fn cancel(&self, _ctx: AgentContext, _task_id: &TaskId) -> Result<(), OrkError> {
        Err(OrkError::Unsupported("cancel".into()))
    }

    /// Ids of *peer agents* this agent depends on (e.g., via
    /// [ADR-0052](../../../docs/adrs/0052-code-first-agent-dsl.md)
    /// `agent_as_tool`). Symmetric with
    /// [`WorkflowDef::referenced_agent_ids`](crate::ports::workflow_def::WorkflowDef::referenced_agent_ids):
    /// `OrkAppBuilder::build()` validates each id is registered in the same app.
    /// Default `&[]` keeps existing implementations unaffected.
    fn referenced_agent_ids(&self) -> &[String] {
        &[]
    }

    /// Ids of workflows this agent depends on (e.g., via
    /// [ADR-0052](../../../docs/adrs/0052-code-first-agent-dsl.md)
    /// `workflow_as_tool`). Validated symmetrically with peer agents.
    fn referenced_workflow_ids(&self) -> &[String] {
        &[]
    }

    /// Ids of MCP servers this agent expects to be registered on the same
    /// `OrkApp` (e.g., via
    /// [ADR-0052](../../../docs/adrs/0052-code-first-agent-dsl.md)
    /// `tool_server`). Validated against `OrkApp::mcp_servers`.
    fn referenced_mcp_server_ids(&self) -> &[String] {
        &[]
    }

    /// ADR-0053: install the [`MemoryStore`] registered on the parent
    /// [`OrkApp`](../../../../ork-app/src/app.rs). Default no-op so
    /// agents that do not consume memory (e.g.
    /// [`crate::ports::remote_agent_builder`]'s remote agent) compile
    /// without changes; concrete agents (`CodeAgent`) override and
    /// store the handle for later use during `send_stream`. Idempotent
    /// — implementations are expected to use a `OnceLock` or similar so
    /// repeated calls are absorbed.
    fn inject_memory(&self, _memory: Arc<dyn MemoryStore>) {}
}
