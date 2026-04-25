//! ADR-0008 §`SSE bridge` — middle tier of the three-tier replay strategy.
//!
//! Postgres provides full task history; Kafka provides the live tail; this buffer
//! caches the most recent `window` worth of SSE-encoded events per task so that a
//! reconnecting client with a `Last-Event-ID` close to the present can be served
//! without a Postgres scan or a Kafka rewind.
//!
//! Two implementations:
//!
//! - [`InMemorySseBuffer`] — used in tests and single-node dev. Eviction is
//!   manual via [`SseBuffer::evict_expired`] (called by the bridge handler on a
//!   coarse cadence) so unit tests can drive expiry deterministically.
//! - [`RedisSseBuffer`] — production. Each task's events live in a sorted set
//!   keyed by event id; TTL on the key handles eviction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct ReplayEvent {
    /// Monotonic per-task event id. Doubles as the SSE `id:` field so that a
    /// reconnecting client can supply `Last-Event-ID` for resume.
    pub id: u64,
    /// Already-rendered SSE event payload (typically a JSON-encoded `TaskEvent`).
    pub payload: Vec<u8>,
    /// Wall-clock at append time; drives in-memory eviction.
    pub at: SystemTime,
}

#[async_trait]
pub trait SseBuffer: Send + Sync {
    async fn append(&self, task_id: &str, event: ReplayEvent);
    async fn replay(&self, task_id: &str, after_id: Option<u64>) -> Vec<ReplayEvent>;
    async fn evict_expired(&self);
}

pub struct InMemorySseBuffer {
    window: Duration,
    inner: Mutex<HashMap<String, Vec<ReplayEvent>>>,
}

impl InMemorySseBuffer {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            inner: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SseBuffer for InMemorySseBuffer {
    async fn append(&self, task_id: &str, event: ReplayEvent) {
        self.inner
            .lock()
            .await
            .entry(task_id.to_string())
            .or_default()
            .push(event);
    }
    async fn replay(&self, task_id: &str, after_id: Option<u64>) -> Vec<ReplayEvent> {
        let g = self.inner.lock().await;
        g.get(task_id)
            .map(|v| {
                v.iter()
                    .filter(|e| after_id.is_none_or(|a| e.id > a))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    async fn evict_expired(&self) {
        let cutoff = SystemTime::now()
            .checked_sub(self.window)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut g = self.inner.lock().await;
        for v in g.values_mut() {
            v.retain(|e| e.at >= cutoff);
        }
    }
}

/// Redis-backed buffer: keyed sorted set per task with score = event id; ZADD on
/// append, ZRANGEBYSCORE on replay, key TTL for eviction. The trait signature
/// mirrors [`InMemorySseBuffer`]; ADR-0022 will add metrics for hit/miss.
pub struct RedisSseBuffer {
    conn: Arc<Mutex<redis::aio::ConnectionManager>>,
    namespace: String,
    window: Duration,
}

impl RedisSseBuffer {
    pub fn new(conn: redis::aio::ConnectionManager, namespace: String, window: Duration) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            namespace,
            window,
        }
    }
    fn key(&self, task_id: &str) -> String {
        format!("{}:sse:{task_id}", self.namespace)
    }
}

#[async_trait]
impl SseBuffer for RedisSseBuffer {
    async fn append(&self, task_id: &str, event: ReplayEvent) {
        use redis::AsyncCommands;
        let key = self.key(task_id);
        let mut c = self.conn.lock().await;
        let _: redis::RedisResult<()> = c.zadd(&key, event.payload, event.id).await;
        let ttl = self.window.as_secs() as i64;
        let _: redis::RedisResult<()> = c.expire(&key, ttl).await;
    }
    async fn replay(&self, task_id: &str, after_id: Option<u64>) -> Vec<ReplayEvent> {
        use redis::AsyncCommands;
        let key = self.key(task_id);
        let min = after_id
            .map(|a| format!("({a}"))
            .unwrap_or_else(|| "-inf".into());
        let mut c = self.conn.lock().await;
        let pairs: Vec<(Vec<u8>, f64)> = c
            .zrangebyscore_withscores(&key, min, "+inf")
            .await
            .unwrap_or_default();
        pairs
            .into_iter()
            .map(|(payload, score)| ReplayEvent {
                id: score as u64,
                payload,
                at: SystemTime::now(),
            })
            .collect()
    }
    async fn evict_expired(&self) {
        // Key TTL handles eviction. Method exists to satisfy the trait contract.
    }
}
