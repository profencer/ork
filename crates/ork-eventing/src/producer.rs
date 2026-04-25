use async_trait::async_trait;

use crate::error::EventingError;

/// Backend-agnostic async producer for Kafka topics (ADR 0004).
///
/// Implementations push a single record (key + headers + payload) onto `topic`. Callers are
/// responsible for choosing the right topic name (use [`ork_a2a::topics`]) and the right
/// header set (use [`ork_a2a::headers`]).
#[async_trait]
pub trait Producer: Send + Sync {
    /// Publish a single record. `key` is used by Kafka for partitioning; pass `None` to let
    /// the backend round-robin. `headers` carry the ADR-0004 envelope metadata.
    async fn publish(
        &self,
        topic: &str,
        key: Option<&[u8]>,
        headers: &[(String, Vec<u8>)],
        payload: &[u8],
    ) -> Result<(), EventingError>;
}
