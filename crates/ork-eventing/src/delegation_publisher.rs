//! Kafka-backed implementation of [`ork_core::ports::delegation_publisher::DelegationPublisher`].
//!
//! Wires the engine / `agent_call` tool's fire-and-forget delegation requests
//! (ADR [`0006`](../../../docs/adrs/0006-peer-delegation.md) §`b) Async (await:false)`)
//! onto the topic layout pinned in [`ork_a2a::topics`].
//!
//! ## Topic mapping
//!
//! - Request: `<ns>.agent.request.<agent_id>` keyed by `task_id`.
//! - Cancel:  `<ns>.agent.cancel` keyed by `task_id` (singleton topic; ADR 0006
//!   §`Cancellation propagation`).

use std::sync::Arc;

use async_trait::async_trait;
use ork_a2a::TaskId;
use ork_a2a::topics;
use ork_common::error::OrkError;
use ork_core::a2a::AgentId;
use ork_core::ports::delegation_publisher::DelegationPublisher;

use crate::producer::Producer;

/// Publishes delegation requests/cancels onto the configured A2A namespace.
///
/// Cheap to clone — wraps an `Arc<dyn Producer>` and an owned namespace string.
#[derive(Clone)]
pub struct KafkaDelegationPublisher {
    producer: Arc<dyn Producer>,
    namespace: String,
}

impl KafkaDelegationPublisher {
    /// Build a publisher bound to `namespace` (typically `cfg.kafka.namespace`).
    #[must_use]
    pub fn new(producer: Arc<dyn Producer>, namespace: impl Into<String>) -> Self {
        Self {
            producer,
            namespace: namespace.into(),
        }
    }
}

#[async_trait]
impl DelegationPublisher for KafkaDelegationPublisher {
    async fn publish_request(
        &self,
        target_agent: &AgentId,
        task_id: TaskId,
        payload: &[u8],
    ) -> Result<(), OrkError> {
        let topic = topics::agent_request(&self.namespace, target_agent.as_str());
        let key = task_id.to_string();
        // ADR 0004 will grow the JSON-RPC envelope headers in a follow-up; for now
        // we publish the raw `MessageSendParams` body the engine has already serialized.
        self.producer
            .publish(&topic, Some(key.as_bytes()), &[], payload)
            .await
            .map_err(|e| {
                OrkError::Integration(format!(
                    "delegation publish_request failed (topic={topic}): {e}"
                ))
            })
    }

    async fn publish_cancel(&self, task_id: TaskId) -> Result<(), OrkError> {
        let topic = topics::agent_cancel(&self.namespace);
        let key = task_id.to_string();
        let payload = serde_json::to_vec(&serde_json::json!({ "task_id": key }))
            .map_err(|e| OrkError::Internal(format!("delegation publish_cancel serialize: {e}")))?;
        // Best-effort: callers swallow errors but we still surface them so they end up
        // in tracing.
        self.producer
            .publish(&topic, Some(key.as_bytes()), &[], &payload)
            .await
            .map_err(|e| {
                OrkError::Integration(format!(
                    "delegation publish_cancel failed (topic={topic}): {e}"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use tokio::time::timeout;

    use super::*;
    use crate::consumer::Consumer;
    use crate::in_memory::InMemoryBackend;

    #[tokio::test]
    async fn publish_request_uses_per_agent_topic() {
        let backend = Arc::new(InMemoryBackend::new());
        let publisher: Arc<dyn DelegationPublisher> =
            Arc::new(KafkaDelegationPublisher::new(backend.clone(), "ork.a2a.v1"));

        let mut stream = backend
            .subscribe("ork.a2a.v1.agent.request.planner")
            .await
            .unwrap();
        let agent: AgentId = "planner".into();
        let task_id = TaskId::new();
        publisher
            .publish_request(&agent, task_id, b"{\"foo\":1}")
            .await
            .unwrap();

        let got = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"{\"foo\":1}");
        assert_eq!(got.key.as_deref(), Some(task_id.to_string().as_bytes()));
    }

    #[tokio::test]
    async fn publish_cancel_targets_singleton_topic() {
        let backend = Arc::new(InMemoryBackend::new());
        let publisher = KafkaDelegationPublisher::new(backend.clone(), "ork.a2a.v1");

        let mut stream = backend.subscribe("ork.a2a.v1.agent.cancel").await.unwrap();
        let task_id = TaskId::new();
        publisher.publish_cancel(task_id).await.unwrap();

        let got = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&got.payload).unwrap();
        assert_eq!(v["task_id"], serde_json::Value::String(task_id.to_string()));
    }
}
