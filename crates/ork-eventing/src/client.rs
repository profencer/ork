//! Top-level [`EventingClient`] factory and bundle. Picks the right backend from a
//! [`KafkaConfig`] so callers do not have to know which one is wired up.

use std::sync::Arc;

use ork_common::config::{KafkaAuth, KafkaConfig, KafkaTransport};

use crate::consumer::Consumer;
use crate::error::EventingError;
use crate::in_memory::InMemoryBackend;
use crate::producer::Producer;
use crate::rskafka_backend::RsKafkaBackend;

fn transport_kind(t: &KafkaTransport) -> &'static str {
    match t {
        KafkaTransport::Plaintext => "plaintext",
        KafkaTransport::Tls { .. } => "tls",
    }
}

fn auth_kind(a: &KafkaAuth) -> &'static str {
    match a {
        KafkaAuth::None => "none",
        KafkaAuth::Scram { .. } => "scram",
        KafkaAuth::Oauthbearer { .. } => "oauthbearer",
    }
}

/// A pair of producer + consumer handles, both backed by the same backend instance so
/// in-process round-trips work without cross-backend state.
///
/// Cheap to clone — the inner trait objects are `Arc`-shared.
#[derive(Clone)]
pub struct EventingClient {
    pub producer: Arc<dyn Producer>,
    pub consumer: Arc<dyn Consumer>,
}

impl EventingClient {
    /// Build a client backed entirely by the in-memory broadcast backend. Useful for unit
    /// tests and explicit dev wiring.
    #[must_use]
    pub fn in_memory() -> Self {
        let backend = Arc::new(InMemoryBackend::new());
        Self {
            producer: backend.clone(),
            consumer: backend,
        }
    }
}

/// Construct an [`EventingClient`] from configuration.
///
/// - `cfg.brokers.is_empty()` → [`InMemoryBackend`] (logs at INFO so dev mode is obvious).
/// - otherwise → [`RsKafkaBackend`].
///
/// `env` is the runtime deployment selector
/// ([`ork_common::config::AppConfig::env`]) — see ADR-0020 §`Kafka trust`,
/// where `RsKafkaBackend::connect` enforces that `PLAINTEXT` only ships
/// in `"dev"`. The in-memory branch is unconditionally allowed because no
/// network is involved.
pub async fn build_client(cfg: &KafkaConfig, env: &str) -> Result<EventingClient, EventingError> {
    if cfg.brokers.is_empty() {
        tracing::info!(
            namespace = %cfg.namespace,
            "kafka.brokers is empty; using in-memory eventing backend (ADR-0004 dev mode)"
        );
        return Ok(EventingClient::in_memory());
    }

    tracing::info!(
        brokers = ?cfg.brokers,
        namespace = %cfg.namespace,
        transport = transport_kind(&cfg.transport),
        auth = auth_kind(&cfg.auth),
        "connecting to Kafka via rskafka backend"
    );
    let backend = Arc::new(RsKafkaBackend::connect(cfg, env).await?);
    Ok(EventingClient {
        producer: backend.clone(),
        consumer: backend,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use ork_common::config::KafkaConfig;
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn empty_brokers_yields_in_memory_roundtrip() {
        let cfg = KafkaConfig::default();
        let client = build_client(&cfg, "dev").await.expect("build");

        let mut stream = client.consumer.subscribe("smoke").await.expect("subscribe");
        client
            .producer
            .publish("smoke", None, &[], b"ping")
            .await
            .expect("publish");

        let got = timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("stream error");
        assert_eq!(got.payload, b"ping");
    }

    #[tokio::test]
    async fn in_memory_constructor_shares_backend() {
        let client = EventingClient::in_memory();
        let mut stream = client.consumer.subscribe("t").await.unwrap();
        client.producer.publish("t", None, &[], b"x").await.unwrap();
        let got = timeout(Duration::from_millis(100), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"x");
    }
}
