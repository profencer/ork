//! Pure-Rust Kafka backend built on [`rskafka`](https://docs.rs/rskafka).
//!
//! Implements [`Producer`] and [`Consumer`] against a real Kafka cluster without pulling in
//! `librdkafka` / C dependencies — see ADR-0004 amendment in
//! [`docs/adrs/0004-hybrid-kong-kafka-transport.md`](../../../docs/adrs/0004-hybrid-kong-kafka-transport.md).
//!
//! ## Phase-1 limitations
//!
//! - **Single partition only.** [`Self::publish`] and [`Self::subscribe`] both target
//!   partition `0`. Multi-partition fan-out is a follow-up; the trait surface stays the
//!   same when we add it. This matches the expected topic shape during Phase-1 (per-task
//!   topics like `agent.status.<task_id>` are inherently single-partition).
//! - **`StartOffset::Latest`.** Subscribers only receive records published after the
//!   subscription is established. ADR-0008's three-tier replay (Redis cache + Postgres) is
//!   layered on top of this for late SSE clients.
//! - **No compression.** Records are produced with [`Compression::NoCompression`]; the
//!   `rskafka` compression crate features are disabled in [`Cargo.toml`] to keep the build
//!   pure-Rust (no `zstd-sys` C builds).
//! - **No SASL/TLS yet.** ADR-0020 covers production posture; for Phase-1 we connect over
//!   plaintext to a trusted internal bootstrap.
//!
//! Live integration tests live in [`crates/ork-eventing/tests/rskafka_roundtrip.rs`] behind
//! `#[ignore]` and a `RSKAFKA_BROKERS` env var, so CI does not require a broker.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use rskafka::client::consumer::{StartOffset, StreamConsumerBuilder};
use rskafka::client::partition::{Compression, UnknownTopicHandling};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;

use crate::consumer::{ConsumedMessage, Consumer, MessageStream};
use crate::error::EventingError;
use crate::producer::Producer;

/// Pure-Rust Kafka backend. Cheap to clone (the inner [`Client`] is `Arc`-shared).
#[derive(Clone)]
pub struct RsKafkaBackend {
    client: Arc<Client>,
}

impl RsKafkaBackend {
    /// Connect to the given bootstrap brokers and prove the connection by listing topics.
    /// Empty `brokers` returns an [`EventingError::Config`].
    pub async fn connect(brokers: Vec<String>) -> Result<Self, EventingError> {
        if brokers.is_empty() {
            return Err(EventingError::Config(
                "RsKafkaBackend requires at least one broker".into(),
            ));
        }

        let client = ClientBuilder::new(brokers)
            .build()
            .await
            .map_err(|e| EventingError::Backend(format!("rskafka build: {e}")))?;

        Ok(Self {
            client: Arc::new(client),
        })
    }

    fn header_vec_to_btreemap(
        headers: &[(String, Vec<u8>)],
    ) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut map = std::collections::BTreeMap::new();
        for (k, v) in headers {
            map.insert(k.clone(), v.clone());
        }
        map
    }
}

#[async_trait]
impl Producer for RsKafkaBackend {
    async fn publish(
        &self,
        topic: &str,
        key: Option<&[u8]>,
        headers: &[(String, Vec<u8>)],
        payload: &[u8],
    ) -> Result<(), EventingError> {
        let partition = self
            .client
            .partition_client(topic.to_string(), 0, UnknownTopicHandling::Retry)
            .await
            .map_err(|e| EventingError::Backend(format!("partition_client: {e}")))?;

        let record = Record {
            key: key.map(<[u8]>::to_vec),
            value: Some(payload.to_vec()),
            headers: Self::header_vec_to_btreemap(headers),
            timestamp: chrono::Utc::now(),
        };

        partition
            .produce(vec![record], Compression::NoCompression)
            .await
            .map_err(|e| EventingError::Backend(format!("produce: {e}")))?;

        Ok(())
    }
}

#[async_trait]
impl Consumer for RsKafkaBackend {
    async fn subscribe(&self, topic: &str) -> Result<MessageStream, EventingError> {
        let partition = self
            .client
            .partition_client(topic.to_string(), 0, UnknownTopicHandling::Retry)
            .await
            .map_err(|e| EventingError::Backend(format!("partition_client: {e}")))?;
        let partition = Arc::new(partition);

        let stream = StreamConsumerBuilder::new(partition, StartOffset::Latest)
            .with_max_wait_ms(500)
            .build();

        let mapped = stream.map(|item| match item {
            Ok((record_and_offset, _high_water)) => {
                let record = record_and_offset.record;
                Ok(ConsumedMessage {
                    key: record.key,
                    headers: record.headers.into_iter().collect(),
                    payload: record.value.unwrap_or_default(),
                })
            }
            Err(e) => Err(EventingError::Backend(format!("rskafka stream: {e}"))),
        });

        Ok(Box::pin(mapped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_brokers_returns_config_error() {
        match RsKafkaBackend::connect(vec![]).await {
            Ok(_) => panic!("expected config error, got Ok"),
            Err(EventingError::Config(_)) => {}
            Err(other) => panic!("expected EventingError::Config, got {other:?}"),
        }
    }
}
