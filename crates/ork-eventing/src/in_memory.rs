//! In-process [`Producer`] + [`Consumer`] backed by `tokio::sync::broadcast`, one channel
//! per topic, lazily created.
//!
//! ## Semantics
//!
//! - **Publish-then-subscribe is lossy.** A subscriber attached *after* a publish receives
//!   nothing for that publish. This matches Kafka's "no offset before subscription" default
//!   when `auto.offset.reset = latest`. Tests that need the opposite must subscribe first.
//! - **Fan-out is N:M.** Every subscriber to a topic gets every record published after it
//!   subscribed.
//! - **Topics are isolated.** Names are exact-match strings; no wildcards.
//! - **Capacity is fixed at 1024 records per topic.** Slow subscribers see
//!   [`broadcast::error::RecvError::Lagged`] surfaced as [`EventingError::Backend`].
//!
//! Used for unit tests and as the default dev-mode backend when no Kafka brokers are
//! configured (see [`crate::build_client`]).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use tokio::sync::{RwLock, broadcast};
use tokio_stream::wrappers::BroadcastStream;

use crate::consumer::{ConsumedMessage, Consumer, MessageStream};
use crate::error::EventingError;
use crate::producer::Producer;

const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// In-process broadcast-channel-backed eventing backend. Cheap to clone; the inner state is
/// `Arc`-shared so producer + consumer halves share the same channels.
#[derive(Clone, Default)]
pub struct InMemoryBackend {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    topics: RwLock<HashMap<String, broadcast::Sender<ConsumedMessage>>>,
    capacity: usize,
}

impl InMemoryBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                topics: RwLock::new(HashMap::new()),
                capacity: DEFAULT_CHANNEL_CAPACITY,
            }),
        }
    }

    /// Construct with a custom per-topic broadcast channel capacity. Useful for tests that
    /// want to provoke `Lagged` deliberately.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                topics: RwLock::new(HashMap::new()),
                capacity: capacity.max(1),
            }),
        }
    }

    async fn sender_for(&self, topic: &str) -> broadcast::Sender<ConsumedMessage> {
        {
            let map = self.inner.topics.read().await;
            if let Some(tx) = map.get(topic) {
                return tx.clone();
            }
        }

        let mut map = self.inner.topics.write().await;
        map.entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(self.inner.capacity).0)
            .clone()
    }
}

#[async_trait]
impl Producer for InMemoryBackend {
    async fn publish(
        &self,
        topic: &str,
        key: Option<&[u8]>,
        headers: &[(String, Vec<u8>)],
        payload: &[u8],
    ) -> Result<(), EventingError> {
        let tx = self.sender_for(topic).await;
        let msg = ConsumedMessage {
            key: key.map(<[u8]>::to_vec),
            headers: headers.to_vec(),
            payload: payload.to_vec(),
        };
        let _ = tx.send(msg);
        Ok(())
    }
}

#[async_trait]
impl Consumer for InMemoryBackend {
    async fn subscribe(&self, topic: &str) -> Result<MessageStream, EventingError> {
        let tx = self.sender_for(topic).await;
        let rx = tx.subscribe();
        let stream = BroadcastStream::new(rx).map(|item| {
            item.map_err(|e| EventingError::Backend(format!("in-memory backend lag: {e}")))
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use futures::StreamExt;
    use tokio::time::timeout;

    fn sample(payload: &[u8]) -> (Vec<(String, Vec<u8>)>, Vec<u8>) {
        let headers = vec![("h-key".to_string(), b"h-val".to_vec())];
        (headers, payload.to_vec())
    }

    #[tokio::test]
    async fn publish_then_subscribe_receives_nothing() {
        let backend = InMemoryBackend::new();
        let (h, p) = sample(b"first");

        backend.publish("t", None, &h, &p).await.unwrap();

        let mut stream = backend.subscribe("t").await.unwrap();

        // No publish after subscribe -> no message within the window.
        let res = timeout(Duration::from_millis(50), stream.next()).await;
        assert!(res.is_err(), "subscribed-after-publish should see nothing");
    }

    #[tokio::test]
    async fn subscribe_then_publish_roundtrip() {
        let backend = InMemoryBackend::new();
        let mut stream = backend.subscribe("t").await.unwrap();

        let (h, p) = sample(b"hello");
        backend.publish("t", Some(b"k"), &h, &p).await.unwrap();

        let got = timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("backend error");

        assert_eq!(got.key.as_deref(), Some(b"k".as_ref()));
        assert_eq!(got.headers, h);
        assert_eq!(got.payload, b"hello");
    }

    #[tokio::test]
    async fn two_subscribers_both_receive() {
        let backend = InMemoryBackend::new();
        let mut s1 = backend.subscribe("t").await.unwrap();
        let mut s2 = backend.subscribe("t").await.unwrap();

        let (h, p) = sample(b"fan-out");
        backend.publish("t", None, &h, &p).await.unwrap();

        let m1 = timeout(Duration::from_millis(200), s1.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let m2 = timeout(Duration::from_millis(200), s2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(m1.payload, b"fan-out");
        assert_eq!(m2.payload, b"fan-out");
    }

    #[tokio::test]
    async fn unrelated_topic_isolated() {
        let backend = InMemoryBackend::new();
        let mut on_b = backend.subscribe("topic-b").await.unwrap();

        let (h, p) = sample(b"only-on-a");
        backend.publish("topic-a", None, &h, &p).await.unwrap();

        let res = timeout(Duration::from_millis(50), on_b.next()).await;
        assert!(res.is_err(), "topic isolation broken");
    }

    #[tokio::test]
    async fn factory_default_uses_default_capacity() {
        let backend = InMemoryBackend::new();
        assert_eq!(backend.inner.capacity, DEFAULT_CHANNEL_CAPACITY);
    }
}
