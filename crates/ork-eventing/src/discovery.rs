//! Per-agent Kafka discovery (ADR
//! [`0005`](../../../docs/adrs/0005-agent-card-and-devportal-discovery.md)).
//!
//! ## Wire format
//!
//! - Topic: `<ns>.discovery.agentcards` (helper [`ork_a2a::topics::discovery_agentcards`]).
//! - Key: the publishing agent's id (UTF-8 bytes).
//! - Payload: `serde_json::to_vec(&AgentCard)`. **An empty payload is a tombstone for `died`.**
//! - Headers: [`ork_a2a::headers::ORK_A2A_VERSION`], [`ork_a2a::headers::ORK_DISCOVERY_EVENT`]
//!   (one of `born | heartbeat | changed | died`), [`ork_a2a::headers::ORK_CONTENT_TYPE`].
//!
//! ## Lifecycle
//!
//! [`DiscoveryPublisher`] runs one task per local agent. On boot it publishes `born`, then
//! `heartbeat` every `interval`, then `died` (with an empty-payload tombstone) on cancel.
//! [`DiscoveryPublisher::publish_change`] is the ADR-0014 hook for plugin-driven card
//! changes.
//!
//! [`DiscoverySubscriber`] runs one task per process. Every record updates / removes a
//! [`ork_core::agent_registry::RemoteAgentEntry`]. Self-id heartbeats are ignored.
//! Decode errors are logged and the stream is **not** poisoned.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use ork_a2a::AgentCard;
use ork_a2a::headers::{
    DEFAULT_CONTENT_TYPE, DISCOVERY_EVENT_BORN, DISCOVERY_EVENT_CHANGED, DISCOVERY_EVENT_DIED,
    DISCOVERY_EVENT_HEARTBEAT, ORK_A2A_VERSION, ORK_CONTENT_TYPE, ORK_DISCOVERY_EVENT,
    WIRE_VERSION,
};
use ork_a2a::topics;
use ork_core::a2a::AgentId;
use ork_core::agent_registry::{AgentRegistry, RemoteAgentEntry, TransportHint};
use ork_core::ports::remote_agent_builder::RemoteAgentBuilder;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::consumer::{ConsumedMessage, Consumer};
use crate::error::EventingError;
use crate::producer::Producer;

/// Async closure-style provider so the publisher can re-read a card every tick (lets
/// ADR-0014 plugins mutate the card without restarting the publisher).
pub type CardProvider = Arc<dyn Fn() -> AgentCard + Send + Sync>;

/// Publishes one local agent's card on the discovery topic.
pub struct DiscoveryPublisher {
    producer: Arc<dyn Producer>,
    namespace: String,
    interval: Duration,
    agent_id: AgentId,
    card_provider: CardProvider,
}

impl DiscoveryPublisher {
    #[must_use]
    pub fn new(
        producer: Arc<dyn Producer>,
        namespace: String,
        agent_id: AgentId,
        interval: Duration,
        card_provider: CardProvider,
    ) -> Self {
        Self {
            producer,
            namespace,
            interval,
            agent_id,
            card_provider,
        }
    }

    /// Run the lifecycle: publish `born`, then a `heartbeat` every `interval`, then a
    /// `died` tombstone when `cancel` fires. Errors are logged at WARN and never break the
    /// loop — a transient backend failure must not take the API down.
    pub async fn run(self, cancel: CancellationToken) {
        let topic = topics::discovery_agentcards(&self.namespace);

        if let Err(e) = self.publish(&topic, DISCOVERY_EVENT_BORN, false).await {
            tracing::warn!(error = %e, agent_id = %self.agent_id, "discovery: born publish failed");
        }

        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate first tick — we just published `born`.
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    tracing::info!(agent_id = %self.agent_id, "discovery: publishing died tombstone");
                    if let Err(e) = self.publish_tombstone(&topic).await {
                        tracing::warn!(error = %e, agent_id = %self.agent_id, "discovery: died publish failed");
                    }
                    return;
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.publish(&topic, DISCOVERY_EVENT_HEARTBEAT, false).await {
                        tracing::warn!(error = %e, agent_id = %self.agent_id, "discovery: heartbeat publish failed");
                    }
                }
            }
        }
    }

    /// Publish a `changed` event immediately (e.g. plugin loaded a new tool, ADR-0014).
    pub async fn publish_change(&self) -> Result<(), EventingError> {
        let topic = topics::discovery_agentcards(&self.namespace);
        self.publish(&topic, DISCOVERY_EVENT_CHANGED, false).await
    }

    async fn publish(
        &self,
        topic: &str,
        event: &str,
        force_empty: bool,
    ) -> Result<(), EventingError> {
        let payload = if force_empty {
            Vec::new()
        } else {
            let card = (self.card_provider)();
            serde_json::to_vec(&card)?
        };
        let headers = vec![
            (
                ORK_A2A_VERSION.to_string(),
                WIRE_VERSION.as_bytes().to_vec(),
            ),
            (ORK_DISCOVERY_EVENT.to_string(), event.as_bytes().to_vec()),
            (
                ORK_CONTENT_TYPE.to_string(),
                DEFAULT_CONTENT_TYPE.as_bytes().to_vec(),
            ),
        ];
        self.producer
            .publish(topic, Some(self.agent_id.as_bytes()), &headers, &payload)
            .await
    }

    async fn publish_tombstone(&self, topic: &str) -> Result<(), EventingError> {
        self.publish(topic, DISCOVERY_EVENT_DIED, true).await
    }
}

