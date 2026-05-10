//! ADR-0054 acceptance criterion `Live sampling`.
//!
//! Verifies the live-sampling sub-system end-to-end **without** booting
//! a full agent stack: we drive `LiveAgentScoringHook` directly with
//! synthetic [`Trace`] inputs. The three sub-tests cover the criterion:
//!
//! - `score_rows_land_for_sampled_runs` — the worker drains the queue
//!   and writes a row through `ScorerResultSink`.
//! - `user_facing_latency_unchanged_within_5ms` — the hook returns
//!   well within budget against a no-scorer baseline.
//! - `dropped_jobs_increment_scorer_dropped_total` — when the bounded
//!   channel is full, jobs are dropped and the metric increments.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ork_agents::hooks::RunCompleteHook;
use ork_common::auth::{TrustClass, TrustTier};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity, TaskId};
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer, ToolCallRecord, Trace};
use ork_eval::agent_hook::{LiveAgentScoringHook, LiveBinding};
use ork_eval::live::{ScoreJob, ScoredRow, ScorerResultSink, spawn_worker};
use ork_eval::metrics::ScorerMetrics;
use ork_eval::sampling::Sampling;
use ork_eval::spec::{ScorerSpec, ScorerTarget};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct CollectingSink {
    rows: Mutex<Vec<ScoredRow>>,
}

#[async_trait]
impl ScorerResultSink for CollectingSink {
    async fn record(&self, row: ScoredRow) {
        self.rows.lock().unwrap().push(row);
    }
}

struct PassthroughScorer {
    id: &'static str,
}

#[async_trait]
impl Scorer for PassthroughScorer {
    fn id(&self) -> &str {
        self.id
    }
    fn description(&self) -> &str {
        "test passthrough"
    }
    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id.into(),
            description: "test passthrough".into(),
            label_set: None,
            details: serde_json::Value::Null,
        }
    }
    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        Ok(ScoreCard {
            score: if input.final_response.is_empty() {
                0.0
            } else {
                1.0
            },
            label: None,
            rationale: None,
            details: serde_json::json!({ "len": input.final_response.len() }),
        })
    }
}

fn make_ctx() -> AgentContext {
    AgentContext {
        tenant_id: TenantId(uuid::Uuid::nil()),
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
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
    }
}

