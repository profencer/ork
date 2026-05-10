//! ADR-0056 §`Open questions` #2: `Idempotency-Key` header.
//!
//! A `POST /api/agents/:id/generate` (or workflow run / tool invoke)
//! retried by a flaky network MUST NOT double-bill. v1 ships an
//! in-memory cache: when a request arrives with `Idempotency-Key: <k>`,
//! the (tenant, route, key) tuple is recorded; subsequent requests with
//! the same tuple within 24 h get the cached response back instead of
//! triggering the action a second time.
//!
//! v1 is in-memory only — sufficient for single-replica deployments and
//! test environments. Multi-replica production swaps the storage to
//! Redis behind the same trait.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ork_common::types::TenantId;

const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One cache row.
#[derive(Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct IdempotencyCache {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    rows: HashMap<Key, (CachedResponse, Instant)>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    tenant: TenantId,
    route: &'static str,
    key: String,
}

impl IdempotencyCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch a previously cached response, if any. Evicts expired rows lazily.
    #[must_use]
    pub fn lookup(
        &self,
        tenant: TenantId,
        route: &'static str,
        key: &str,
    ) -> Option<CachedResponse> {
        let mut g = self.inner.lock().expect("idempotency cache poisoned");
        g.rows.retain(|_, (_, at)| at.elapsed() < DEFAULT_TTL);
        let k = Key {
            tenant,
            route,
            key: key.to_string(),
        };
        g.rows.get(&k).map(|(resp, _)| resp.clone())
    }

    pub fn record(
        &self,
        tenant: TenantId,
        route: &'static str,
        key: &str,
        response: CachedResponse,
    ) {
        let mut g = self.inner.lock().expect("idempotency cache poisoned");
        let k = Key {
            tenant,
            route,
            key: key.to_string(),
        };
        g.rows.insert(k, (response, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_lookup_returns_same_body() {
        let c = IdempotencyCache::new();
        let tenant = TenantId::new();
        c.record(
            tenant,
            "/api/agents/x/generate",
            "abc",
            CachedResponse {
                status: 200,
                body: b"hello".to_vec(),
            },
        );
        let got = c
            .lookup(tenant, "/api/agents/x/generate", "abc")
            .expect("hit");
        assert_eq!(got.status, 200);
        assert_eq!(got.body, b"hello");
    }

    #[test]
    fn lookup_misses_when_key_unknown() {
        let c = IdempotencyCache::new();
        assert!(c.lookup(TenantId::new(), "/r", "missing").is_none());
    }

    #[test]
    fn different_tenant_does_not_share_cache() {
        let c = IdempotencyCache::new();
        let t1 = TenantId::new();
        let t2 = TenantId::new();
        c.record(
            t1,
            "/r",
            "k",
            CachedResponse {
                status: 200,
                body: b"a".to_vec(),
            },
        );
        assert!(c.lookup(t2, "/r", "k").is_none());
    }
}
