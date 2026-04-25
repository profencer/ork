//! Per-gateway Kafka discovery (ADR
//! [`0013`](../../../docs/adrs/0013-generic-gateway-abstraction.md)).
//!
//! ## Wire format
//!
//! - Topic: `<ns>.discovery.gatewaycards` ([`ork_a2a::topics::discovery_gatewaycards`]).
//! - Key: publishing gateway id (UTF-8 bytes).
//! - Payload: `serde_json::to_vec(&GatewayCard)`. **Empty payload = tombstone for `died`.**
//! - Same discovery headers as agent cards ([`ork_a2a::headers::ORK_DISCOVERY_EVENT`], etc.).

use std::sync::Arc;
use std::time::Duration;

use ork_a2a::headers::{
    DEFAULT_CONTENT_TYPE, DISCOVERY_EVENT_BORN, DISCOVERY_EVENT_CHANGED, DISCOVERY_EVENT_DIED,
    DISCOVERY_EVENT_HEARTBEAT, ORK_A2A_VERSION, ORK_CONTENT_TYPE, ORK_DISCOVERY_EVENT,
    WIRE_VERSION,
};
use ork_a2a::topics;
use ork_core::ports::gateway::GatewayCard;
use ork_core::ports::gateway::GatewayId;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::error::EventingError;
use crate::producer::Producer;

pub type GatewayCardProvider = Arc<dyn Fn() -> GatewayCard + Send + Sync>;

/// Publishes one gateway's card on the gateway discovery topic.
pub struct GatewayDiscoveryPublisher {
    producer: Arc<dyn Producer>,
    namespace: String,
    interval: Duration,
    gateway_id: GatewayId,
    card_provider: GatewayCardProvider,
}

impl GatewayDiscoveryPublisher {
    #[must_use]
    pub fn new(
        producer: Arc<dyn Producer>,
        namespace: String,
        gateway_id: GatewayId,
        interval: Duration,
        card_provider: GatewayCardProvider,
    ) -> Self {
        Self {
            producer,
            namespace,
            interval,
            gateway_id,
            card_provider,
        }
    }

    /// Run: `born`, then `heartbeat` every `interval`, then `died` tombstone on cancel.
    pub async fn run(self, cancel: CancellationToken) {
        let topic = topics::discovery_gatewaycards(&self.namespace);

        if let Err(e) = self.publish(&topic, DISCOVERY_EVENT_BORN, false).await {
            tracing::warn!(error = %e, gateway_id = %self.gateway_id, "gateway discovery: born publish failed");
        }

        let mut ticker = interval(self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    tracing::info!(gateway_id = %self.gateway_id, "gateway discovery: publishing died tombstone");
                    if let Err(e) = self.publish_tombstone(&topic).await {
                        tracing::warn!(error = %e, gateway_id = %self.gateway_id, "gateway discovery: died publish failed");
                    }
                    return;
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.publish(&topic, DISCOVERY_EVENT_HEARTBEAT, false).await {
                        tracing::warn!(error = %e, gateway_id = %self.gateway_id, "gateway discovery: heartbeat publish failed");
                    }
                }
            }
        }
    }

    /// Publish a `changed` event immediately.
    pub async fn publish_change(&self) -> Result<(), EventingError> {
        let topic = topics::discovery_gatewaycards(&self.namespace);
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
            .publish(topic, Some(self.gateway_id.as_bytes()), &headers, &payload)
            .await
    }

    async fn publish_tombstone(&self, topic: &str) -> Result<(), EventingError> {
        self.publish(topic, DISCOVERY_EVENT_DIED, true).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ork_a2a::headers::ORK_DISCOVERY_EVENT;
    use ork_a2a::topics;
    use tokio::time::timeout;

    use super::*;
    use crate::consumer::ConsumedMessage;
    use crate::consumer::Consumer;
    use crate::in_memory::InMemoryBackend;
    use crate::producer::Producer;
    use futures::StreamExt;

    fn sample_card(name: &str) -> GatewayCard {
        GatewayCard {
            id: name.into(),
            gateway_type: "rest".into(),
            name: name.into(),
            description: "test".into(),
            version: "0.0.1".into(),
            endpoint: None,
            capabilities: vec!["invoke".into()],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            extensions: vec![],
        }
    }

    #[tokio::test]
    async fn gateway_publisher_emits_born_heartbeat_died_on_cancel() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let mut stream = consumer
            .subscribe(&topics::discovery_gatewaycards("ork.a2a.v1"))
            .await
            .unwrap();

        let card = sample_card("gw-one");
        let card_provider: GatewayCardProvider = Arc::new(move || card.clone());
        let cancel = CancellationToken::new();
        let publisher = GatewayDiscoveryPublisher::new(
            producer,
            "ork.a2a.v1".into(),
            "gw-one".into(),
            Duration::from_millis(40),
            card_provider,
        );

        let h = tokio::spawn({
            let c = cancel.clone();
            async move { publisher.run(c).await }
        });

        let collect_event = |msg: &ConsumedMessage| -> String {
            msg.headers
                .iter()
                .find(|(k, _)| k == ORK_DISCOVERY_EVENT)
                .map(|(_, v)| String::from_utf8_lossy(v).to_string())
                .unwrap_or_default()
        };

        let m1 = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(collect_event(&m1), DISCOVERY_EVENT_BORN);
        assert!(!m1.payload.is_empty());
        assert_eq!(m1.key.as_deref(), Some(b"gw-one".as_ref()));

        let m2 = timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(collect_event(&m2), DISCOVERY_EVENT_HEARTBEAT);
        assert!(!m2.payload.is_empty());

        cancel.cancel();
        let m3 = timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(collect_event(&m3), DISCOVERY_EVENT_DIED);
        assert!(m3.payload.is_empty());

        h.await.unwrap();
    }

    #[tokio::test]
    async fn publish_change_emits_changed_event() {
        let backend = InMemoryBackend::new();
        let producer: Arc<dyn Producer> = Arc::new(backend.clone());
        let consumer: Arc<dyn Consumer> = Arc::new(backend.clone());

        let mut stream = consumer
            .subscribe(&topics::discovery_gatewaycards("ork.a2a.v1"))
            .await
            .unwrap();

        let card = sample_card("g2");
        let card_for_provider = Arc::new(std::sync::Mutex::new(card));
        let card_for_provider2 = card_for_provider.clone();
        let card_provider: GatewayCardProvider =
            Arc::new(move || card_for_provider2.lock().expect("lock").clone());
        let publisher = GatewayDiscoveryPublisher::new(
            producer,
            "ork.a2a.v1".into(),
            "g2".into(),
            Duration::from_secs(60),
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
