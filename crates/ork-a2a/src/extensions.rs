//! Reserved A2A extension URIs owned by ork (ADR
//! [`0005`](../../../docs/adrs/0005-agent-card-and-devportal-discovery.md) §`Card content`).
//!
//! These constants are the wire-format names of ork-specific extensions added to
//! [`crate::AgentCard::extensions`]. Spec-strict A2A clients ignore unknown URIs, so adding
//! one here is a forward-compatible card change.
//!
//! | URI | Carries |
//! | --- | ------- |
//! | [`EXT_TRANSPORT_HINT`] | `params.kafka_request_topic` for callers that can speak Kafka |
//! | [`EXT_TENANT_REQUIRED`] | `params.header` declaring which header carries the tenant id |
//!
//! These are wire-format pins. Bumping either string is a wire break and must be reasoned
//! about under the ADR-0001 supersedes rule.

/// `transport-hint` — exposes the Kafka request topic for callers that can speak Kafka,
/// alongside the spec-mandated HTTP `url`. ADR-0005, mirrors SAM's `gateway-role` extension.
pub const EXT_TRANSPORT_HINT: &str = "https://ork.dev/a2a/extensions/transport-hint";

/// `tenant-required` — declares which HTTP header carries the tenant id (ADR-0020).
pub const EXT_TENANT_REQUIRED: &str = "https://ork.dev/a2a/extensions/tenant-required";

/// Common parameter keys for [`EXT_TRANSPORT_HINT`].
pub const PARAM_KAFKA_REQUEST_TOPIC: &str = "kafka_request_topic";

/// Common parameter keys for [`EXT_TENANT_REQUIRED`].
pub const PARAM_TENANT_HEADER: &str = "header";

#[cfg(test)]
mod tests {
    use super::*;

    // The asserts below are wire-format pins. See module docs.

    #[test]
    fn transport_hint_uri_is_pinned() {
        assert_eq!(
            EXT_TRANSPORT_HINT,
            "https://ork.dev/a2a/extensions/transport-hint"
        );
    }

    #[test]
    fn tenant_required_uri_is_pinned() {
        assert_eq!(
            EXT_TENANT_REQUIRED,
            "https://ork.dev/a2a/extensions/tenant-required"
        );
    }

    #[test]
    fn param_keys_are_pinned() {
        assert_eq!(PARAM_KAFKA_REQUEST_TOPIC, "kafka_request_topic");
        assert_eq!(PARAM_TENANT_HEADER, "header");
    }
}
