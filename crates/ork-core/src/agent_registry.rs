//! Runtime registry of [`Agent`] implementations and TTL-cached remote agent cards
//! (ADR [`0005`](../../../docs/adrs/0005-agent-card-and-devportal-discovery.md)).
//!
//! The registry has two halves with different semantics:
//!
//! - `local`: in-process `Arc<dyn Agent>` callable directly via [`AgentRegistry::resolve`].
//! - `remote`: TTL-cached `AgentCard`s learned from the discovery topic. The cards expose
//!   the callable URL/Kafka topic via [`TransportHint`]; the actual call is made by an
//!   `A2aRemoteAgent` (ADR-0007, out of scope here).
//!
//! Concurrency: the local map is immutable after construction, so it lives behind a plain
//! `HashMap`. The remote map is written by [`crate::a2a::AgentId`]'s discovery subscriber
//! and read by request handlers, so it lives behind a `tokio::sync::RwLock`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ork_a2a::AgentCard;
use ork_a2a::extensions::{EXT_TRANSPORT_HINT, PARAM_KAFKA_REQUEST_TOPIC};
use tokio::sync::RwLock;
use url::Url;

use crate::a2a::AgentId;
use crate::ports::agent::Agent;

/// Hint to a caller about how to reach a remote agent. Built from the agent's card by
/// inspecting the ork `transport-hint` extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportHint {
    /// Only the spec-mandated HTTP `url` is known.
    HttpOnly { url: Url },
    /// Both HTTP and the Kafka request topic are known.
    HttpAndKafka {
        url: Url,
        kafka_request_topic: String,
    },
    /// Card had no `url` and no transport-hint extension. Caller must look up some other
    /// way (e.g. fall back to a sibling card).
    Unknown,
}

impl TransportHint {
    /// Derive a hint from the card. Looks for the [`EXT_TRANSPORT_HINT`] extension first
    /// and falls back to `card.url`.
    #[must_use]
    pub fn from_card(card: &AgentCard) -> Self {
        let kafka_topic = card.extensions.as_ref().and_then(|exts| {
            exts.iter()
                .find(|e| e.uri == EXT_TRANSPORT_HINT)
                .and_then(|e| e.params.as_ref())
                .and_then(|params| params.get(PARAM_KAFKA_REQUEST_TOPIC))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

        match (card.url.clone(), kafka_topic) {
            (Some(url), Some(kafka_request_topic)) => Self::HttpAndKafka {
                url,
                kafka_request_topic,
            },
            (Some(url), None) => Self::HttpOnly { url },
            (None, _) => Self::Unknown,
        }
    }
}

/// A remote agent's card plus the metadata the registry uses to decide when to evict it.
///
/// `agent` is `Some` once a [`crate::ports::remote_agent_builder::RemoteAgentBuilder`]
/// has materialised a callable [`Agent`] for this card (ADR-0007). The static-config
/// loader in `ork-api` and the Kafka discovery subscriber both populate it; tests can
/// also leave it `None` and exercise the registry as a card-only directory.
#[derive(Clone)]
pub struct RemoteAgentEntry {
    pub card: AgentCard,
    /// When this entry was last refreshed (born / heartbeat / changed).
    pub last_seen: Instant,
    /// Eviction window — typically `3 * discovery_interval` per ADR-0005.
    pub ttl: Duration,
    pub transport_hint: TransportHint,
    /// Optional callable agent built from `card` (ADR-0007). When `None`, callers
    /// must build their own client from the card or fall through to a different
    /// resolver.
    pub agent: Option<Arc<dyn Agent>>,
}

impl std::fmt::Debug for RemoteAgentEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteAgentEntry")
            .field("card", &self.card)
            .field("last_seen", &self.last_seen)
            .field("ttl", &self.ttl)
            .field("transport_hint", &self.transport_hint)
            .field("agent", &self.agent.is_some())
            .finish()
    }
}

impl RemoteAgentEntry {
    /// `true` if `now - last_seen > ttl`.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_seen) > self.ttl
    }
}