fn make_trace(tool_calls: Vec<ToolCallRecord>) -> Trace {
    Trace {
        user_message: "hello".into(),
        tool_calls,
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn score_rows_land_for_sampled_runs() {
    let metrics = ScorerMetrics::new();
    let sink = Arc::new(CollectingSink {
        rows: Mutex::new(vec![]),
    });
    let sampler = spawn_worker(
        Arc::clone(&sink) as Arc<dyn ScorerResultSink>,
        Arc::clone(&metrics),
        16,
    );

    let scorer: Arc<dyn Scorer> = Arc::new(PassthroughScorer { id: "passthrough" });
    let bindings = vec![LiveBinding::new(
        ScorerTarget::agent("weather"),
        ScorerSpec::live(scorer, Sampling::Ratio { rate: 1.0 }),
    )];
    let hook = LiveAgentScoringHook::new("weather", bindings, sampler);

    let ctx = make_ctx();
    for _ in 0..5 {
        hook.on_run_complete(
            &ctx,
            "what's it like in SF?",
            "sunny",
            &make_trace(vec![]),
            None,
        )
        .await;
    }

    // Wait for the worker to drain.
    for _ in 0..50 {
        if sink.rows.lock().unwrap().len() == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sink.rows.lock().unwrap().len(), 5);
    assert_eq!(metrics.processed_total.get(), 5);
    assert_eq!(metrics.dropped_total.get(), 0);

    let row = sink.rows.lock().unwrap()[0].clone();
    assert_eq!(row.scorer_id, "passthrough");
    assert!(row.agent_id.as_deref() == Some("weather"));
    assert_eq!(row.sampled_via, "live:ratio");
    assert!((row.score - 1.0).abs() < f32::EPSILON);
}

#[tokio::test]
async fn user_facing_latency_unchanged_within_5ms() {
    // Baseline: hook with no bindings.
    let metrics = ScorerMetrics::new();
    let sink = Arc::new(CollectingSink {
        rows: Mutex::new(vec![]),
    });
    let sampler = spawn_worker(
        Arc::clone(&sink) as Arc<dyn ScorerResultSink>,
        Arc::clone(&metrics),
        128,
    );
    let baseline_hook = LiveAgentScoringHook::new("weather", vec![], sampler.clone());

    let ctx = make_ctx();
    let baseline = measure_avg(&baseline_hook, &ctx, 50).await;

    // Scored: 100% Ratio sampling against the same passthrough scorer.
    let scorer: Arc<dyn Scorer> = Arc::new(PassthroughScorer { id: "passthrough" });
    let scored_hook = LiveAgentScoringHook::new(
        "weather",
        vec![LiveBinding::new(
            ScorerTarget::agent("weather"),
            ScorerSpec::live(scorer, Sampling::Ratio { rate: 1.0 }),
        )],
        sampler,
    );
    let scored = measure_avg(&scored_hook, &ctx, 50).await;

    // The criterion is "user-facing response latency unaffected
    // within ±5 ms". The hook is what runs on the user-facing path;
    // its overhead must be a small fixed cost. We compare scored vs
    // baseline averages across 50 iterations.
    let delta = scored.as_secs_f64() - baseline.as_secs_f64();
    assert!(
        delta.abs() < 0.005,
        "scored hook overhead {scored:?} vs baseline {baseline:?} (delta {delta} s) exceeds 5ms"
    );
}

async fn measure_avg(hook: &LiveAgentScoringHook, ctx: &AgentContext, iters: usize) -> Duration {
    let mut total = Duration::ZERO;
    let trace = make_trace(vec![]);
    for _ in 0..iters {
        let started = Instant::now();
        hook.on_run_complete(ctx, "u", "r", &trace, None).await;
        total += started.elapsed();
    }
    total / iters as u32
}

#[tokio::test]
async fn dropped_jobs_increment_scorer_dropped_total() {
    let metrics = ScorerMetrics::new();
    // Build a sampler whose channel is capacity 1, and prefill it
    // with a sentinel that the (absent) worker never drains. We use
    // `LiveSamplerHandle` directly because `spawn_worker` wires a
    // worker that *would* drain the queue.
    let (tx, _rx) = mpsc::channel::<ScoreJob>(1);
    let handle = ork_eval::live::LiveSamplerHandle::from_sender(tx.clone(), Arc::clone(&metrics));

    // Fill capacity.
    let scorer: Arc<dyn Scorer> = Arc::new(PassthroughScorer { id: "passthrough" });
    let job = make_job(Arc::clone(&scorer));
    tx.try_send(job).expect("first send fits");

    let scored_hook = LiveAgentScoringHook::new(
        "weather",
        vec![LiveBinding::new(
            ScorerTarget::agent("weather"),
            ScorerSpec::live(Arc::clone(&scorer), Sampling::Ratio { rate: 1.0 }),
        )],
        handle,
    );

    let ctx = make_ctx();
    // 3 follow-up runs all see a full channel and increment the counter.
    for _ in 0..3 {
        scored_hook
            .on_run_complete(&ctx, "u", "r", &make_trace(vec![]), None)
            .await;
    }
    assert_eq!(metrics.dropped_total.get(), 3);
}

fn make_job(scorer: Arc<dyn Scorer>) -> ScoreJob {
    use ork_core::ports::scorer::{RunId, RunKind};
    ScoreJob {
        run_id: RunId::new(),
        run_kind: RunKind::Agent,
        agent_id: Some("weather".into()),
        workflow_id: None,
        user_message: "u".into(),
        final_response: "r".into(),
        trace: make_trace(vec![]),
        expected: None,
        context: make_ctx(),
        scorer,
        scorer_id: "passthrough".into(),
        sampled_via: "live:test".into(),
    }
}
