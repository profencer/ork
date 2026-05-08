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
//! | [`EXT_MESH_TRUST`] | `params.accepted_scopes` + `params.accepts_external_tenants` for ADR-0020 mesh-token narrowing and cross-tenant policy |
//!
//! These are wire-format pins. Bumping either string is a wire break and must be reasoned
//! about under the ADR-0001 supersedes rule.

/// `transport-hint` — exposes the Kafka request topic for callers that can speak Kafka,
/// alongside the spec-mandated HTTP `url`. ADR-0005, mirrors SAM's `gateway-role` extension.
pub const EXT_TRANSPORT_HINT: &str = "https://ork.dev/a2a/extensions/transport-hint";

/// `tenant-required` — declares which HTTP header carries the tenant id (ADR-0020).
pub const EXT_TENANT_REQUIRED: &str = "https://ork.dev/a2a/extensions/tenant-required";

/// `mesh-trust` — ADR-0020 §`Mesh trust`. Card-side companion to the
/// `X-Ork-Mesh-Token` JWT shape. `params.accepted_scopes` lists scopes
/// the destination agent is willing to accept; the calling ork narrows
/// the originator's scope set against this list when minting the token.
/// `params.accepts_external_tenants` (bool, default false) signals that
/// the agent accepts cross-tenant delegations without `tenant:admin`
/// (ADR-0020 §`Tenant id propagation across delegation`).
pub const EXT_MESH_TRUST: &str = "https://ork.dev/a2a/extensions/mesh-trust";

/// Common parameter keys for [`EXT_TRANSPORT_HINT`].
pub const PARAM_KAFKA_REQUEST_TOPIC: &str = "kafka_request_topic";

/// Common parameter keys for [`EXT_TENANT_REQUIRED`].
pub const PARAM_TENANT_HEADER: &str = "header";

/// Common parameter keys for [`EXT_MESH_TRUST`].
pub const PARAM_ACCEPTED_SCOPES: &str = "accepted_scopes";
pub const PARAM_ACCEPTS_EXTERNAL_TENANTS: &str = "accepts_external_tenants";

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

    #[test]
    fn mesh_trust_uri_is_pinned() {
        assert_eq!(EXT_MESH_TRUST, "https://ork.dev/a2a/extensions/mesh-trust");
        assert_eq!(PARAM_ACCEPTED_SCOPES, "accepted_scopes");
        assert_eq!(PARAM_ACCEPTS_EXTERNAL_TENANTS, "accepts_external_tenants");
    }
}
