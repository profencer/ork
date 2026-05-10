//! `cost_under` — best-effort cost-budget scorer.
//!
//! v1 reads the cost in USD from `ScoreInput::trace.tool_calls`'
//! aggregated `details.cost_usd` (when surfaced by the agent's
//! telemetry; ADR-0058 will broaden this). When no cost figure is
//! available, the scorer returns a neutral `0.0` with a `details.note`
//! flagging that cost accounting was unavailable, rather than passing
//! the run silently. Customers can opt out by not registering this
//! scorer until ADR-0058 lands.

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer};
use serde_json::json;

#[must_use]
pub fn cost_under(usd: f32) -> CostUnderBuilder {
    CostUnderBuilder { budget_usd: usd }
}

pub struct CostUnderBuilder {
    budget_usd: f32,
}

impl CostUnderBuilder {
    #[must_use]
    pub fn build(self) -> Arc<dyn Scorer> {
        Arc::new(CostUnder {
            budget_usd: self.budget_usd,
        })
    }
}

struct CostUnder {
    budget_usd: f32,
}

#[async_trait]
impl Scorer for CostUnder {
    fn id(&self) -> &str {
        "cost_under"
    }

    fn description(&self) -> &str {
        "Pass when the run's reported cost (USD) is within the configured budget."
    }

    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id().into(),
            description: self.description().into(),
            label_set: Some(vec!["under".into(), "over".into(), "unknown".into()]),
            details: json!({ "budget_usd": self.budget_usd }),
        }
    }

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        let mut total = 0.0f32;
        let mut saw_any = false;
        for call in &input.trace.tool_calls {
            if let Some(cost) = call
                .result
                .get("cost_usd")
                .and_then(|v| v.as_f64())
                .or_else(|| call.result.get("cost").and_then(|v| v.as_f64()))
            {
                total += cost as f32;
                saw_any = true;
            }
        }
        if !saw_any {
            return Ok(ScoreCard {
                score: 0.0,
                label: Some("unknown".into()),
                rationale: Some(
                    "no cost_usd available on tool calls; ADR-0058 will surface this".into(),
                ),
                details: json!({ "note": "cost accounting unavailable" }),
            });
        }
        let pass = total <= self.budget_usd;
        Ok(ScoreCard {
            score: if pass { 1.0 } else { 0.0 },
            label: Some(if pass { "under".into() } else { "over".into() }),
            rationale: (!pass).then(|| {
                format!(
                    "observed ${total:.4} exceeded budget ${:.4}",
                    self.budget_usd
                )
            }),
            details: json!({ "observed_usd": total, "budget_usd": self.budget_usd }),
        })
    }
}
