use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;

use crate::error::EventingError;

/// A single record received from a topic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumedMessage {
    pub key: Option<Vec<u8>>,
    pub headers: Vec<(String, Vec<u8>)>,
    pub payload: Vec<u8>,
}

/// Stream of records produced by [`Consumer::subscribe`]. Errors are surfaced inline so a
/// single transient backend hiccup does not cancel the whole subscription.
pub type MessageStream = Pin<Box<dyn Stream<Item = Result<ConsumedMessage, EventingError>> + Send>>;

/// Backend-agnostic async consumer for Kafka topics (ADR 0004).
///
/// Implementations return a long-lived stream of records published to `topic` from the time
/// of subscription onward. Replay semantics (Kafka offsets, broadcast capacity, etc.) are
/// backend-defined; see the impl docs.
#[async_trait]
pub trait Consumer: Send + Sync {
    /// Subscribe to `topic`. The returned stream is `Send` so it can be moved across await
    /// points and into spawned tasks.
    async fn subscribe(&self, topic: &str) -> Result<MessageStream, EventingError>;
}
