//! Per-process TTL cache for MCP tool descriptors.
//!
//! Sized for one entry per `(TenantId, server_id)` per ork-api process;
//! a `RwLock<HashMap>` is plenty. We deliberately avoid pulling in `moka`
//! or `cached` — both would be larger than the entire MCP client.
//!
//! The cache is **per-tenant by key**, not by storage namespace, because
//! tenants override server URLs and credentials (ADR 0010 §`Server
//! registration` first source) so two tenants pointing at "the same" id
//! `atlassian` may resolve to different servers.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::RwLock;
use std::time::Duration;

use tokio::time::Instant;

/// In-memory map with per-entry expiry. Reads are lock-free up to the
/// `RwLock`; writes briefly take the exclusive lock to insert / overwrite.
///
/// Expiry is **lazy**: an expired entry is detected on `get` and reported
/// as `None`. There's no background sweeper because the descriptor refresh
/// loop in [`crate::client::McpClient`] overwrites entries before they can
/// stale-out in steady state, so a sweeper would be pure overhead.
pub struct TtlCache<K, V>
where
    K: Eq + Hash,
{
    inner: RwLock<HashMap<K, Entry<V>>>,
    ttl: Duration,
}

struct Entry<V> {
    value: V,
    inserted_at: Instant,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Build an empty cache with the given expiry. `ttl` is enforced
    /// against [`tokio::time::Instant::now`], which respects
    /// `tokio::time::pause` in tests.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Returns `Some(value)` when the entry exists and was inserted within
    /// the cache's `ttl`; otherwise `None`.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<V> {
        let guard = self.inner.read().ok()?;
        let entry = guard.get(key)?;
        if entry.inserted_at.elapsed() <= self.ttl {
            Some(entry.value.clone())
        } else {
            None
        }
    }

    /// Inserts `value` under `key`, overwriting any existing entry and
    /// resetting the per-entry insertion timestamp.
    pub fn insert(&self, key: K, value: V) {
        if let Ok(mut guard) = self.inner.write() {
            guard.insert(
                key,
                Entry {
                    value,
                    inserted_at: Instant::now(),
                },
            );
        }
    }

    /// Drop a single entry (used by tests; production code refreshes
    /// instead).
    pub fn invalidate(&self, key: &K) {
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(key);
        }
    }

    /// Snapshot the live (non-expired) entries. `McpClient::refresh_all`
    /// uses this to enumerate which (tenant, server) pairs to refresh.
    #[must_use]
    pub fn keys(&self) -> Vec<K> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        guard
            .iter()
            .filter(|(_, entry)| entry.inserted_at.elapsed() <= self.ttl)
            .map(|(k, _)| k.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn returns_value_within_ttl() {
        let cache: TtlCache<&'static str, i32> = TtlCache::new(Duration::from_secs(60));
        cache.insert("k", 7);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(cache.get(&"k"), Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn returns_none_after_ttl() {
        let cache: TtlCache<&'static str, i32> = TtlCache::new(Duration::from_secs(60));
        cache.insert("k", 7);
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(cache.get(&"k"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn insert_overwrites_and_resets_clock() {
        let cache: TtlCache<&'static str, i32> = TtlCache::new(Duration::from_secs(60));
        cache.insert("k", 1);
        tokio::time::advance(Duration::from_secs(40)).await;
        cache.insert("k", 2);
        tokio::time::advance(Duration::from_secs(40)).await;
        // 40s after the second insert is well within the 60s ttl.
        assert_eq!(cache.get(&"k"), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn invalidate_drops_entry_immediately() {
        let cache: TtlCache<&'static str, i32> = TtlCache::new(Duration::from_secs(60));
        cache.insert("k", 9);
        cache.invalidate(&"k");
        assert_eq!(cache.get(&"k"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn keys_excludes_expired_entries() {
        let cache: TtlCache<&'static str, i32> = TtlCache::new(Duration::from_secs(60));
        cache.insert("alive", 1);
        tokio::time::advance(Duration::from_secs(120)).await;
        cache.insert("fresh", 2);
        let keys = cache.keys();
        assert_eq!(keys, vec!["fresh"]);
    }
}
