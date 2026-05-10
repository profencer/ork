//! `latency_under` — passes when the run's wall-clock duration is at
//! or below the configured budget.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer};
use serde_json::json;

#[must_use]
pub fn latency_under(budget: Duration) -> LatencyUnderBuilder {
    LatencyUnderBuilder { budget }
}

pub struct LatencyUnderBuilder {
    budget: Duration,
}

impl LatencyUnderBuilder {
    #[must_use]
    pub fn build(self) -> Arc<dyn Scorer> {
        Arc::new(LatencyUnder {
            budget: self.budget,
        })
    }
}

struct LatencyUnder {
    budget: Duration,
}

#[async_trait]
impl Scorer for LatencyUnder {
    fn id(&self) -> &str {
        "latency_under"
    }

    fn description(&self) -> &str {
        "Pass when the run's wall-clock duration is within the configured budget."
    }

    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id().into(),
            description: self.description().into(),
            label_set: Some(vec!["under".into(), "over".into()]),
            details: json!({ "budget_ms": self.budget.as_millis() as u64 }),
        }
    }

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        let observed = input.trace.duration();
        let pass = observed <= self.budget;
        Ok(ScoreCard {
            score: if pass { 1.0 } else { 0.0 },
            label: Some(if pass { "under".into() } else { "over".into() }),
            rationale: (!pass).then(|| {
                format!(
                    "observed {} ms exceeded budget {} ms",
                    observed.as_millis(),
                    self.budget.as_millis()
                )
            }),
            details: json!({
                "observed_ms": observed.as_millis() as u64,
                "budget_ms": self.budget.as_millis() as u64,
            }),
        })
    }
}
