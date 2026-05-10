//! `answer_relevancy` — LLM-as-judge: how well does the final response
//! address the user's message?

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer};
use serde_json::json;

use crate::scorers::judge::Judge;

const PROMPT_TEMPLATE: &str = r#"You are an evaluator scoring how well an assistant's answer addresses the user's question.

USER MESSAGE:
{user}

ASSISTANT FINAL RESPONSE:
{response}

Score relevancy on a 0.0 to 1.0 scale. 1.0 = directly and completely addresses the question. 0.0 = irrelevant or non-responsive.
Return JSON {"score": <float>, "rationale": <one short sentence>}."#;

#[must_use]
pub fn answer_relevancy() -> AnswerRelevancyBuilder {
    AnswerRelevancyBuilder::default()
}

#[derive(Default)]
pub struct AnswerRelevancyBuilder {
    judge: Option<Arc<dyn Judge>>,
    judge_model_override: Option<String>,
}

impl AnswerRelevancyBuilder {
    /// Inject a [`Judge`]. ork-eval ships no concrete judge; callers
    /// wire one up out-of-crate (e.g.
    /// [`crate::scorers::LlmProviderJudge`]) so the scorer crate
    /// stays free of LLM-client deps beyond the port.
    pub fn judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Override the `provider/model` selector recorded on
    /// `scorer_results.judge_model`. Optional — when omitted, the
    /// injected [`Judge`]'s `judge_model()` is used.
    pub fn judge_model(mut self, model: impl Into<String>) -> Self {
        self.judge_model_override = Some(model.into());
        self
    }

    /// Construct the scorer. Returns `OrkError::Configuration` when
    /// `.judge(...)` was not supplied — surfaces builder misuse as a
    /// typed error instead of a runtime panic (ADR-0054 reviewer m6).
    pub fn try_build(self) -> Result<Arc<dyn Scorer>, OrkError> {
        let judge = self.judge.ok_or_else(|| OrkError::Configuration {
            message: "answer_relevancy: .judge(...) is required before .build()".into(),
        })?;
        Ok(Arc::new(AnswerRelevancy {
            judge,
            judge_model_override: self.judge_model_override,
        }))
    }

    /// Infallible alias for `try_build().expect(...)`. Keeps the
    /// ADR's `.build()` ergonomics; production callers wanting a
    /// typed error path use [`Self::try_build`].
    #[must_use]
    pub fn build(self) -> Arc<dyn Scorer> {
        self.try_build().expect("answer_relevancy")
    }
}

struct AnswerRelevancy {
    judge: Arc<dyn Judge>,
    /// `.judge_model(...)` override, recorded on the `ScoreCard` so
    /// it shows up in `scorer_results.judge_model` (m7).
    judge_model_override: Option<String>,
}

impl AnswerRelevancy {
    fn judge_model(&self) -> &str {
        self.judge_model_override
            .as_deref()
            .unwrap_or_else(|| self.judge.judge_model())
    }
}

#[async_trait]
impl Scorer for AnswerRelevancy {
    fn id(&self) -> &str {
        "answer_relevancy"
    }

    fn description(&self) -> &str {
        "LLM-as-judge: 1.0 means the final response directly addresses the user's message."
    }

    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id().into(),
            description: self.description().into(),
            label_set: None,
            details: json!({ "judge_model": self.judge_model() }),
        }
    }

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        let prompt = PROMPT_TEMPLATE
            .replace("{user}", input.user_message)
            .replace("{response}", input.final_response);
        let resp = self.judge.judge(&prompt).await?;
        Ok(ScoreCard {
            score: resp.output.score.clamp(0.0, 1.0),
            label: None,
            rationale: Some(resp.output.rationale),
            details: json!({
                "judge_model": self.judge_model(),
                "judge_input_tokens": resp.usage.prompt_tokens,
                "judge_output_tokens": resp.usage.completion_tokens,
            }),
        })
    }
}
