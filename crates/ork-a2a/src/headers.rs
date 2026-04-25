//! Kafka header constants and envelope helpers for ork's async A2A plane (ADR
//! [`0004`](../../../docs/adrs/0004-hybrid-kong-kafka-transport.md)).
//!
//! Every A2A message on Kafka carries the same JSON-RPC envelope used on the sync plane
//! (ADR [`0003`](../../../docs/adrs/0003-a2a-protocol-model.md)), plus Kafka headers that
//! mirror SAM's user properties:
//!
//! | Header                    | SAM equivalent     | Meaning                           |
//! | ------------------------- | ------------------ | --------------------------------- |
//! | [`ORK_A2A_VERSION`]       | n/a                | Wire-format version (`"1.0"`)     |
//! | [`ORK_TASK_ID`]           | `taskId`           | A2A task id                       |
//! | [`ORK_CONTEXT_ID`]        | `contextId`        | A2A conversation context id       |
//! | [`ORK_REPLY_TOPIC`]       | `replyTo`          | Topic for the response            |
//! | [`ORK_STATUS_TOPIC`]      | `a2aStatusTopic`   | Topic for status updates          |
//! | [`ORK_TENANT_ID`]         | tenant in payload  | Tenant scoping (ADR 0020)         |
//! | [`ORK_TRACE_ID`]          | `traceparent`      | W3C trace propagation             |
//! | [`ORK_CONTENT_TYPE`]      | `application/json` | Always JSON-RPC                   |
//!
//! Use [`KafkaEnvelope`] to assemble a producer-ready record from a JSON-RPC request.

use serde::Serialize;

use crate::ids::{ContextId, TaskId};
use crate::jsonrpc::JsonRpcRequest;

pub const ORK_A2A_VERSION: &str = "ork-a2a-version";
pub const ORK_TASK_ID: &str = "ork-task-id";
pub const ORK_CONTEXT_ID: &str = "ork-context-id";
pub const ORK_REPLY_TOPIC: &str = "ork-reply-topic";
pub const ORK_STATUS_TOPIC: &str = "ork-status-topic";
pub const ORK_TENANT_ID: &str = "ork-tenant-id";
pub const ORK_TRACE_ID: &str = "ork-trace-id";
pub const ORK_CONTENT_TYPE: &str = "ork-content-type";

/// Discovery event type header (ADR-0005). Carries one of the `DISCOVERY_EVENT_*` values.
pub const ORK_DISCOVERY_EVENT: &str = "ork-discovery-event";

/// First card publication when an agent comes up.
pub const DISCOVERY_EVENT_BORN: &str = "born";
/// Periodic re-publication every `discovery_interval`.
pub const DISCOVERY_EVENT_HEARTBEAT: &str = "heartbeat";
/// Card content changed (e.g. plugin loaded a new tool, ADR-0014).
pub const DISCOVERY_EVENT_CHANGED: &str = "changed";
/// Tombstone published on graceful shutdown (payload is empty).
pub const DISCOVERY_EVENT_DIED: &str = "died";

/// Wire-format version that lands in [`ORK_A2A_VERSION`].
pub const WIRE_VERSION: &str = "1.0";

/// Default content type for all A2A Kafka messages.
pub const DEFAULT_CONTENT_TYPE: &str = "application/json";

/// Producer-ready Kafka record: a JSON payload plus the ADR-0004 header set.
///
/// `headers` is an ordered `Vec` (not a map) because the Kafka wire format allows duplicates
/// and preserves order. `payload` is the JSON-RPC envelope serialised with `serde_json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KafkaEnvelope {
    pub headers: Vec<(String, Vec<u8>)>,
    pub payload: Vec<u8>,
}

impl KafkaEnvelope {
    /// Assemble an envelope from a JSON-RPC request and per-message routing metadata.
    ///
    /// `task_id` and `context_id` are required; the optional `reply_topic` /
    /// `status_topic` / `tenant_id` / `trace_id` are written only when `Some`.
    pub fn from_jsonrpc<P: Serialize>(
        req: &JsonRpcRequest<P>,
        task_id: &TaskId,
        context_id: &ContextId,
        reply_topic: Option<&str>,
        status_topic: Option<&str>,
        tenant_id: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<Self, serde_json::Error> {
        let payload = serde_json::to_vec(req)?;

        let mut headers: Vec<(String, Vec<u8>)> = Vec::with_capacity(8);
        headers.push((
            ORK_A2A_VERSION.to_string(),
            WIRE_VERSION.as_bytes().to_vec(),
        ));
        headers.push((ORK_TASK_ID.to_string(), task_id.to_string().into_bytes()));
        headers.push((
            ORK_CONTEXT_ID.to_string(),
            context_id.to_string().into_bytes(),
        ));
        if let Some(t) = reply_topic {
            headers.push((ORK_REPLY_TOPIC.to_string(), t.as_bytes().to_vec()));
        }
        if let Some(t) = status_topic {
            headers.push((ORK_STATUS_TOPIC.to_string(), t.as_bytes().to_vec()));
        }
        if let Some(t) = tenant_id {
            headers.push((ORK_TENANT_ID.to_string(), t.as_bytes().to_vec()));
        }
        if let Some(t) = trace_id {
            headers.push((ORK_TRACE_ID.to_string(), t.as_bytes().to_vec()));
        }
        headers.push((
            ORK_CONTENT_TYPE.to_string(),
            DEFAULT_CONTENT_TYPE.as_bytes().to_vec(),
        ));

        Ok(Self { headers, payload })
    }

    /// Look up a header value by name. Returns the first match (Kafka allows duplicates).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_slice())
    }

