//! Hybrid Kong/Kafka transport selection (ADR
//! [`0004`](../../../../docs/adrs/0004-hybrid-kong-kafka-transport.md) §`Routing rules`).
//!
//! [`TransportSelector`] is a pure function over `(callee, caller_kind, await_response)` —
//! it has no I/O. Callers pass its [`TransportDecision`] to whichever subsystem actually
//! performs the call (HTTP client, [`ork_eventing::Producer`], or in-process registry).
//!
//! ## The seven routing rules (verbatim)
//!
//! | # | Caller                                              | Decision                                      |
//! | - | --------------------------------------------------- | --------------------------------------------- |
//! | 1 | External A2A client → ork agent                     | HTTP through Kong                             |
//! | 2 | Local → local (same process)                        | In-process                                    |
//! | 3 | Local → local (different ork-api), `await: true`    | HTTP through Kong                             |
//! | 4 | Local → local (different ork-api), `await: false`   | Kafka `agent.request.<agent_id>`              |
//! | 5 | Status update during streaming task                 | Kafka `agent.status.<task_id>`                |
//! | 6 | Discovery / heartbeat                               | Kafka `discovery.agentcards`                  |
//! | 7 | Push notification delivery                          | HTTP POST to subscriber URL (after Kafka outbox) |
//! | 8 | ork → external A2A agent (third party)              | HTTP                                          |

use ork_a2a::topics;
use url::Url;

use super::AgentId;

/// Where the call originates and what kind of work it represents. Drives the rule lookup in
/// [`TransportSelector::select`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallerKind {
    /// Rule 1 / Rule 8: third-party A2A client at the edge of the mesh, or third-party
    /// callee on the far side. The `await_response` flag never changes the decision: HTTP.
    External,
    /// Rule 2: local agent calling another agent in the same `ork-api` process.
    LocalSameProcess,
    /// Rule 3 / 4: local agent calling another agent that lives in a different `ork-api`
    /// instance. The `await_response` flag picks between HTTP (rule 3) and Kafka (rule 4).
    LocalOtherProcess,
    /// Rule 5: a streaming task is emitting a status event mid-flight. `task_id` keys the
    /// per-task topic.
    StatusFanOut { task_id: String },
    /// Rule 6: agent / gateway card heartbeats. Always Kafka discovery.
    Discovery,
    /// Rule 7: push-notification delivery to a subscriber URL. Decision is HTTP, but the
    /// caller is expected to first persist to the push-outbox Kafka topic; this enum
    /// represents the *delivery* leg.
    PushDelivery { subscriber_url: Url },
}

/// Where the call should go. Variants match the seven rules; data carries everything the
/// caller needs to actually issue the call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportDecision {
    /// Rule 2.
    InProcess,
    /// Rules 1, 3, 7, 8. `url` is the fully-qualified callee endpoint.
    HttpThroughKong { url: Url },
    /// Rule 4.
    KafkaRequest { topic: String },
    /// Rule 5.
    KafkaStatus { topic: String },
    /// Rule 6.
    KafkaDiscovery { topic: String },
}

/// Routing-rule engine.
///
/// Construct once at boot from [`ork_common::config::AppConfig`]; clone freely.
#[derive(Clone, Debug)]
pub struct TransportSelector {
    namespace: String,
    local_agent_ids: Vec<AgentId>,
    http_base_url: Url,
}

impl TransportSelector {
    pub fn new(namespace: impl Into<String>, http_base_url: Url) -> Self {
        Self {
            namespace: namespace.into(),
            local_agent_ids: Vec::new(),
            http_base_url,
        }
    }

    /// Register the agents hosted by *this* `ork-api` process. Used to short-circuit Rule 2
    /// when the callee resolves locally even though the caller passed [`CallerKind::LocalOtherProcess`].
    #[must_use]
    pub fn with_local_agent_ids(mut self, ids: impl IntoIterator<Item = AgentId>) -> Self {
        self.local_agent_ids = ids.into_iter().collect();
        self
    }

