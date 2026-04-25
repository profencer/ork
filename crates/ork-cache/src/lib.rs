//! Tiny key/value cache abstraction shared by anything in `ork` that needs an
//! "out of process" hot cache (per ADR-0004 — short-lived correlation/replay
//! traffic). The first user is ADR-0007 [`A2aRemoteAgent`] card fetching.
//!
//! The trait is intentionally byte-oriented and async-only so callers can pick
//! their own serialisation format and we can swap Redis out for an in-memory
//! impl in tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use tokio::sync::Mutex;

/// Minimal byte-oriented K/V cache. Implementations MUST be safe to share
/// across tasks and SHOULD be cheap to clone (typically `Arc`-wrap themselves).
#[async_trait]
pub trait KeyValueCache: Send + Sync {
    /// Retrieve a value by key. Returns `Ok(None)` on a cache miss; `Err`
    /// only for transport/serialisation faults (the caller decides whether to
    /// fail open or closed).
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, OrkError>;

    /// Insert / overwrite a value. `ttl` MUST be honoured by the backend.
    async fn set_with_ttl(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), OrkError>;

    /// Forget a single key. Missing keys are NOT an error.
    async fn delete(&self, key: &str) -> Result<(), OrkError>;
}

/// Production [`KeyValueCache`] backed by a single Redis connection manager
/// (clone-on-share, automatic reconnect — see `redis::aio::ConnectionManager`).
#[derive(Clone)]
pub struct RedisCache {
    conn: ConnectionManager,
}

impl RedisCache {
    /// Build a cache from a Redis URL (e.g. `redis://localhost:6379`).
    pub async fn connect(url: &str) -> Result<Self, OrkError> {
        let client = redis::Client::open(url)
            .map_err(|e| OrkError::Internal(format!("redis open {url}: {e}")))?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| OrkError::Internal(format!("redis connect {url}: {e}")))?;
        Ok(Self { conn })
    }

    /// Wrap an existing connection manager (handy for tests / shared pools).
    pub fn from_connection_manager(conn: ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl KeyValueCache for RedisCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, OrkError> {
        let mut conn = self.conn.clone();
        let value: Option<Vec<u8>> = conn
            .get(key)
            .await
            .map_err(|e| OrkError::Internal(format!("redis GET {key}: {e}")))?;
        Ok(value)
    }

    async fn set_with_ttl(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), OrkError> {
        let mut conn = self.conn.clone();
        let secs = ttl.as_secs().max(1);
        let _: () = conn
            .set_ex(key, value, secs)
            .await
            .map_err(|e| OrkError::Internal(format!("redis SETEX {key}: {e}")))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), OrkError> {
        let mut conn = self.conn.clone();
        let _: () = conn
            .del(key)
            .await
            .map_err(|e| OrkError::Internal(format!("redis DEL {key}: {e}")))?;
        Ok(())
    }
}

/// In-process [`KeyValueCache`] for tests and ergonomic local dev. TTLs are
/// stored alongside the value and checked on read; expired keys are removed
/// lazily on access.
#[derive(Clone, Default)]
pub struct InMemoryCache {
    inner: Arc<Mutex<HashMap<String, (Vec<u8>, std::time::Instant)>>>,
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl KeyValueCache for InMemoryCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, OrkError> {
        let mut guard = self.inner.lock().await;
        if let Some((value, expires_at)) = guard.get(key) {
            if std::time::Instant::now() < *expires_at {
                return Ok(Some(value.clone()));
            }
            guard.remove(key);
        }
        Ok(None)
    }

    async fn set_with_ttl(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), OrkError> {
        let mut guard = self.inner.lock().await;
        let expires_at = std::time::Instant::now() + ttl;
        guard.insert(key.to_string(), (value.to_vec(), expires_at));
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), OrkError> {
        let mut guard = self.inner.lock().await;
        guard.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_round_trip() {
        let cache = InMemoryCache::new();
        cache
            .set_with_ttl("k", b"v", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[tokio::test]
    async fn in_memory_ttl_expires() {
        let cache = InMemoryCache::new();
        cache
            .set_with_ttl("k", b"v", Duration::from_millis(20))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(cache.get("k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_delete_removes_key() {
        let cache = InMemoryCache::new();
        cache
            .set_with_ttl("k", b"v", Duration::from_secs(5))
            .await
            .unwrap();
        cache.delete("k").await.unwrap();
        assert!(cache.get("k").await.unwrap().is_none());
    }
}