/// Consume the discovery topic and apply every record to the shared `AgentRegistry`.
///
/// One per process. Self-id heartbeats are ignored (cards we publish ourselves are already
/// represented by the local registry). Decode errors are logged and the stream continues —
/// a corrupt payload from one peer must not blind us to every other peer.
pub struct DiscoverySubscriber {
    consumer: Arc<dyn Consumer>,
    namespace: String,
    registry: Arc<AgentRegistry>,
    local_ids: HashSet<AgentId>,
    /// Used to compute `ttl = ttl_multiplier * discovery_interval` per ADR-0005.
    discovery_interval: Duration,
    ttl_multiplier: u32,
    /// ADR-0007: when set, the subscriber materialises every newly-discovered
    /// peer card into a callable [`ork_core::ports::agent::Agent`] and
    /// registers it via [`AgentRegistry::upsert_remote_with_agent`]. When `None`
    /// (Phase-1 / tests) we fall back to the card-only path so peers remain
    /// browsable but not invocable.
    remote_builder: Option<Arc<dyn RemoteAgentBuilder>>,
}

impl DiscoverySubscriber {
    #[must_use]
    pub fn new(
        consumer: Arc<dyn Consumer>,
        namespace: String,
        registry: Arc<AgentRegistry>,
        local_ids: HashSet<AgentId>,
        discovery_interval: Duration,
        ttl_multiplier: u32,
    ) -> Self {
        Self {
            consumer,
            namespace,
            registry,
            local_ids,
            discovery_interval,
            ttl_multiplier,
            remote_builder: None,
        }
    }

    /// Wire a [`RemoteAgentBuilder`] so every born/heartbeat/changed event for
    /// an unknown peer materialises a callable agent. ADR-0007 §3.
    #[must_use]
    pub fn with_remote_builder(mut self, builder: Arc<dyn RemoteAgentBuilder>) -> Self {
        self.remote_builder = Some(builder);
        self
    }

