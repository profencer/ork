//! Push notification outbox publisher.
//!
//! [`PushService::publish_terminal`] is invoked by the JSON-RPC dispatcher in
//! `ork-api` once `update_state(...terminal)` succeeds. It builds a small JSON
//! envelope and publishes it on `ork.a2a.v1.push.outbox` (helper:
//! [`ork_a2a::topics::push_outbox`]). Publishing is best-effort: a Kafka outage
//! must not poison the inbound JSON-RPC response, so failures are logged at
//! WARN and swallowed.
//!
//! The delivery worker [`crate::worker`] consumes the same envelope from
//! Kafka, fans the payload out to every `a2a_push_configs` row registered for
//! the task, and signs each request via [`crate::JwksProvider`].

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ork_a2a::{TaskId, TaskState, topics};
use ork_common::types::TenantId;
use ork_eventing::EventingClient;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushOutboxEnvelope {
    pub task_id: TaskId,
    pub tenant_id: TenantId,
    /// A2A `TaskState` serialised as the wire string the JSON-RPC API uses
    /// (`"completed"`, `"failed"`, `"canceled"`, `"rejected"`).
    pub state: String,
    pub occurred_at: DateTime<Utc>,
}

/// Outbox publisher held in `AppState`. Public surface intentionally tiny so
/// the JSON-RPC dispatcher can call `publish_terminal(...)` from its three
/// existing terminal-state sites with a single line.
pub struct PushService {
    eventing: EventingClient,
    namespace: String,
}

impl PushService {
    #[must_use]
    pub fn new(eventing: EventingClient, namespace: String) -> Arc<Self> {
        Arc::new(Self {
            eventing,
            namespace,
        })
    }

    /// Publish a terminal-state envelope onto `ork.a2a.v1.push.outbox`.
    /// Returns `Ok(())` on a successful publish; a Kafka error is logged at
    /// WARN and surfaced as `Ok(())` so callers don't have to wrap.
    pub async fn publish_terminal(&self, tenant_id: TenantId, task_id: TaskId, state: TaskState) {
        let envelope = PushOutboxEnvelope {
            task_id,
            tenant_id,
            state: state_to_wire(state).to_owned(),
            occurred_at: Utc::now(),
        };
        let payload = match serde_json::to_vec(&envelope) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "ADR-0009: failed to serialise push outbox envelope");
                return;
            }
        };
        let topic = topics::push_outbox(&self.namespace);
        let key = task_id.to_string();
        if let Err(e) = self
            .eventing
            .producer
            .publish(&topic, Some(key.as_bytes()), &[], &payload)
            .await
        {
            tracing::warn!(
                error = %e,
                topic = %topic,
                task_id = %key,
                "ADR-0009: failed to publish push outbox envelope"
            );
        }
    }
}

/// Wire form of [`TaskState`] used in the outbox envelope and on the receiver
/// side. Pinned here rather than relying on `TaskState`'s serde because the
/// envelope is a public contract that crosses the Kafka boundary.
#[must_use]
pub const fn state_to_wire(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::InputRequired => "input_required",
        TaskState::AuthRequired => "auth_required",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::Rejected => "rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::time::timeout;
    use uuid::Uuid;

    #[tokio::test]
    async fn topic_is_pinned_to_push_outbox() {
        let eventing = EventingClient::in_memory();
        let svc = PushService::new(eventing.clone(), "ork.a2a.v1".into());

        let mut sub = eventing
            .consumer
            .subscribe("ork.a2a.v1.push.outbox")
            .await
            .unwrap();

        let task_id = TaskId(Uuid::now_v7());
        let tenant_id = TenantId(Uuid::now_v7());
        svc.publish_terminal(tenant_id, task_id, TaskState::Completed)
            .await;

        let msg = timeout(Duration::from_millis(200), sub.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let env: PushOutboxEnvelope = serde_json::from_slice(&msg.payload).unwrap();
        assert_eq!(env.task_id, task_id);
        assert_eq!(env.tenant_id, tenant_id);
        assert_eq!(env.state, "completed");
    }

    #[test]
    fn envelope_serde_shape_is_pinned() {
        let env = PushOutboxEnvelope {
            task_id: TaskId(Uuid::nil()),
            tenant_id: TenantId(Uuid::nil()),
            state: "completed".into(),
            occurred_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 4, 24, 12, 0, 0).unwrap(),
        };
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["task_id"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(json["tenant_id"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(json["state"], "completed");
        assert_eq!(json["occurred_at"], "2026-04-24T12:00:00Z");
    }
}
