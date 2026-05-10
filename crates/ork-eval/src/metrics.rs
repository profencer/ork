//! Scorer metrics
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Live sampling`).
//!
//! `scorer_dropped_total` is exported because dropped score jobs (the
//! background queue is full) are silent failures — they do not surface
//! in the user-facing response or in `scorer_results`. The counter is
//! the only signal that a Studio "scorer health" panel (deferred) can
//! latch onto; a downstream alert ADR will turn it into a Prometheus
//! alert.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::registry::Registry;
use std::sync::Arc;

/// Set of scorer-related counters owned by the live worker.
///
/// Cloning is cheap (`Arc` + `Counter` is internally `Arc`-shared) so
/// the same handle can be passed to the worker, the agent hook, and
/// the workflow hook.
#[derive(Clone, Default)]
pub struct ScorerMetrics {
    /// Score jobs that could not be enqueued because the bounded
    /// channel was full. ADR-0054 acceptance criterion (c).
    pub dropped_total: Counter,
    /// Score jobs that could not be enqueued because the worker
    /// queue is closed (consumer panicked / shutdown). Surfaced
    /// separately from `dropped_total` so an "outage on the worker"
    /// never gets confused with backlog under load (ADR-0054
    /// reviewer finding m3, deferred from a single-counter design).
    pub worker_closed_total: Counter,
    /// Score jobs that were enqueued and consumed by the worker.
    pub processed_total: Counter,
    /// Score jobs whose `Scorer::score` returned `Err`.
    pub failed_total: Counter,
}

impl ScorerMetrics {
    /// Build a fresh set of counters. The caller owns the
    /// [`Registry`] — installs that aggregate registries at the
    /// process level can call [`Self::register`] to publish ours.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register this metric set in `registry` under the
    /// `scorer_*_total` namespace.
    pub fn register(self: &Arc<Self>, registry: &mut Registry) {
        registry.register(
            "scorer_dropped",
            "Score jobs dropped because the live worker queue was full",
            self.dropped_total.clone(),
        );
        registry.register(
            "scorer_worker_closed",
            "Score jobs dropped because the live worker queue is closed (worker shutdown / panic)",
            self.worker_closed_total.clone(),
        );
        registry.register(
            "scorer_processed",
            "Score jobs successfully processed by the live worker",
            self.processed_total.clone(),
        );
        registry.register(
            "scorer_failed",
            "Score jobs whose Scorer::score returned an error",
            self.failed_total.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment() {
        let m = ScorerMetrics::new();
        m.dropped_total.inc();
        m.processed_total.inc();
        m.processed_total.inc();
        m.failed_total.inc();
        assert_eq!(m.dropped_total.get(), 1);
        assert_eq!(m.processed_total.get(), 2);
        assert_eq!(m.failed_total.get(), 1);
    }

    #[test]
    fn registers_in_registry_without_panic() {
        let m = ScorerMetrics::new();
        let mut registry = Registry::default();
        m.register(&mut registry);
    }
}
