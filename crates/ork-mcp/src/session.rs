//! Per-`(tenant, server)` connection pool over rmcp client sessions.
//!
//! Each MCP server connection is **per-tenant**: ADR 0010 §`Security and
//! tenant scoping` demands that two tenants never share an `rmcp`
//! `RunningService`, because a single `RunningService` carries one
//! authenticated session and one stdio child-process. Sharing would leak
//! credentials across tenants.
//!
//! Within a tenant the pool gives a single-flight guarantee: two concurrent
//! `acquire` calls for the same `(tenant_id, server_id)` race into the same
//! [`tokio::sync::OnceCell`], so only one connect / `initialize` handshake
//! actually goes over the wire and both callers receive the same
//! `Arc<RunningService>`.
//!
//! Idle eviction (ADR 0010 §`Negative / costs`): an entry whose
//! `last_used` instant is older than `idle_ttl` is dropped on the next
//! sweeper tick. Dropping the entry releases the `Arc<RunningService>`,
//! which in turn (once any in-flight callers also drop their clones) lets
//! the rmcp `RunningService::Drop` impl close the connection — for stdio
//! servers that means the child process is killed.
//!
//! The pool is generic over the value type `V` so tests can swap in a
//! cheap mock; production code instantiates it with rmcp's
//! `RunningService<RoleClient, ()>`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ork_common::error::OrkError;
use ork_common::types::TenantId;
use rmcp::service::{RoleClient, RunningService};
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Production-typed alias: each session is an rmcp client `RunningService`.
pub type SessionPool = GenericSessionPool<RunningService<RoleClient, ()>>;

/// `(TenantId, server_id)` cache key. Uses `String` for the server id to
/// avoid leaking a borrow across `await` boundaries.
type Key = (TenantId, String);

/// One per-key cell + idleness clock.
struct Entry<V> {
    /// Single-flight init. The first task to call
    /// [`OnceCell::get_or_try_init`] runs the connect closure; concurrent
    /// callers await on the same cell and get the resulting `Arc<V>`.
    cell: OnceCell<Arc<V>>,
    /// Most recent `acquire` instant. Updated under a *std* mutex (cheap,
    /// short critical section) instead of `tokio::sync::Mutex` because we
    /// never hold it across `await`.
    last_used: StdMutex<Instant>,
}

/// Pool body. Construct with [`Self::new`] and start the eviction sweeper
/// with [`Self::spawn_eviction_loop`]; the sweeper's `JoinHandle` is owned
/// by `McpClient` so it gets cancelled in process shutdown.
pub struct GenericSessionPool<V> {
    inner: Mutex<HashMap<Key, Arc<Entry<V>>>>,
    idle_ttl: Duration,
    cancel: CancellationToken,
}

