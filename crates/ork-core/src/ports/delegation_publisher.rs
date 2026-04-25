//! Publish-side port for fire-and-forget peer delegation
//! (ADR [`0006`](../../../docs/adrs/0006-peer-delegation.md) §`b) Async (await:false)`).
//!
//! `ork-core` cannot depend on `ork-eventing` (it would cycle: eventing already depends
//! on core). The engine therefore depends on this minimal trait; the API wires an
//! eventing-backed implementation that publishes to `<ns>.agent.request.<agent_id>`
//! and `<ns>.agent.cancel`.

use async_trait::async_trait;
use ork_a2a::TaskId;
use ork_common::error::OrkError;

use crate::a2a::AgentId;

/// Backend-agnostic publisher used by the engine and the `agent_call` tool when
/// `await: false`. The implementation owns the topic naming (it has access to the
/// configured A2A namespace) and the wire format (Solace A2A 1.0 `MessageSendParams`
/// or, for cancel, a small `{"task_id": "..."}` envelope).
#[async_trait]
pub trait DelegationPublisher: Send + Sync {
    /// Publish a fire-and-forget request to `target_agent`. `payload` is the JSON-RPC
    /// `MessageSendParams` body (already serialized) so the publisher only adds
    /// headers and routing.
    async fn publish_request(
        &self,
        target_agent: &AgentId,
        task_id: TaskId,
        payload: &[u8],
    ) -> Result<(), OrkError>;

    /// Publish a best-effort cancel marker for an in-flight fire-and-forget child task.
    /// Implementations that lack a configured broker should return `Ok(())`.
    async fn publish_cancel(&self, task_id: TaskId) -> Result<(), OrkError>;
}
