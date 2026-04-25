//! Helpers that build the ork-specific A2A `AgentExtension` entries declared in ADR-0005.
//!
//! Wire URIs and param keys live in [`ork_a2a::extensions`]; this module is the
//! Rust-typed builder that callers (e.g. the local-agent card builder) use to assemble the
//! extension structs without juggling raw JSON.

use ork_a2a::AgentExtension;
use ork_a2a::extensions::{
    EXT_TENANT_REQUIRED, EXT_TRANSPORT_HINT, PARAM_KAFKA_REQUEST_TOPIC, PARAM_TENANT_HEADER,
};
use serde_json::Value;

/// `transport-hint` extension carrying the Kafka request topic for callers that can speak
/// Kafka in addition to the spec-mandated HTTP `url`.
#[must_use]
pub fn transport_hint_extension(kafka_request_topic: impl Into<String>) -> AgentExtension {
    let mut params = serde_json::Map::new();
    params.insert(
        PARAM_KAFKA_REQUEST_TOPIC.into(),
        Value::String(kafka_request_topic.into()),
    );
    AgentExtension {
        uri: EXT_TRANSPORT_HINT.into(),
        description: Some("Kafka request topic for in-mesh callers (ADR-0005).".into()),
        params: Some(params),
    }
}

/// `tenant-required` extension declaring which HTTP header carries the tenant id (ADR-0020).
#[must_use]
pub fn tenant_required_extension(header: impl Into<String>) -> AgentExtension {
    let mut params = serde_json::Map::new();
    params.insert(PARAM_TENANT_HEADER.into(), Value::String(header.into()));
    AgentExtension {
        uri: EXT_TENANT_REQUIRED.into(),
        description: Some("HTTP header carrying the tenant id (ADR-0020).".into()),
        params: Some(params),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_hint_extension_carries_kafka_topic() {
        let ext = transport_hint_extension("ork.a2a.v1.agent.request.planner");
        assert_eq!(ext.uri, EXT_TRANSPORT_HINT);
        let topic = ext
            .params
            .as_ref()
            .and_then(|p| p.get(PARAM_KAFKA_REQUEST_TOPIC))
            .and_then(Value::as_str);
        assert_eq!(topic, Some("ork.a2a.v1.agent.request.planner"));
    }

    #[test]
    fn tenant_required_extension_carries_header_name() {
        let ext = tenant_required_extension("X-Tenant-Id");
        assert_eq!(ext.uri, EXT_TENANT_REQUIRED);
        let header = ext
            .params
            .as_ref()
            .and_then(|p| p.get(PARAM_TENANT_HEADER))
            .and_then(Value::as_str);
        assert_eq!(header, Some("X-Tenant-Id"));
    }
}
