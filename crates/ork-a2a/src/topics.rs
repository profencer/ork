//! Kafka topic-name helpers for ork's async A2A plane (ADR
//! [`0004`](../../../docs/adrs/0004-hybrid-kong-kafka-transport.md)).
//!
//! Wire-format names live here so `ork-eventing` and `ork-core::a2a::transport` can stay
//! backend-agnostic. Every helper produces a name in the `ork.a2a.v1.*` namespace by default
//! (the `namespace` argument is configurable; the seven tests in this module pin the wire
//! format against the table in the ADR).
//!
//! Topic table (mirrors ADR 0004 §`Async plane: Kafka topic layout`, plus the cancel
//! channel from ADR 0006 §`Cancellation propagation`):
//!
//! | Topic                             | Helper                  |
//! | --------------------------------- | ----------------------- |
//! | `<ns>.discovery.agentcards`       | [`discovery_agentcards`]|
//! | `<ns>.discovery.gatewaycards`     | [`discovery_gatewaycards`]|
//! | `<ns>.agent.request.<agent_id>`   | [`agent_request`]       |
//! | `<ns>.agent.status.<task_id>`     | [`agent_status`]        |
//! | `<ns>.agent.response.<client_id>` | [`agent_response`]      |
//! | `<ns>.agent.cancel`               | [`agent_cancel`]        |
//! | `<ns>.push.outbox`                | [`push_outbox`]         |
//! | `<ns>.trust.cards`                | [`trust_cards`]         |

/// The default Kafka namespace for ork A2A topics. Operators can override this via
/// `ORK__KAFKA__NAMESPACE`; helpers below take the namespace as an argument so callers stay
/// configuration-driven.
pub const DEFAULT_NAMESPACE: &str = "ork.a2a.v1";

/// `<ns>.discovery.agentcards` — agent card heartbeats (key = `agent_id`).
#[must_use]
pub fn discovery_agentcards(namespace: &str) -> String {
    format!("{namespace}.discovery.agentcards")
}

/// `<ns>.discovery.gatewaycards` — gateway card heartbeats (key = `gateway_id`).
#[must_use]
pub fn discovery_gatewaycards(namespace: &str) -> String {
    format!("{namespace}.discovery.gatewaycards")
}

/// `<ns>.agent.request.<agent_id>` — fire-and-forget delegation requests (key = `task_id`).
#[must_use]
pub fn agent_request(namespace: &str, agent_id: &str) -> String {
    format!("{namespace}.agent.request.{agent_id}")
}

/// `<ns>.agent.status.<task_id>` — mid-task status events for one task (key = `task_id`).
#[must_use]
pub fn agent_status(namespace: &str, task_id: &str) -> String {
    format!("{namespace}.agent.status.{task_id}")
}

/// `<ns>.agent.response.<client_id>` — final task responses to fire-and-forget callers
/// (key = `client_id`).
#[must_use]
pub fn agent_response(namespace: &str, client_id: &str) -> String {
    format!("{namespace}.agent.response.{client_id}")
}

/// `<ns>.agent.cancel` — best-effort cancel events for fire-and-forget delegations
/// (ADR 0006 §`Cancellation propagation`; key = `task_id`).
#[must_use]
pub fn agent_cancel(namespace: &str) -> String {
    format!("{namespace}.agent.cancel")
}

/// `<ns>.push.outbox` — push-notification delivery jobs (key = `task_id`).
#[must_use]
pub fn push_outbox(namespace: &str) -> String {
    format!("{namespace}.push.outbox")
}

/// `<ns>.trust.cards` — trust attestations bound to broker identity (key = `agent_id`).
#[must_use]
pub fn trust_cards(namespace: &str) -> String {
    format!("{namespace}.trust.cards")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The asserts below are wire-format pins. Bumping any of them is a wire-protocol break
    // and must come with a new ADR per ADR-0001's `Supersedes` rule.

    #[test]
    fn discovery_agentcards_is_namespaced() {
        assert_eq!(
            discovery_agentcards(DEFAULT_NAMESPACE),
            "ork.a2a.v1.discovery.agentcards"
        );
    }

    #[test]
    fn discovery_gatewaycards_is_namespaced() {
        assert_eq!(
            discovery_gatewaycards(DEFAULT_NAMESPACE),
            "ork.a2a.v1.discovery.gatewaycards"
        );
    }

    #[test]
    fn agent_request_includes_agent_id() {
        assert_eq!(
            agent_request(DEFAULT_NAMESPACE, "planner"),
            "ork.a2a.v1.agent.request.planner"
        );
    }

    #[test]
    fn agent_status_includes_task_id() {
        assert_eq!(
            agent_status(DEFAULT_NAMESPACE, "abc-123"),
            "ork.a2a.v1.agent.status.abc-123"
        );
    }

    #[test]
    fn agent_response_includes_client_id() {
        assert_eq!(
            agent_response(DEFAULT_NAMESPACE, "client-7"),
            "ork.a2a.v1.agent.response.client-7"
        );
    }

    #[test]
    fn push_outbox_is_singleton() {
        assert_eq!(push_outbox(DEFAULT_NAMESPACE), "ork.a2a.v1.push.outbox");
    }

    #[test]
    fn agent_cancel_is_singleton() {
        assert_eq!(agent_cancel(DEFAULT_NAMESPACE), "ork.a2a.v1.agent.cancel");
    }

    #[test]
    fn trust_cards_is_singleton() {
        assert_eq!(trust_cards(DEFAULT_NAMESPACE), "ork.a2a.v1.trust.cards");
    }

    #[test]
    fn namespace_is_configurable() {
        // Operators may scope topics to e.g. a region cluster.
        assert_eq!(
            discovery_agentcards("ork.eu-west.v1"),
            "ork.eu-west.v1.discovery.agentcards"
        );
    }
}