    /// Block until `cancel` fires or the upstream stream ends.
    pub async fn run(self, cancel: CancellationToken) -> Result<(), EventingError> {
        let topic = topics::discovery_agentcards(&self.namespace);
        let mut stream = self.consumer.subscribe(&topic).await?;
        let ttl = self.discovery_interval.saturating_mul(self.ttl_multiplier);

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    tracing::info!(topic = %topic, "discovery: subscriber exiting");
                    return Ok(());
                }
                next = stream.next() => {
                    match next {
                        Some(Ok(msg)) => self.handle(msg, ttl).await,
                        Some(Err(e)) => {
                            // Backend hiccup; keep going. A real Kafka driver should be
                            // resilient on its own, but we log so it shows up in audits.
                            tracing::warn!(error = %e, topic = %topic, "discovery: stream error");
                        }
                        None => {
                            tracing::info!(topic = %topic, "discovery: stream ended");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    async fn handle(&self, msg: ConsumedMessage, ttl: Duration) {
        let agent_id = match msg.key.as_deref().and_then(|k| std::str::from_utf8(k).ok()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                tracing::warn!("discovery: dropped record with missing/non-utf8 key");
                return;
            }
        };

        if self.local_ids.contains(&agent_id) {
            // Skip our own heartbeats — we already represent these in the local map.
            return;
        }

        let event = msg
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(ORK_DISCOVERY_EVENT))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
            .unwrap_or("");

        if event == DISCOVERY_EVENT_DIED || msg.payload.is_empty() {
            self.registry.forget_remote(&agent_id).await;
            return;
        }

        let card: AgentCard = match serde_json::from_slice(&msg.payload) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, agent = %agent_id, "discovery: malformed card payload");
                return;
            }
        };

        let mut entry = RemoteAgentEntry {
            transport_hint: TransportHint::from_card(&card),
            card: card.clone(),
            last_seen: Instant::now(),
            ttl,
            agent: None,
        };

        // ADR-0007: if a builder is wired AND we haven't already materialised an
        // agent for this id, build one from the card so the registry resolves to
        // a callable A2aRemoteAgent. Existing agents are preserved by `upsert_remote`,
        // so we only build when nothing is registered yet — this avoids rebuilding
        // an agent on every heartbeat (and discarding the warmed token cache).
        if let Some(builder) = &self.remote_builder
            && self
                .registry
                .remote_entry(&agent_id)
                .await
                .is_none_or(|e| e.agent.is_none())
        {
            match builder.build(card).await {
                Ok(agent) => {
                    entry.agent = Some(agent.clone());
                    self.registry
                        .upsert_remote_with_agent(agent_id, entry, agent)
                        .await;
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, agent = %agent_id, "discovery: failed to materialise remote agent; falling back to card-only entry");
                }
            }
        }

        self.registry.upsert_remote(agent_id, entry).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::extensions::{EXT_TRANSPORT_HINT, PARAM_KAFKA_REQUEST_TOPIC};
    use ork_a2a::{AgentCapabilities, AgentExtension, AgentSkill};
    use std::sync::Mutex;
    use tokio::time::timeout;

    use crate::in_memory::InMemoryBackend;

    fn sample_card(name: &str) -> AgentCard {
        let mut params = serde_json::Map::new();
        params.insert(
            PARAM_KAFKA_REQUEST_TOPIC.into(),
            serde_json::Value::String(format!("ork.a2a.v1.agent.request.{name}")),
        );
        AgentCard {
            name: name.into(),
            description: "test".into(),
            version: "0.0.1".into(),
            url: Some(
                url::Url::parse(&format!("https://api.example.com/a2a/agents/{name}")).unwrap(),
            ),
            provider: None,
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: false,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: format!("{name}-default"),
                name: name.into(),
                description: "test".into(),
                tags: vec![],
                examples: vec![],
                input_modes: None,
                output_modes: None,
            }],
            security_schemes: None,
            security: None,
            extensions: Some(vec![AgentExtension {
                uri: EXT_TRANSPORT_HINT.into(),
                description: None,
                params: Some(params),
            }]),
        }
    }

    #[tokio::test]
    async fn publisher_emits_born_then_heartbeat_then_died_on_cancel() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let mut stream = consumer
            .subscribe(&topics::discovery_agentcards("ork.a2a.v1"))
            .await
            .unwrap();

        let card = sample_card("planner");
        let card_provider: CardProvider = Arc::new(move || card.clone());
        let cancel = CancellationToken::new();
        let publisher = DiscoveryPublisher::new(
            producer,
            "ork.a2a.v1".into(),
            "planner".into(),
            Duration::from_millis(40),
            card_provider,
        );

        let cancel_for_task = cancel.clone();
        let h = tokio::spawn(async move { publisher.run(cancel_for_task).await });

        let collect_event = |msg: &ConsumedMessage| -> String {
            msg.headers
                .iter()
                .find(|(k, _)| k == ORK_DISCOVERY_EVENT)
                .map(|(_, v)| String::from_utf8_lossy(v).to_string())
                .unwrap_or_default()
        };

        // born
        let m1 = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(collect_event(&m1), DISCOVERY_EVENT_BORN);
        assert!(!m1.payload.is_empty(), "born must carry the card body");
        assert_eq!(m1.key.as_deref(), Some(b"planner".as_ref()));

        // at least one heartbeat
        let m2 = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(collect_event(&m2), DISCOVERY_EVENT_HEARTBEAT);
        assert!(!m2.payload.is_empty());

        cancel.cancel();
        // died (tombstone)
        let m3 = timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(collect_event(&m3), DISCOVERY_EVENT_DIED);
        assert!(
            m3.payload.is_empty(),
            "died is a tombstone with empty payload"
        );

        h.await.unwrap();
    }

    #[tokio::test]
    async fn subscriber_upserts_then_removes_on_tombstone() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let registry = Arc::new(AgentRegistry::new());
        let cancel = CancellationToken::new();

        let sub = DiscoverySubscriber::new(
            consumer.clone(),
            "ork.a2a.v1".into(),
            registry.clone(),
            HashSet::new(),
            Duration::from_secs(30),
            3,
        );
        let cancel_for_sub = cancel.clone();
        let sub_handle = tokio::spawn(async move { sub.run(cancel_for_sub).await });

        // Subscriber must subscribe before publishes; in_memory is not replaying.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let card = sample_card("writer");
        let card_provider: CardProvider = Arc::new(move || card.clone());
        let pub_cancel = CancellationToken::new();
        let publisher = DiscoveryPublisher::new(
            producer.clone(),
            "ork.a2a.v1".into(),
            "writer".into(),
            Duration::from_millis(40),
            card_provider,
        );
        let pub_cancel_for_task = pub_cancel.clone();
        let pub_handle = tokio::spawn(async move { publisher.run(pub_cancel_for_task).await });

        // Wait for at least one upsert to land.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if registry.remote_entry(&"writer".to_string()).await.is_some() {
                break;
            }
        }
        let entry = registry
            .remote_entry(&"writer".to_string())
            .await
            .expect("writer card upserted");
        assert_eq!(entry.card.name, "writer");
        match &entry.transport_hint {
            TransportHint::HttpAndKafka {
                url,
                kafka_request_topic,
            } => {
                assert_eq!(url.as_str(), "https://api.example.com/a2a/agents/writer");
                assert_eq!(kafka_request_topic, "ork.a2a.v1.agent.request.writer");
            }
            other => panic!("expected HttpAndKafka, got {other:?}"),
        }

        // Tombstone the publisher; subscriber should drop the entry.
        pub_cancel.cancel();
        let _ = timeout(Duration::from_millis(300), pub_handle).await;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if registry.remote_entry(&"writer".to_string()).await.is_none() {
                break;
            }
        }
        assert!(
            registry.remote_entry(&"writer".to_string()).await.is_none(),
            "tombstone must remove the remote entry"
        );

        cancel.cancel();
        let _ = timeout(Duration::from_millis(200), sub_handle).await;
    }

    #[tokio::test]
    async fn subscriber_ignores_self_heartbeats() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let registry = Arc::new(AgentRegistry::new());
        let cancel = CancellationToken::new();
        let mut local_ids = HashSet::new();
        local_ids.insert("planner".to_string());

        let sub = DiscoverySubscriber::new(
            consumer,
            "ork.a2a.v1".into(),
            registry.clone(),
            local_ids,
            Duration::from_secs(30),
            3,
        );
        let cancel_for_sub = cancel.clone();
        let sub_handle = tokio::spawn(async move { sub.run(cancel_for_sub).await });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let card = sample_card("planner");
        let card_provider: CardProvider = Arc::new(move || card.clone());
        let pub_cancel = CancellationToken::new();
        let publisher = DiscoveryPublisher::new(
            producer,
            "ork.a2a.v1".into(),
            "planner".into(),
            Duration::from_millis(40),
            card_provider,
        );
        let pub_cancel_for_task = pub_cancel.clone();
        let pub_handle = tokio::spawn(async move { publisher.run(pub_cancel_for_task).await });

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            registry
                .remote_entry(&"planner".to_string())
                .await
                .is_none(),
            "self heartbeats must not populate remote map"
        );

        pub_cancel.cancel();
        cancel.cancel();
        let _ = timeout(Duration::from_millis(200), pub_handle).await;
        let _ = timeout(Duration::from_millis(200), sub_handle).await;
    }

    #[tokio::test]
    async fn subscriber_tolerates_malformed_payloads() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let registry = Arc::new(AgentRegistry::new());
        let cancel = CancellationToken::new();

        let sub = DiscoverySubscriber::new(
            consumer.clone(),
            "ork.a2a.v1".into(),
            registry.clone(),
            HashSet::new(),
            Duration::from_secs(30),
            3,
        );
        let cancel_for_sub = cancel.clone();
        let sub_handle = tokio::spawn(async move { sub.run(cancel_for_sub).await });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let topic = topics::discovery_agentcards("ork.a2a.v1");
        let headers = vec![(
            ORK_DISCOVERY_EVENT.to_string(),
            DISCOVERY_EVENT_HEARTBEAT.as_bytes().to_vec(),
        )];
        // Garbage payload — must be tolerated.
        producer
            .publish(&topic, Some(b"junk"), &headers, b"NOT JSON")
            .await
            .unwrap();
        // Valid payload immediately after — must still land.
        let card = sample_card("planner");
        let card_bytes = serde_json::to_vec(&card).unwrap();
        producer
            .publish(&topic, Some(b"planner"), &headers, &card_bytes)
            .await
            .unwrap();

        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if registry
                .remote_entry(&"planner".to_string())
                .await
                .is_some()
            {
                break;
            }
        }
        assert!(
            registry
                .remote_entry(&"planner".to_string())
                .await
                .is_some(),
            "subscriber kept consuming after malformed payload"
        );

        cancel.cancel();
        let _ = timeout(Duration::from_millis(200), sub_handle).await;
    }

    /// Stub builder used by [`subscriber_auto_registers_remote_agents`] to count
    /// build invocations and produce a callable agent without doing real I/O.
    struct StubBuilder {
        built: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct StubAgent {
        id: AgentId,
        card: AgentCard,
    }

    #[async_trait::async_trait]
    impl ork_core::ports::agent::Agent for StubAgent {
        fn id(&self) -> &AgentId {
            &self.id
        }
        fn card(&self) -> &AgentCard {
            &self.card
        }
        async fn send_stream(
            &self,
            _ctx: ork_core::a2a::AgentContext,
            _msg: ork_core::a2a::AgentMessage,
        ) -> Result<ork_core::ports::agent::AgentEventStream, ork_common::error::OrkError> {
            unimplemented!("not used in this test")
        }
    }

    #[async_trait::async_trait]
    impl RemoteAgentBuilder for StubBuilder {
        async fn build(
            &self,
            card: AgentCard,
        ) -> Result<Arc<dyn ork_core::ports::agent::Agent>, ork_common::error::OrkError> {
            self.built
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Arc::new(StubAgent {
                id: card.name.clone(),
                card,
            }))
        }
    }

    #[tokio::test]
    async fn subscriber_auto_registers_remote_agents_when_builder_present() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let registry = Arc::new(AgentRegistry::new());
        let cancel = CancellationToken::new();
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let builder: Arc<dyn RemoteAgentBuilder> = Arc::new(StubBuilder {
            built: built.clone(),
        });

        let sub = DiscoverySubscriber::new(
            consumer.clone(),
            "ork.a2a.v1".into(),
            registry.clone(),
            HashSet::new(),
            Duration::from_secs(30),
            3,
        )
        .with_remote_builder(builder);
        let cancel_for_sub = cancel.clone();
        let sub_handle = tokio::spawn(async move { sub.run(cancel_for_sub).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let topic = topics::discovery_agentcards("ork.a2a.v1");
        let card = sample_card("vendor");
        let card_bytes = serde_json::to_vec(&card).unwrap();
        let headers = vec![(
            ORK_DISCOVERY_EVENT.to_string(),
            DISCOVERY_EVENT_BORN.as_bytes().to_vec(),
        )];
        producer
            .publish(&topic, Some(b"vendor"), &headers, &card_bytes)
            .await
            .unwrap();

        // Wait for entry + agent to appear.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(entry) = registry.remote_entry(&"vendor".to_string()).await
                && entry.agent.is_some()
            {
                break;
            }
        }
        let entry = registry
            .remote_entry(&"vendor".to_string())
            .await
            .expect("entry upserted");
        assert!(
            entry.agent.is_some(),
            "builder should have materialised a callable Agent"
        );
        let resolved = registry
            .resolve(&"vendor".to_string())
            .await
            .expect("registry should resolve to the built agent");
        assert_eq!(resolved.id(), &"vendor".to_string());

        // A second heartbeat must NOT rebuild the agent (token caches stay warm).
        let headers_hb = vec![(
            ORK_DISCOVERY_EVENT.to_string(),
            DISCOVERY_EVENT_HEARTBEAT.as_bytes().to_vec(),
        )];
        producer
            .publish(&topic, Some(b"vendor"), &headers_hb, &card_bytes)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            built.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "builder must be invoked at most once per id"
        );

        cancel.cancel();
        let _ = timeout(Duration::from_millis(200), sub_handle).await;
    }

    // Used to make sure the publish_change hook is callable. (Mutex used to confirm only
    // one thread of execution publishes a change.)
    #[tokio::test]
    async fn publish_change_emits_changed_event() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let mut stream = consumer
            .subscribe(&topics::discovery_agentcards("ork.a2a.v1"))
            .await
            .unwrap();

        let card = Arc::new(Mutex::new(sample_card("planner")));
        let card_for_provider = card.clone();
        let card_provider: CardProvider =
            Arc::new(move || card_for_provider.lock().unwrap().clone());
        let publisher = DiscoveryPublisher::new(
            producer,
            "ork.a2a.v1".into(),
            "planner".into(),
            Duration::from_secs(60), // long enough that no heartbeat fires
            card_provider,
        );

        publisher.publish_change().await.unwrap();

        let m = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let evt = m
            .headers
            .iter()
            .find(|(k, _)| k == ORK_DISCOVERY_EVENT)
            .map(|(_, v)| String::from_utf8_lossy(v).to_string())
            .unwrap();
        assert_eq!(evt, DISCOVERY_EVENT_CHANGED);
    }
}