/// Local + remote agent registry. Cheap to clone via `Arc`.
pub struct AgentRegistry {
    local: HashMap<AgentId, Arc<dyn Agent>>,
    remote: RwLock<HashMap<AgentId, RemoteAgentEntry>>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            local: HashMap::new(),
            remote: RwLock::new(HashMap::new()),
        }
    }

    /// Build a registry seeded with `agents` as locals; remote map starts empty.
    #[must_use]
    pub fn from_agents(agents: impl IntoIterator<Item = Arc<dyn Agent>>) -> Self {
        let mut local = HashMap::new();
        for a in agents {
            local.insert(a.id().clone(), a);
        }
        Self {
            local,
            remote: RwLock::new(HashMap::new()),
        }
    }

    /// Add or replace a local agent. Intended for boot-time seeding only — request paths
    /// don't mutate locals at runtime.
    pub fn register(&mut self, agent: Arc<dyn Agent>) {
        self.local.insert(agent.id().clone(), agent);
    }

    /// Resolve a callable agent for `id`. Order:
    ///
    /// 1. local in-process map (LocalAgent etc).
    /// 2. remote map's `agent` slot (populated by ADR-0007 builders during static
    ///    config load or Kafka discovery auto-register).
    ///
    /// Returns `None` if `id` is unknown or known only as a card with no callable
    /// client materialised yet.
    pub async fn resolve(&self, id: &AgentId) -> Option<Arc<dyn Agent>> {
        if let Some(a) = self.local.get(id) {
            return Some(a.clone());
        }
        let guard = self.remote.read().await;
        guard.get(id).and_then(|e| e.agent.clone())
    }

    /// All local agent ids. Used by the discovery subscriber to skip self-heartbeats and
    /// by the publisher to spawn one task per local agent.
    #[must_use]
    pub fn local_ids(&self) -> Vec<AgentId> {
        self.local.keys().cloned().collect()
    }

    /// All local agent ids as a fast set, for the subscriber's "is this me?" check.
    #[must_use]
    pub fn local_id_set(&self) -> HashSet<AgentId> {
        self.local.keys().cloned().collect()
    }

    /// Insert or refresh a remote entry under its agent id. Preserves any previously
    /// materialised `agent` slot if the new entry omits it (so a heartbeat that only
    /// carries a fresh card doesn't drop the callable client we already built).
    pub async fn upsert_remote(&self, id: AgentId, entry: RemoteAgentEntry) {
        let mut guard = self.remote.write().await;
        let mut entry = entry;
        if entry.agent.is_none()
            && let Some(prev) = guard.get(&id)
            && let Some(prev_agent) = prev.agent.clone()
        {
            entry.agent = Some(prev_agent);
        }
        guard.insert(id, entry);
    }

    /// Insert or refresh a remote entry along with the callable [`Agent`] built from
    /// its card. Used by the static-config loader (ADR-0007 §`Static config`) and the
    /// Kafka discovery subscriber (ADR-0007 §`Discovery auto-register`) so subsequent
    /// `resolve()` calls return the live client.
    pub async fn upsert_remote_with_agent(
        &self,
        id: AgentId,
        mut entry: RemoteAgentEntry,
        agent: Arc<dyn Agent>,
    ) {
        entry.agent = Some(agent);
        let mut guard = self.remote.write().await;
        guard.insert(id, entry);
    }

    /// Drop a remote entry (tombstone or `died`).
    pub async fn forget_remote(&self, id: &AgentId) {
        let mut guard = self.remote.write().await;
        guard.remove(id);
    }

    /// Drop every remote entry whose `last_seen + ttl < now`. Returns the dropped ids so
    /// the caller can log/observe.
    pub async fn expire_stale(&self, now: Instant) -> Vec<AgentId> {
        let mut guard = self.remote.write().await;
        let stale: Vec<AgentId> = guard
            .iter()
            .filter_map(|(k, v)| {
                if v.is_expired(now) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for k in &stale {
            guard.remove(k);
        }
        stale
    }

    /// Snapshot of every remote entry. Useful for tests and observability.
    pub async fn list_remote(&self) -> Vec<(AgentId, RemoteAgentEntry)> {
        let guard = self.remote.read().await;
        guard.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Look up a remote entry by id (`None` if unknown or evicted).
    pub async fn remote_entry(&self, id: &AgentId) -> Option<RemoteAgentEntry> {
        let guard = self.remote.read().await;
        guard.get(id).cloned()
    }

    /// Union of every known card: local cards (always live) + every non-expired remote
    /// card. This is the surface ADR-0006 will consume to populate delegation tool
    /// descriptions.
    pub async fn list_cards(&self) -> Vec<AgentCard> {
        let mut out: Vec<AgentCard> = self.local.values().map(|a| a.card().clone()).collect();
        let guard = self.remote.read().await;
        out.extend(guard.values().map(|e| e.card.clone()));
        out
    }

    /// Lookup a card by id, preferring local. Returns `None` if neither local nor remote
    /// knows the id.
    pub async fn card_for(&self, id: &AgentId) -> Option<AgentCard> {
        if let Some(a) = self.local.get(id) {
            return Some(a.card().clone());
        }
        let guard = self.remote.read().await;
        guard.get(id).map(|e| e.card.clone())
    }

    /// `true` if `id` is a known local agent **or** a non-evicted remote entry. Used by
    /// the `agent_call` tool and `delegate_to` step (ADR 0006) to map "unknown agent" to
    /// `TaskState::Rejected` instead of crashing inside the resolver.
    pub async fn knows(&self, id: &AgentId) -> bool {
        if self.local.contains_key(id) {
            return true;
        }
        let guard = self.remote.read().await;
        guard.contains_key(id)
    }

    /// Per-peer tool surface for the LLM (ADR 0006 §`LLM tool surface`).
    ///
    /// Walks every known card (local + non-expired remote) and emits one
    /// [`PeerToolDescription`] per `AgentSkill`, plus a generic `agent_call` entry as the
    /// fallback for unstructured cases. ADR 0011 will consume these via a tool-calling
    /// loop; until it lands this function has no in-tree caller, but pinning the shape
    /// here means ADR 0011 needs no registry change.
    pub async fn peer_tool_descriptions(&self) -> Vec<PeerToolDescription> {
        let mut out = Vec::new();
        out.push(PeerToolDescription {
            name: "agent_call".to_string(),
            description: "Delegate work to another agent. Pass `agent` (target id) and `prompt`."
                .to_string(),
            target_agent_id: None,
            skill_id: None,
        });

        for card in self.local.values().map(|a| a.card().clone()) {
            push_skills(&mut out, &card);
        }
        let guard = self.remote.read().await;
        for entry in guard.values() {
            push_skills(&mut out, &entry.card);
        }
        out
    }
}

fn push_skills(out: &mut Vec<PeerToolDescription>, card: &AgentCard) {
    let agent_id = card.name.clone();
    for skill in &card.skills {
        out.push(PeerToolDescription {
            name: format!("peer_{agent_id}_{}", skill.id),
            description: format!("[{agent_id}/{}] {}", skill.name, skill.description),
            target_agent_id: Some(agent_id.clone()),
            skill_id: Some(skill.id.clone()),
        });
    }
}

/// One LLM-facing tool entry derived from an [`AgentCard`] skill (ADR 0006 §`LLM tool
/// surface`). The first entry returned by [`AgentRegistry::peer_tool_descriptions`] is
/// always the generic `agent_call` (with `target_agent_id`/`skill_id` both `None`); the
/// rest are per-skill entries named `peer_<agent_id>_<skill_id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerToolDescription {
    pub name: String,
    pub description: String,
    pub target_agent_id: Option<AgentId>,
    pub skill_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::{AgentCapabilities, AgentExtension, AgentSkill};

    fn sample_card(name: &str, kafka_topic: Option<&str>, url: Option<&str>) -> AgentCard {
        let mut extensions = Vec::new();
        if let Some(t) = kafka_topic {
            let mut params = serde_json::Map::new();
            params.insert(
                PARAM_KAFKA_REQUEST_TOPIC.into(),
                serde_json::Value::String(t.into()),
            );
            extensions.push(AgentExtension {
                uri: EXT_TRANSPORT_HINT.into(),
                description: None,
                params: Some(params),
            });
        }
        AgentCard {
            name: name.into(),
            description: "test".into(),
            version: "0.0.1".into(),
            url: url.map(|u| Url::parse(u).expect("test url")),
            provider: None,
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: false,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "default".into(),
                name: name.into(),
                description: "test".into(),
                tags: vec![],
                examples: vec![],
                input_modes: None,
                output_modes: None,
            }],
            security_schemes: None,
            security: None,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(extensions)
            },
        }
    }

    #[test]
    fn transport_hint_http_and_kafka_when_extension_present() {
        let card = sample_card(
            "planner",
            Some("ork.a2a.v1.agent.request.planner"),
            Some("https://api.example.com/a2a/agents/planner"),
        );
        match TransportHint::from_card(&card) {
            TransportHint::HttpAndKafka {
                url,
                kafka_request_topic,
            } => {
                assert_eq!(url.as_str(), "https://api.example.com/a2a/agents/planner");
                assert_eq!(kafka_request_topic, "ork.a2a.v1.agent.request.planner");
            }
            other => panic!("expected HttpAndKafka, got {other:?}"),
        }
    }

    #[test]
    fn transport_hint_http_only_when_no_extension() {
        let card = sample_card(
            "planner",
            None,
            Some("https://api.example.com/a2a/agents/planner"),
        );
        match TransportHint::from_card(&card) {
            TransportHint::HttpOnly { url } => {
                assert_eq!(url.as_str(), "https://api.example.com/a2a/agents/planner")
            }
            other => panic!("expected HttpOnly, got {other:?}"),
        }
    }

    #[test]
    fn transport_hint_unknown_when_no_url() {
        let card = sample_card("planner", None, None);
        assert_eq!(TransportHint::from_card(&card), TransportHint::Unknown);
    }

    fn entry_for(card: &AgentCard) -> RemoteAgentEntry {
        RemoteAgentEntry {
            card: card.clone(),
            last_seen: Instant::now(),
            ttl: Duration::from_secs(90),
            transport_hint: TransportHint::from_card(card),
            agent: None,
        }
    }

    #[tokio::test]
    async fn upsert_then_forget_remote() {
        let reg = AgentRegistry::new();
        let card = sample_card("planner", None, Some("https://example.com/a"));
        reg.upsert_remote("planner".into(), entry_for(&card)).await;
        assert_eq!(reg.list_remote().await.len(), 1);

        reg.forget_remote(&"planner".to_string()).await;
        assert!(reg.list_remote().await.is_empty());
    }

    #[tokio::test]
    async fn expire_stale_drops_old_entries_only() {
        let reg = AgentRegistry::new();
        let now = Instant::now();
        let fresh = RemoteAgentEntry {
            card: sample_card("fresh", None, Some("https://example.com/f")),
            last_seen: now,
            ttl: Duration::from_secs(60),
            transport_hint: TransportHint::Unknown,
            agent: None,
        };
        let stale = RemoteAgentEntry {
            card: sample_card("stale", None, Some("https://example.com/s")),
            last_seen: now - Duration::from_secs(120),
            ttl: Duration::from_secs(60),
            transport_hint: TransportHint::Unknown,
            agent: None,
        };
        reg.upsert_remote("fresh".into(), fresh).await;
        reg.upsert_remote("stale".into(), stale).await;

        let dropped = reg.expire_stale(now).await;
        assert_eq!(dropped, vec!["stale".to_string()]);
        let remaining: Vec<_> = reg
            .list_remote()
            .await
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(remaining, vec!["fresh".to_string()]);
    }

    #[tokio::test]
    async fn knows_returns_true_for_local_or_remote() {
        let reg = AgentRegistry::new();
        let card = sample_card("planner", None, Some("https://example.com/a"));
        assert!(!reg.knows(&"planner".to_string()).await);
        reg.upsert_remote("planner".into(), entry_for(&card)).await;
        assert!(reg.knows(&"planner".to_string()).await);
        assert!(!reg.knows(&"unknown".to_string()).await);
    }

    #[tokio::test]
    async fn peer_tool_descriptions_includes_generic_plus_per_skill_entries() {
        let reg = AgentRegistry::new();
        let card = sample_card("researcher", None, Some("https://example.com/r"));
        reg.upsert_remote("researcher".into(), entry_for(&card))
            .await;

        let tools = reg.peer_tool_descriptions().await;
        assert_eq!(tools[0].name, "agent_call");
        assert!(tools[0].target_agent_id.is_none());
        assert!(tools[0].skill_id.is_none());

        let skill_entry = tools
            .iter()
            .find(|t| t.name == "peer_researcher_default")
            .expect("per-skill entry present");
        assert_eq!(skill_entry.target_agent_id.as_deref(), Some("researcher"));
        assert_eq!(skill_entry.skill_id.as_deref(), Some("default"));
    }

    #[tokio::test]
    async fn list_cards_returns_local_union_remote() {
        // Two locals via from_agents — but easier: just exercise remote-only.
        let reg = AgentRegistry::new();
        let card_a = sample_card("a", None, Some("https://example.com/a"));
        let card_b = sample_card("b", None, Some("https://example.com/b"));
        reg.upsert_remote("a".into(), entry_for(&card_a)).await;
        reg.upsert_remote("b".into(), entry_for(&card_b)).await;

        let mut names: Vec<String> = reg.list_cards().await.into_iter().map(|c| c.name).collect();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    use crate::a2a::{AgentContext, AgentMessage};
    use crate::ports::agent::{Agent as AgentTrait, AgentEventStream};
    use ork_a2a::TaskId;
    use ork_common::error::OrkError;

    /// Tiny stub agent for resolve()-via-remote tests.
    struct StubAgent {
        id: AgentId,
        card: AgentCard,
    }

    #[async_trait::async_trait]
    impl AgentTrait for StubAgent {
        fn id(&self) -> &AgentId {
            &self.id
        }
        fn card(&self) -> &AgentCard {
            &self.card
        }
        async fn send_stream(
            &self,
            _ctx: AgentContext,
            _msg: AgentMessage,
        ) -> Result<AgentEventStream, OrkError> {
            unreachable!("not exercised by registry tests")
        }
        async fn cancel(&self, _ctx: AgentContext, _task_id: &TaskId) -> Result<(), OrkError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn resolve_finds_remote_agent_when_builder_materialised_one() {
        let reg = AgentRegistry::new();
        let card = sample_card("vendor", None, Some("https://vendor.example.com/a2a"));
        let agent: Arc<dyn AgentTrait> = Arc::new(StubAgent {
            id: "vendor".into(),
            card: card.clone(),
        });
        reg.upsert_remote_with_agent("vendor".into(), entry_for(&card), agent.clone())
            .await;

        let resolved = reg.resolve(&"vendor".to_string()).await;
        assert!(resolved.is_some(), "remote agent should be resolvable");
        assert_eq!(resolved.unwrap().id(), &"vendor".to_string());
    }

    #[tokio::test]
    async fn resolve_returns_none_for_card_only_remote_entry() {
        let reg = AgentRegistry::new();
        let card = sample_card("orphan", None, Some("https://orphan.example.com/a2a"));
        reg.upsert_remote("orphan".into(), entry_for(&card)).await;

        assert!(reg.resolve(&"orphan".to_string()).await.is_none());
        // But the registry still "knows" about it for delegation gating.
        assert!(reg.knows(&"orphan".to_string()).await);
    }

    #[tokio::test]
    async fn upsert_remote_preserves_previously_built_agent() {
        let reg = AgentRegistry::new();
        let card = sample_card("vendor", None, Some("https://vendor.example.com/a2a"));
        let agent: Arc<dyn AgentTrait> = Arc::new(StubAgent {
            id: "vendor".into(),
            card: card.clone(),
        });
        reg.upsert_remote_with_agent("vendor".into(), entry_for(&card), agent.clone())
            .await;

        // Heartbeat path: an upsert with `agent: None` MUST keep the cached client.
        reg.upsert_remote("vendor".into(), entry_for(&card)).await;
        assert!(reg.resolve(&"vendor".to_string()).await.is_some());
    }
}