    /// Header value as UTF-8 string, if present and decodable.
    #[must_use]
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name).and_then(|v| std::str::from_utf8(v).ok())
    }

    /// Total wire size (payload + sum of header bytes). Useful for size-cap enforcement at
    /// the producer.
    #[must_use]
    pub fn wire_size(&self) -> usize {
        self.payload.len()
            + self
                .headers
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ContextId;
    use crate::jsonrpc::{A2aMethod, JsonRpcRequest};
    use crate::methods::MessageSendParams;
    use crate::types::{Message, Part};

    fn sample_request() -> JsonRpcRequest<MessageSendParams> {
        let msg = Message::user(vec![Part::text("hello")]);
        let params = MessageSendParams {
            message: msg,
            configuration: None,
            metadata: None,
        };
        JsonRpcRequest::new(
            Some(serde_json::Value::String("rpc-1".into())),
            A2aMethod::MessageSend,
            Some(params),
        )
    }

    #[test]
    fn envelope_carries_required_headers() {
        let req = sample_request();
        let task_id = TaskId::new();
        let ctx_id = ContextId::new();

        let env = KafkaEnvelope::from_jsonrpc(
            &req,
            &task_id,
            &ctx_id,
            Some("ork.a2a.v1.agent.response.client-7"),
            Some("ork.a2a.v1.agent.status.task-1"),
            Some("tenant-abc"),
            Some("00-trace-01"),
        )
        .expect("envelope build");

        assert_eq!(env.header_str(ORK_A2A_VERSION), Some(WIRE_VERSION));
        assert_eq!(
            env.header_str(ORK_TASK_ID),
            Some(task_id.to_string().as_str())
        );
        assert_eq!(
            env.header_str(ORK_CONTEXT_ID),
            Some(ctx_id.to_string().as_str())
        );
        assert_eq!(
            env.header_str(ORK_REPLY_TOPIC),
            Some("ork.a2a.v1.agent.response.client-7")
        );
        assert_eq!(
            env.header_str(ORK_STATUS_TOPIC),
            Some("ork.a2a.v1.agent.status.task-1")
        );
        assert_eq!(env.header_str(ORK_TENANT_ID), Some("tenant-abc"));
        assert_eq!(env.header_str(ORK_TRACE_ID), Some("00-trace-01"));
        assert_eq!(env.header_str(ORK_CONTENT_TYPE), Some(DEFAULT_CONTENT_TYPE));
    }

    #[test]
    fn envelope_omits_optional_headers_when_none() {
        let req = sample_request();
        let env = KafkaEnvelope::from_jsonrpc(
            &req,
            &TaskId::new(),
            &ContextId::new(),
            None,
            None,
            None,
            None,
        )
        .expect("envelope build");

        assert!(env.header(ORK_REPLY_TOPIC).is_none());
        assert!(env.header(ORK_STATUS_TOPIC).is_none());
        assert!(env.header(ORK_TENANT_ID).is_none());
        assert!(env.header(ORK_TRACE_ID).is_none());

        assert_eq!(env.header_str(ORK_A2A_VERSION), Some(WIRE_VERSION));
        assert_eq!(env.header_str(ORK_CONTENT_TYPE), Some(DEFAULT_CONTENT_TYPE));
    }

    #[test]
    fn payload_roundtrips_through_serde_json() {
        let req = sample_request();
        let env = KafkaEnvelope::from_jsonrpc(
            &req,
            &TaskId::new(),
            &ContextId::new(),
            None,
            None,
            None,
            None,
        )
        .expect("envelope build");

        let parsed: JsonRpcRequest<MessageSendParams> =
            serde_json::from_slice(&env.payload).expect("payload parse");
        assert_eq!(parsed.jsonrpc, "2.0");
        assert_eq!(parsed.method, A2aMethod::MessageSend.to_wire_string());
        assert!(parsed.params.is_some());
    }

    #[test]
    fn wire_version_is_pinned() {
        // Bumping WIRE_VERSION is a wire break; this test exists so the bump shows up in
        // diff review and triggers the `Supersedes`-rule conversation.
        assert_eq!(WIRE_VERSION, "1.0");
    }

    #[test]
    fn discovery_event_header_is_pinned() {
        // ADR-0005 wire-format pin. Renaming this header is a wire break.
        assert_eq!(ORK_DISCOVERY_EVENT, "ork-discovery-event");
    }

    #[test]
    fn discovery_event_values_are_pinned() {
        assert_eq!(DISCOVERY_EVENT_BORN, "born");
        assert_eq!(DISCOVERY_EVENT_HEARTBEAT, "heartbeat");
        assert_eq!(DISCOVERY_EVENT_CHANGED, "changed");
        assert_eq!(DISCOVERY_EVENT_DIED, "died");
    }
}
