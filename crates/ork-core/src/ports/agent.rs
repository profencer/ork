use std::pin::Pin;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::Stream;
use ork_common::error::OrkError;

use crate::a2a::{AgentCard, AgentContext, AgentEvent, AgentId, AgentMessage, TaskId};

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
}
