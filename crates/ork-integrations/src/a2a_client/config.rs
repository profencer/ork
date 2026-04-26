//! Static client-side knobs for [`super::A2aRemoteAgent`] (ADR-0007 §`Construction`).
//!
//! These are picked at build time, not per-call, so a single `A2aClientConfig`
//! is shared by the static-config loader, the discovery subscriber, and the
//! workflow inline-card overlay. Per-call HTTP behaviour (idempotency keys,
//! tenant header values, …) lives in [`crate::a2a_client::A2aRemoteAgent`].

use std::sync::Arc;
use std::time::Duration;

use ork_core::ports::artifact_meta_repo::ArtifactMetaRepo;
use ork_core::ports::artifact_store::ArtifactStore;

/// HTTP+SSE client tuning shared across remote A2A clients in this process.
#[derive(Clone)]
pub struct A2aClientConfig {
    /// Per-request HTTP timeout (TCP connect + TLS + headers + body). Default 30s.
    pub request_timeout: Duration,
    /// Idle-data timeout while reading an SSE body. The remote SHOULD send periodic
    /// `working` heartbeats; we treat a quiet connection as broken after this.
    /// ADR-0007 calls out 5min as a sensible default.
    pub stream_idle_timeout: Duration,
    /// Retry policy applied to non-idempotent transport-level failures (5xx, TLS,
    /// connection reset). 4xx-not-429 are NEVER retried.
    pub retry: RetryPolicy,
    /// `User-Agent` header sent on every request (defaults to `ork/<crate version>`).
    pub user_agent: String,
    /// Refresh interval for cached `AgentCard`s pulled via `CardFetcher`.
    pub card_refresh_interval: Duration,
    /// ADR-0016: when all three are set, outbound `message/send` and `message/stream`
    /// replace `Part::File` base64 with proxy URIs before POST.
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    pub artifact_meta: Option<Arc<dyn ArtifactMetaRepo>>,
    /// Public API base, no path (e.g. `https://ork.example`); `None` disables rewrite.
    pub artifact_public_base: Option<String>,
}

impl std::fmt::Debug for A2aClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aClientConfig")
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("retry", &self.retry)
            .field("user_agent", &self.user_agent)
            .field("card_refresh_interval", &self.card_refresh_interval)
            .field(
                "artifact_store",
                &self.artifact_store.as_ref().map(|_| "<set>"),
            )
            .field(
                "artifact_meta",
                &self.artifact_meta.as_ref().map(|_| "<set>"),
            )
            .field("artifact_public_base", &self.artifact_public_base)
            .finish()
    }
}

impl Default for A2aClientConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            stream_idle_timeout: Duration::from_secs(300),
            retry: RetryPolicy::default(),
            user_agent: format!("ork/{}", env!("CARGO_PKG_VERSION")),
            card_refresh_interval: Duration::from_secs(60 * 60),
            artifact_store: None,
            artifact_meta: None,
            artifact_public_base: None,
        }
    }
}

/// Exponential-backoff retry policy with a hard ceiling. Honours `Retry-After`
/// when the response carries one (capped at `max_delay`).
///
/// Defaults pulled from the ADR: up to 3 attempts, 250ms initial delay, x2 factor,
/// 5s ceiling — total wall-clock budget tops out at ~5.25s.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Maximum number of *attempts* (including the first). `1` disables retries.
    pub max_attempts: u32,
    /// Initial backoff delay before the second attempt.
    pub initial_delay: Duration,
    /// Multiplicative factor applied to the previous delay.
    pub factor: f32,
    /// Hard ceiling for any single backoff delay (also caps `Retry-After`).
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            factor: 2.0,
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// Compute the backoff for the (1-indexed) `attempt`, ignoring `Retry-After`.
    /// `attempt = 1` returns `Duration::ZERO` (no backoff before the first call).
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let exp = attempt.saturating_sub(2);
        let mut secs = self.initial_delay.as_secs_f32();
        for _ in 0..exp {
            secs *= self.factor;
        }
        let candidate = Duration::from_secs_f32(secs);
        candidate.min(self.max_delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_adr_table() {
        let cfg = A2aClientConfig::default();
        assert_eq!(cfg.request_timeout, Duration::from_secs(30));
        assert_eq!(cfg.stream_idle_timeout, Duration::from_secs(300));
        assert_eq!(cfg.retry.max_attempts, 3);
        assert_eq!(cfg.retry.initial_delay, Duration::from_millis(250));
        assert!(cfg.user_agent.starts_with("ork/"));
    }

    #[test]
    fn retry_delay_doubles_then_caps() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for(1), Duration::ZERO);
        assert_eq!(p.delay_for(2), Duration::from_millis(250));
        assert_eq!(p.delay_for(3), Duration::from_millis(500));
        assert!(p.delay_for(8) <= p.max_delay);
    }
}
