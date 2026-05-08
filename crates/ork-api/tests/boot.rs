//! Boot smoke test for the eventing wiring (ADR-0004).
//!
//! Builds an [`AppState`]-shaped fixture using the same code path as `main.rs`, then proves
//! that the producer in the resulting `EventingClient` can publish to a smoke topic and that
//! the matching consumer receives the bytes.
//!
//! We do not stand up the full Postgres-backed `AppState` here — that requires a database
//! and is out of scope for an ADR-0004 smoke. The test exercises the precise call chain
//! `AppConfig::default() → ork_eventing::build_client → publish/subscribe`.

use std::time::Duration;

use futures::StreamExt;
use ork_common::config::AppConfig;
use tokio::time::timeout;

#[tokio::test]
async fn build_client_from_default_config_round_trips() {
    let config = AppConfig::default();
    assert!(
        config.kafka.brokers.is_empty(),
        "default config should select the in-memory backend"
    );

    let client = ork_eventing::build_client(&config.kafka, &config.env)
        .await
        .expect("build_client");

    let topic = ork_a2a::topics::discovery_agentcards(&config.kafka.namespace);
    let mut stream = client.consumer.subscribe(&topic).await.expect("subscribe");

    let payload = br#"{"agent_id":"boot-smoke","ts":0}"#;
    client
        .producer
        .publish(
            &topic,
            Some(b"boot-smoke"),
            &[("ork-a2a-version".to_string(), b"1.0".to_vec())],
            payload,
        )
        .await
        .expect("publish");

    let got = timeout(Duration::from_millis(250), stream.next())
        .await
        .expect("recv timeout")
        .expect("stream closed")
        .expect("backend error");

    assert_eq!(got.payload, payload);
    assert_eq!(got.key.as_deref(), Some(b"boot-smoke".as_ref()));
    assert!(
        got.headers
            .iter()
            .any(|(k, v)| k == "ork-a2a-version" && v == b"1.0")
    );
}
