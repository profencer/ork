//! ADR-0005 integration test: two publishers + one subscriber over `InMemoryBackend`.
//!
//! Verifies the wire-format and lifecycle pieces in the `crates/ork-api/src/main.rs` wiring:
//!
//! - Each publisher's `born`/`heartbeat` records land in the registry as remote entries.
//! - Cancelling a publisher fires `died`; the subscriber removes that entry.
//! - The TTL sweep helper drops entries whose `last_seen + ttl < now`.
//!
//! Uses very short intervals so the test runs quickly.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ork_a2a::{AgentCapabilities, AgentCard, AgentSkill, topics};
use ork_core::a2a::card_builder::{CardEnrichmentContext, build_local_card};
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::agent::AgentConfig;
use ork_eventing::{
    Consumer, EventingClient, Producer,
    discovery::{CardProvider, DiscoveryPublisher, DiscoverySubscriber},
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn cfg(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.into(),
        name: format!("{id} agent"),
        description: "test".into(),
        system_prompt: "sys".into(),
        tools: vec![],
        provider: None,
        model: None,
        temperature: 0.0,
        max_tokens: 100,
        max_tool_iterations: ork_core::models::agent::default_max_tool_iterations(),
        max_parallel_tool_calls: ork_core::models::agent::default_max_parallel_tool_calls(),
        max_tool_result_bytes: ork_core::models::agent::default_max_tool_result_bytes(),
        expose_reasoning: false,
    }
}

#[tokio::test]
async fn two_publishers_one_subscriber_roundtrip_and_tombstone() {
    let client = EventingClient::in_memory();
    let producer: Arc<dyn Producer> = client.producer.clone();
    let consumer: Arc<dyn Consumer> = client.consumer.clone();

    let registry = Arc::new(AgentRegistry::new());
    let cancel_sub = CancellationToken::new();

    // Subscribe before any publisher starts so InMemory can deliver.
    let sub = DiscoverySubscriber::new(
        consumer.clone(),
        topics::DEFAULT_NAMESPACE.into(),
        registry.clone(),
        HashSet::new(),
        Duration::from_millis(100),
        3,
    );
    let cancel_sub_for_task = cancel_sub.clone();
    let sub_handle = tokio::spawn(async move { sub.run(cancel_sub_for_task).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ctx = CardEnrichmentContext::minimal();
    let card_planner = build_local_card(&cfg("planner"), &ctx);
    let card_writer = build_local_card(&cfg("writer"), &ctx);

    let pub_a_cancel = CancellationToken::new();
    let pub_b_cancel = CancellationToken::new();

    let pub_a = DiscoveryPublisher::new(
        producer.clone(),
        topics::DEFAULT_NAMESPACE.into(),
        "planner".into(),
        Duration::from_millis(40),
        Arc::new(move || card_planner.clone()) as CardProvider,
    );
    let pub_b = DiscoveryPublisher::new(
        producer.clone(),
        topics::DEFAULT_NAMESPACE.into(),
        "writer".into(),
        Duration::from_millis(40),
        Arc::new(move || card_writer.clone()) as CardProvider,
    );

    let pub_a_cancel_clone = pub_a_cancel.clone();
    let pub_b_cancel_clone = pub_b_cancel.clone();
    let h_a = tokio::spawn(async move { pub_a.run(pub_a_cancel_clone).await });
    let h_b = tokio::spawn(async move { pub_b.run(pub_b_cancel_clone).await });

    // Wait for both upserts.
    let mut both_seen = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let names: HashSet<String> = registry
            .list_remote()
            .await
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        if names.contains("planner") && names.contains("writer") {
            both_seen = true;
            break;
        }
    }
    assert!(both_seen, "expected both planner and writer in remote map");

    // Both cards exposed via list_cards (local is empty here).
    let cards: Vec<String> = registry
        .list_cards()
        .await
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(cards.iter().any(|n| n.contains("planner")));
    assert!(cards.iter().any(|n| n.contains("writer")));

    // Tombstone planner; subscriber removes it.
    pub_a_cancel.cancel();
    let _ = timeout(Duration::from_millis(400), h_a).await;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if registry
            .remote_entry(&"planner".to_string())
            .await
            .is_none()
        {
            break;
        }
    }
    assert!(
        registry
            .remote_entry(&"planner".to_string())
            .await
            .is_none(),
        "planner should be gone after `died`"
    );
    assert!(
        registry.remote_entry(&"writer".to_string()).await.is_some(),
        "writer should still be present"
    );

    // Drop the writer publisher *without cancel* so no tombstone fires; rely on TTL sweep.
    h_b.abort();

    // Force-age the writer entry past TTL by sweeping with `now + huge`.
    let dropped = registry
        .expire_stale(Instant::now() + Duration::from_secs(3600))
        .await;
    assert!(dropped.contains(&"writer".to_string()));
    assert!(registry.list_remote().await.is_empty());

    cancel_sub.cancel();
    let _ = timeout(Duration::from_millis(200), sub_handle).await;
    pub_b_cancel.cancel();
}

#[tokio::test]
async fn ttl_sweep_drops_only_expired_entries() {
    let registry = AgentRegistry::new();
    let now = Instant::now();
    let ttl = Duration::from_millis(100);

    let card = AgentCard {
        name: "fresh".into(),
        description: "t".into(),
        version: "0".into(),
        url: None,
        provider: None,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "fresh-default".into(),
            name: "fresh".into(),
            description: "t".into(),
            tags: vec![],
            examples: vec![],
            input_modes: None,
            output_modes: None,
        }],
        security_schemes: None,
        security: None,
        extensions: None,
    };

    registry
        .upsert_remote(
            "fresh".into(),
            ork_core::agent_registry::RemoteAgentEntry {
                card: card.clone(),
                last_seen: now,
                ttl,
                transport_hint: ork_core::agent_registry::TransportHint::Unknown,
                agent: None,
            },
        )
        .await;
    registry
        .upsert_remote(
            "stale".into(),
            ork_core::agent_registry::RemoteAgentEntry {
                card,
                last_seen: now - Duration::from_secs(10),
                ttl,
                transport_hint: ork_core::agent_registry::TransportHint::Unknown,
                agent: None,
            },
        )
        .await;

    let dropped = registry.expire_stale(now).await;
    assert_eq!(dropped, vec!["stale".to_string()]);
    let remaining: Vec<_> = registry
        .list_remote()
        .await
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(remaining, vec!["fresh".to_string()]);
}