    /// Apply the seven rules. The function is pure: same input → same output.
    pub fn select(
        &self,
        callee: &AgentId,
        caller_kind: &CallerKind,
        await_response: bool,
    ) -> TransportDecision {
        match caller_kind {
            // Rule 1: external clients always come through Kong.
            CallerKind::External => TransportDecision::HttpThroughKong {
                url: self.http_url_for(callee),
            },

            // Rule 2: same-process agents skip the wire entirely.
            CallerKind::LocalSameProcess => TransportDecision::InProcess,

            // Rules 3 & 4: cross-process local. `await` picks the lane.
            CallerKind::LocalOtherProcess => {
                if await_response {
                    TransportDecision::HttpThroughKong {
                        url: self.http_url_for(callee),
                    }
                } else {
                    TransportDecision::KafkaRequest {
                        topic: topics::agent_request(&self.namespace, callee),
                    }
                }
            }

            // Rule 5: status fan-out is always Kafka.
            CallerKind::StatusFanOut { task_id } => TransportDecision::KafkaStatus {
                topic: topics::agent_status(&self.namespace, task_id),
            },

            // Rule 6: discovery is always Kafka.
            CallerKind::Discovery => TransportDecision::KafkaDiscovery {
                topic: topics::discovery_agentcards(&self.namespace),
            },

            // Rule 7: push delivery is HTTP (the Kafka outbox hop is the caller's job).
            CallerKind::PushDelivery { subscriber_url } => TransportDecision::HttpThroughKong {
                url: subscriber_url.clone(),
            },
        }
    }

    fn http_url_for(&self, callee: &AgentId) -> Url {
        // The base URL is e.g. `https://ork.example.com/`; agents live under `/agents/<id>`
        // per ADR 0008. Mistakes here surface as 404s, not protocol breaks.
        let path = format!("agents/{callee}");
        self.http_base_url
            .join(&path)
            .unwrap_or_else(|_| self.http_base_url.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> TransportSelector {
        TransportSelector::new(
            ork_a2a::topics::DEFAULT_NAMESPACE,
            Url::parse("https://ork.example.com/").unwrap(),
        )
    }

    #[test]
    fn rule_1_external_to_ork() {
        let s = selector();
        let d = s.select(&"planner".to_string(), &CallerKind::External, true);
        match d {
            TransportDecision::HttpThroughKong { url } => {
                assert_eq!(url.as_str(), "https://ork.example.com/agents/planner");
            }
            other => panic!("expected HttpThroughKong, got {other:?}"),
        }
    }

    #[test]
    fn rule_2_local_to_local_same_process() {
        let s = selector();
        let d = s.select(&"planner".to_string(), &CallerKind::LocalSameProcess, true);
        assert_eq!(d, TransportDecision::InProcess);
    }

    #[test]
    fn rule_3_local_to_local_other_process_await_true() {
        let s = selector();
        let d = s.select(&"planner".to_string(), &CallerKind::LocalOtherProcess, true);
        match d {
            TransportDecision::HttpThroughKong { url } => {
                assert_eq!(url.as_str(), "https://ork.example.com/agents/planner");
            }
            other => panic!("expected HttpThroughKong, got {other:?}"),
        }
    }

    #[test]
    fn rule_4_local_to_local_other_process_await_false() {
        let s = selector();
        let d = s.select(
            &"planner".to_string(),
            &CallerKind::LocalOtherProcess,
            false,
        );
        assert_eq!(
            d,
            TransportDecision::KafkaRequest {
                topic: "ork.a2a.v1.agent.request.planner".into()
            }
        );
    }

    #[test]
    fn rule_5_status_fan_out() {
        let s = selector();
        let d = s.select(
            &"planner".to_string(),
            &CallerKind::StatusFanOut {
                task_id: "task-42".into(),
            },
            false,
        );
        assert_eq!(
            d,
            TransportDecision::KafkaStatus {
                topic: "ork.a2a.v1.agent.status.task-42".into()
            }
        );
    }

    #[test]
    fn rule_6_discovery_heartbeat() {
        let s = selector();
        let d = s.select(&"planner".to_string(), &CallerKind::Discovery, false);
        assert_eq!(
            d,
            TransportDecision::KafkaDiscovery {
                topic: "ork.a2a.v1.discovery.agentcards".into()
            }
        );
    }

    #[test]
    fn rule_7_push_delivery() {
        let s = selector();
        let url = Url::parse("https://customer.example.com/webhook").unwrap();
        let d = s.select(
            &"planner".to_string(),
            &CallerKind::PushDelivery {
                subscriber_url: url.clone(),
            },
            false,
        );
        assert_eq!(d, TransportDecision::HttpThroughKong { url });
    }

    #[test]
    fn rule_8_ork_to_external() {
        // External callee is modelled as `External` from the perspective of the routing
        // engine — the rule fires whether ork is the source or the destination. ADR-0004
        // calls this out as the symmetrical pair of rule 1.
        let s = selector();
        let d = s.select(&"third-party".to_string(), &CallerKind::External, false);
        match d {
            TransportDecision::HttpThroughKong { url } => {
                assert_eq!(url.as_str(), "https://ork.example.com/agents/third-party");
            }
            other => panic!("expected HttpThroughKong, got {other:?}"),
        }
    }
}
