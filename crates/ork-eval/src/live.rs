//! Live sampling worker
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Live sampling`).
//!
//! `OrkApp::build` constructs a [`LiveWorker`] and hands a
//! [`LiveSamplerHandle`] back to the agent / workflow hooks. Hooks
//! decide whether to enqueue (sampling predicate) and `try_send`
//! the [`ScoreJob`] — failure increments `scorer_dropped_total` so
//! a backed-up worker never slows the user-facing path.

use std::sync::Arc;

use ork_core::ports::scorer::{RunId, RunKind, ScoreInput, Scorer, Trace};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::warn;

use crate::metrics::ScorerMetrics;

/// Default bounded-channel capacity. Sized to swallow short bursts
/// without forcing the caller into the worker's critical path.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Snapshot of a completed run packaged for background scoring.
///
/// Owned data only — the worker outlives the originating run. The
/// `context` is captured eagerly because [`AgentContext`] is `Clone`
/// and inexpensive (`Arc`-shared internals).
pub struct ScoreJob {
    pub run_id: RunId,
    pub run_kind: RunKind,
    pub agent_id: Option<String>,
    pub workflow_id: Option<String>,
    pub user_message: String,
    pub final_response: String,
    pub trace: Trace,
    pub expected: Option<Value>,
    pub context: ork_core::a2a::AgentContext,
    pub scorer: Arc<dyn Scorer>,
    pub scorer_id: String,
    pub sampled_via: String,
}

/// Cheap, clonable producer side of the worker queue.
#[derive(Clone)]
pub struct LiveSamplerHandle {
    tx: mpsc::Sender<ScoreJob>,
    metrics: Arc<ScorerMetrics>,
}

impl LiveSamplerHandle {
    /// Enqueue a job. Returns `true` if the send succeeded; on a full
    /// queue, increments `scorer_dropped_total` and returns `false`.
    /// Never blocks.
    pub fn try_enqueue(&self, job: ScoreJob) -> bool {
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.dropped_total.inc();
                warn!("scorer queue full; dropping score job");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // ADR-0054 reviewer m3: surface worker-shutdown
                // separately from queue-full so alerts on
                // `scorer_dropped_total` never mask a dead worker.
                self.metrics.worker_closed_total.inc();
                warn!("scorer queue closed; dropping score job");
                false
            }
        }
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<ScorerMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Low-level constructor for tests / advanced consumers that
    /// supply their own bounded queue without going through
    /// [`spawn_worker`]. Production code should use
    /// [`spawn_worker`], which constructs both ends.
    #[doc(hidden)]
    #[must_use]
    pub fn from_sender(tx: mpsc::Sender<ScoreJob>, metrics: Arc<ScorerMetrics>) -> Self {
        Self { tx, metrics }
    }
}

/// Trait for the persistence side of scoring. The live worker writes
/// each [`ScoreCard`] result to a [`ScorerResultSink`]; production
/// uses the Postgres-backed implementation in `ork-persistence`,
/// tests use an in-memory sink.
#[async_trait::async_trait]
pub trait ScorerResultSink: Send + Sync {
    async fn record(&self, row: ScoredRow);
    /// ADR-0056: `GET /api/scorer-results` reads recent rows. Default
    /// returns empty so existing impls (e.g. fire-and-forget Kafka
    /// sinks) need not change. The in-memory sink overrides this for
    /// dev / test introspection; the Postgres-backed sink owned by
    /// ADR-0054's M1 follow-up will provide the real query.
    async fn list_recent(&self, _limit: usize) -> Vec<ScoredRow> {
        Vec::new()
    }
}

/// A scored result destined for `scorer_results` (persistence) and
/// the offline report writer.
///
/// Judge metadata (`judge_model`, `judge_input_tokens`,
/// `judge_output_tokens`) is surfaced as first-class fields so the
/// Postgres sink and Studio queries do not have to JSON-poke into
/// `details`. Deterministic scorers leave them `None`.
#[derive(Clone, Debug)]
pub struct ScoredRow {
    pub run_id: RunId,
    pub run_kind: RunKind,
    pub agent_id: Option<String>,
    pub workflow_id: Option<String>,
    pub scorer_id: String,
    pub score: f32,
    pub label: Option<String>,
    pub rationale: Option<String>,
    pub details: Value,
    pub scorer_duration_ms: u64,
    pub sampled_via: String,
    pub tenant_id: ork_common::types::TenantId,
    /// `provider/model` selector recorded on the judge call. `None`
    /// for deterministic scorers.
    pub judge_model: Option<String>,
    pub judge_input_tokens: Option<u32>,
    pub judge_output_tokens: Option<u32>,
}

/// Extract the judge metadata fields a judge scorer wrote into
/// `details`. Mirrors the keys [`crate::scorers::answer_relevancy`],
/// [`crate::scorers::faithfulness`], and [`crate::scorers::toxicity`]
/// emit so the worker can surface first-class columns on
/// [`ScoredRow`] without redoing the JSON dance in every sink.
fn extract_judge_metadata(details: &Value) -> (Option<String>, Option<u32>, Option<u32>) {
    let model = details
        .get("judge_model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let input = details
        .get("judge_input_tokens")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let output = details
        .get("judge_output_tokens")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    (model, input, output)
}

/// In-memory `ScorerResultSink` for tests, dev, and the v1
/// production default until ADR-0054's Postgres-backed sink lands
/// (reviewer M1, deferred). Stores scored rows behind a `Mutex<Vec>`
/// and exposes a snapshot via [`Self::rows`].
///
/// Production deployments wanting durable storage should swap this
/// with a Postgres-backed `ScorerResultSink` written through
/// `ork-persistence` once ADR-0054's M1 follow-up lands.
pub struct InMemoryScorerResultSink {
    rows: std::sync::Mutex<Vec<ScoredRow>>,
}

impl Default for InMemoryScorerResultSink {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryScorerResultSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Snapshot the rows accumulated so far. Cheap clone — the
    /// expected use is debugging and Studio's local-dev panel.
    #[must_use]
    pub fn rows(&self) -> Vec<ScoredRow> {
        self.rows.lock().map(|g| g.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.lock().map(|g| g.len()).unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl ScorerResultSink for InMemoryScorerResultSink {
    async fn record(&self, row: ScoredRow) {
        if let Ok(mut g) = self.rows.lock() {
            g.push(row);
        }
    }

    async fn list_recent(&self, limit: usize) -> Vec<ScoredRow> {
        let g = match self.rows.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let take = limit.min(g.len());
        let start = g.len().saturating_sub(take);
        g[start..].iter().rev().cloned().collect()
    }
}

/// Spawn a background worker that drains the queue and writes
/// results to `sink`. Returns the producer handle.
pub fn spawn_worker(
    sink: Arc<dyn ScorerResultSink>,
    metrics: Arc<ScorerMetrics>,
    capacity: usize,
) -> LiveSamplerHandle {
    let (tx, mut rx) = mpsc::channel::<ScoreJob>(capacity.max(1));
    let worker_metrics = Arc::clone(&metrics);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let started = std::time::Instant::now();
            let scorer_id = job.scorer.id().to_string();
            let input = ScoreInput {
                run_id: job.run_id,
                run_kind: job.run_kind,
                agent_id: job.agent_id.as_deref(),
                workflow_id: job.workflow_id.as_deref(),
                user_message: &job.user_message,
                final_response: &job.final_response,
                trace: &job.trace,
                expected: job.expected.as_ref(),
                context: &job.context,
            };
            match job.scorer.score(&input).await {
                Ok(card) => {
                    worker_metrics.processed_total.inc();
                    let (judge_model, judge_input_tokens, judge_output_tokens) =
                        extract_judge_metadata(&card.details);
                    let row = ScoredRow {
                        run_id: job.run_id,
                        run_kind: job.run_kind,
                        agent_id: job.agent_id.clone(),
                        workflow_id: job.workflow_id.clone(),
                        scorer_id,
                        score: card.score,
                        label: card.label,
                        rationale: card.rationale,
                        details: card.details,
                        scorer_duration_ms: started.elapsed().as_millis() as u64,
                        sampled_via: job.sampled_via.clone(),
                        tenant_id: job.context.tenant_id,
                        judge_model,
                        judge_input_tokens,
                        judge_output_tokens,
                    };
                    sink.record(row).await;
                }
                Err(e) => {
                    worker_metrics.failed_total.inc();
                    warn!(error = %e, scorer = %scorer_id, "scorer failed");
                }
            }
        }
    });
    LiveSamplerHandle { tx, metrics }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CollectingSink {
        rows: Mutex<Vec<ScoredRow>>,
    }

    #[async_trait::async_trait]
    impl ScorerResultSink for CollectingSink {
        async fn record(&self, row: ScoredRow) {
            self.rows.lock().unwrap().push(row);
        }
    }

    #[test]
    fn handle_drop_increments_counter_when_full() {
        // Build the runtime ourselves and never spawn a consumer so
        // the channel stays full at capacity 1.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let metrics = ScorerMetrics::new();
            let (tx, _rx) = mpsc::channel::<ScoreJob>(1);
            let handle = LiveSamplerHandle {
                tx: tx.clone(),
                metrics: Arc::clone(&metrics),
            };

            // Fill the channel with a sentinel sender that never reads.
            let dummy_scorer = NoopScorer;
            let dummy = make_job(Arc::new(dummy_scorer));
            tx.try_send(dummy).expect("first send fits");

            let dropped = make_job(Arc::new(NoopScorer));
            assert!(!handle.try_enqueue(dropped));
            assert_eq!(metrics.dropped_total.get(), 1);
        });
    }

    fn make_job(scorer: Arc<dyn Scorer>) -> ScoreJob {
        use ork_common::auth::{TrustClass, TrustTier};
        use ork_common::types::TenantId;
        use ork_core::a2a::TaskId;
        use tokio_util::sync::CancellationToken;
        ScoreJob {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a".into()),
            workflow_id: None,
            user_message: "u".into(),
            final_response: "r".into(),
            trace: Trace {
                user_message: "u".into(),
                tool_calls: vec![],
                started_at: chrono::Utc::now(),
                completed_at: chrono::Utc::now(),
            },
            expected: None,
            context: ork_core::a2a::AgentContext {
                tenant_id: TenantId(uuid::Uuid::nil()),
                task_id: TaskId::new(),
                parent_task_id: None,
                cancel: CancellationToken::new(),
                caller: ork_core::a2a::CallerIdentity {
                    tenant_id: TenantId(uuid::Uuid::nil()),
                    user_id: None,
                    scopes: vec![],
                    tenant_chain: vec![TenantId(uuid::Uuid::nil())],
                    trust_tier: TrustTier::Internal,
                    trust_class: TrustClass::User,
                    agent_id: None,
                },
                push_notification_url: None,
                trace_ctx: None,
                context_id: None,
                workflow_input: serde_json::Value::Null,
                iteration: None,
                delegation_depth: 0,
                delegation_chain: vec![],
                step_llm_overrides: None,
                artifact_store: None,
                artifact_public_base: None,
                resource_id: None,
                thread_id: None,
            },
            scorer,
            scorer_id: "noop".into(),
            sampled_via: "live:test".into(),
        }
    }

    struct NoopScorer;

    #[async_trait::async_trait]
    impl Scorer for NoopScorer {
        fn id(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "no-op"
        }
        fn schema(&self) -> ork_core::ports::scorer::ScoreSchema {
            ork_core::ports::scorer::ScoreSchema {
                id: "noop".into(),
                description: "no-op".into(),
                label_set: None,
                details: serde_json::Value::Null,
            }
        }
        async fn score(
            &self,
            _input: &ScoreInput<'_>,
        ) -> Result<ork_core::ports::scorer::ScoreCard, ork_common::error::OrkError> {
            Ok(ork_core::ports::scorer::ScoreCard {
                score: 1.0,
                label: None,
                rationale: None,
                details: serde_json::Value::Null,
            })
        }
    }

    #[tokio::test]
    async fn worker_processes_and_records() {
        let metrics = ScorerMetrics::new();
        let sink = Arc::new(CollectingSink {
            rows: Mutex::new(vec![]),
        });
        let handle = spawn_worker(
            Arc::clone(&sink) as Arc<dyn ScorerResultSink>,
            Arc::clone(&metrics),
            8,
        );
        let scorer: Arc<dyn Scorer> = Arc::new(NoopScorer);
        let job = make_job(scorer);
        assert!(handle.try_enqueue(job));
        // Yield to give the worker time to drain.
        for _ in 0..50 {
            if !sink.rows.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(sink.rows.lock().unwrap().len(), 1);
        assert_eq!(metrics.processed_total.get(), 1);
        assert_eq!(metrics.dropped_total.get(), 0);
    }
}