impl<V> GenericSessionPool<V>
where
    V: Send + Sync + 'static,
{
    #[must_use]
    pub fn new(idle_ttl: Duration, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
            idle_ttl,
            cancel,
        })
    }

    /// Acquire (or open) the session for `(tenant_id, server_id)`. The
    /// supplied `connect` closure is only invoked the first time; all later
    /// callers (including those racing the first one) get an `Arc`-clone
    /// of the original session.
    ///
    /// # Errors
    ///
    /// Whatever `connect` returns. The cell stays empty on failure so the
    /// next `acquire` will retry — we don't want a single bad startup to
    /// permanently wedge the pool.
    pub async fn acquire<F, Fut>(
        &self,
        tenant_id: TenantId,
        server_id: &str,
        connect: F,
    ) -> Result<Arc<V>, OrkError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, OrkError>>,
    {
        let key: Key = (tenant_id, server_id.to_string());
        let entry = {
            let mut inner = self.inner.lock().await;
            inner
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(Entry {
                        cell: OnceCell::new(),
                        last_used: StdMutex::new(Instant::now()),
                    })
                })
                .clone()
        };

        let svc = entry
            .cell
            .get_or_try_init(|| async move {
                let v = connect().await?;
                Ok::<_, OrkError>(Arc::new(v))
            })
            .await?
            .clone();

        if let Ok(mut last) = entry.last_used.lock() {
            *last = Instant::now();
        }
        Ok(svc)
    }

    /// Start the background eviction loop. The returned `JoinHandle` should
    /// be held by `McpClient`; the loop terminates when the
    /// [`CancellationToken`] passed to [`Self::new`] is cancelled.
    #[must_use]
    pub fn spawn_eviction_loop(self: Arc<Self>) -> JoinHandle<()> {
        let interval = (self.idle_ttl / 2).max(Duration::from_secs(1));
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        self.evict_idle().await;
                    }
                }
            }
        })
    }

    /// Drop entries whose `last_used` instant is older than `idle_ttl`.
    /// Visible to tests so they can drive eviction without spinning the
    /// sweeper task.
    pub async fn evict_idle(&self) {
        let cutoff = Instant::now() - self.idle_ttl;
        let mut inner = self.inner.lock().await;
        let before = inner.len();
        inner.retain(|key, entry| {
            let last = match entry.last_used.lock() {
                Ok(g) => *g,
                Err(poisoned) => *poisoned.into_inner(),
            };
            let keep = last >= cutoff;
            if !keep {
                tracing::debug!(
                    tenant_id = %key.0.0,
                    server_id = %key.1,
                    "ADR-0010: evicting idle MCP session"
                );
            }
            keep
        });
        let dropped = before.saturating_sub(inner.len());
        if dropped > 0 {
            warn!(
                dropped,
                "ADR-0010: SessionPool reaped {dropped} idle MCP session(s)"
            );
        }
    }

    /// Test/observability hook: number of live entries.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Test/observability hook: `true` when the pool currently holds no
    /// `(tenant, server)` entries. Pairs with [`Self::len`] to silence
    /// clippy's `len_without_is_empty` lint.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stand-in for `RunningService` so tests don't need to spin up real
    /// MCP servers. Carries an id we can assert against.
    #[derive(Debug)]
    struct MockSession {
        id: usize,
    }

    fn pool() -> Arc<GenericSessionPool<MockSession>> {
        GenericSessionPool::new(Duration::from_secs(60), CancellationToken::new())
    }

    #[tokio::test]
    async fn concurrent_acquires_for_same_tenant_share_one_session() {
        let p = pool();
        let counter = Arc::new(AtomicUsize::new(0));
        let tenant = TenantId::new();

        let make_connect = || {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    let id = counter.fetch_add(1, Ordering::SeqCst);
                    // Yield to give the second task a chance to slot into
                    // the same OnceCell while we're still "connecting".
                    tokio::task::yield_now().await;
                    Ok::<_, OrkError>(MockSession { id })
                }
            }
        };

        let p1 = p.clone();
        let p2 = p.clone();
        let connect1 = make_connect();
        let connect2 = make_connect();
        let (a, b) = tokio::join!(
            async move { p1.acquire(tenant, "srv", connect1).await.unwrap() },
            async move { p2.acquire(tenant, "srv", connect2).await.unwrap() },
        );

        assert_eq!(a.id, b.id, "both calls must return the same session id");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "connect closure ran more than once: pool failed to single-flight"
        );
        assert!(
            Arc::ptr_eq(&a, &b),
            "Arc identity must match across concurrent acquires"
        );
    }

    #[tokio::test]
    async fn different_tenants_get_distinct_sessions() {
        let p = pool();
        let counter = Arc::new(AtomicUsize::new(0));
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();

        let make_connect = || {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    let id = counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, OrkError>(MockSession { id })
                }
            }
        };

        let a = p.acquire(tenant_a, "srv", make_connect()).await.unwrap();
        let b = p.acquire(tenant_b, "srv", make_connect()).await.unwrap();

        assert_ne!(a.id, b.id, "tenant isolation broken: same session reused");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn second_acquire_reuses_session_within_idle_ttl() {
        let p = pool();
        let counter = Arc::new(AtomicUsize::new(0));
        let tenant = TenantId::new();

        let make_connect = || {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    let id = counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, OrkError>(MockSession { id })
                }
            }
        };

        let a = p.acquire(tenant, "srv", make_connect()).await.unwrap();
        let b = p.acquire(tenant, "srv", make_connect()).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn eviction_drops_idle_entries() {
        let pool = GenericSessionPool::<MockSession>::new(
            Duration::from_secs(60),
            CancellationToken::new(),
        );
        let counter = Arc::new(AtomicUsize::new(0));
        let tenant = TenantId::new();

        let connect = {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    let id = counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, OrkError>(MockSession { id })
                }
            }
        };

        pool.acquire(tenant, "srv", connect).await.unwrap();
        assert_eq!(pool.len().await, 1);

        tokio::time::advance(Duration::from_secs(120)).await;
        pool.evict_idle().await;
        assert_eq!(pool.len().await, 0, "idle entry must be evicted");
    }

    #[tokio::test(start_paused = true)]
    async fn recently_used_entries_are_not_evicted() {
        let pool = GenericSessionPool::<MockSession>::new(
            Duration::from_secs(60),
            CancellationToken::new(),
        );
        let counter = Arc::new(AtomicUsize::new(0));
        let tenant = TenantId::new();

        let make_connect = || {
            let counter = counter.clone();
            move || {
                let counter = counter.clone();
                async move {
                    let id = counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, OrkError>(MockSession { id })
                }
            }
        };

        pool.acquire(tenant, "srv", make_connect()).await.unwrap();
        tokio::time::advance(Duration::from_secs(40)).await;
        pool.acquire(tenant, "srv", make_connect()).await.unwrap();
        tokio::time::advance(Duration::from_secs(40)).await;

        pool.evict_idle().await;
        assert_eq!(
            pool.len().await,
            1,
            "entry was used 40s ago (< 60s idle TTL); must NOT be evicted"
        );
    }
}
