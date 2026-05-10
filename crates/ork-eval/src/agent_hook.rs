//! `LiveAgentScoringHook` — implements [`ork_agents::RunCompleteHook`]
//! and dispatches matching scorer bindings into the live worker
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Live sampling`).
//!
//! Bindings live on the parent `OrkApp`; this hook is constructed at
//! `OrkApp::build()` time with an `Arc` to a snapshot of the
//! `Live` / `Both` bindings and the [`crate::live::LiveSamplerHandle`]
//! produced by [`crate::live::spawn_worker`]. Every call to
//! [`Self::on_run_complete`] is `O(bindings)` and never blocks.

use std::sync::Arc;

use async_trait::async_trait;
use ork_agents::hooks::RunCompleteHook;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::scorer::{RunId, RunKind, Trace};

use crate::live::{LiveSamplerHandle, ScoreJob};
use crate::sampling::SamplingState;
use crate::spec::{ScorerSpec, ScorerTarget};

/// One entry in the hook's binding table — a registered scorer that
/// fires live on agent runs whose id matches `target`.
pub struct LiveBinding {
    pub target: ScorerTarget,
    pub spec: ScorerSpec,
    /// Per-binding token bucket / RNG state for the
    /// [`crate::sampling::Sampling`] predicate.
    pub state: Arc<SamplingState>,
}

impl LiveBinding {
    #[must_use]
    pub fn new(target: ScorerTarget, spec: ScorerSpec) -> Self {
        Self {
            target,
            spec,
            state: Arc::new(SamplingState::default()),
        }
    }
}

pub struct LiveAgentScoringHook {
    agent_id: String,
    bindings: Vec<LiveBinding>,
    sampler: LiveSamplerHandle,
}

impl LiveAgentScoringHook {
    /// `agent_id` is the id of the agent this hook is attached to —
    /// used both as the binding match target and as the value
    /// recorded on `scorer_results.agent_id`.
    #[must_use]
    pub fn new(
        agent_id: impl Into<String>,
        bindings: Vec<LiveBinding>,
        sampler: LiveSamplerHandle,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            bindings,
            sampler,
        }
    }
}

#[async_trait]
impl RunCompleteHook for LiveAgentScoringHook {
    async fn on_run_complete(
        &self,
        ctx: &AgentContext,
        user_message: &str,
        final_text: &str,
        trace: &Trace,
        error: Option<&OrkError>,
    ) {
        let errored = error.is_some();
        for binding in &self.bindings {
            if !binding.target.matches_agent(&self.agent_id) {
                continue;
            }
            if !binding.spec.fires_live() {
                continue;
            }
            let Some(sampling) = binding.spec.sampling() else {
                continue;
            };
            if !sampling.should_fire(errored, &binding.state) {
                continue;
            }
            let scorer = binding.spec.scorer().clone();
            let scorer_id = scorer.id().to_string();
            let job = ScoreJob {
                run_id: RunId::new(),
                run_kind: RunKind::Agent,
                agent_id: Some(self.agent_id.clone()),
                workflow_id: None,
                user_message: user_message.to_string(),
                final_response: final_text.to_string(),
                trace: trace.clone(),
                expected: None,
                context: ctx.clone(),
                scorer,
                scorer_id,
                sampled_via: sampled_via_label(sampling),
            };
            self.sampler.try_enqueue(job);
        }
    }
}

fn sampled_via_label(s: &crate::sampling::Sampling) -> String {
    match s {
        crate::sampling::Sampling::Ratio { .. } => "live:ratio".into(),
        crate::sampling::Sampling::PerMinute { .. } => "live:per_minute".into(),
        crate::sampling::Sampling::OnError => "live:on_error".into(),
        crate::sampling::Sampling::Never => "live:never".into(),
    }
}
