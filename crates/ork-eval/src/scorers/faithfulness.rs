//! `faithfulness` — LLM-as-judge: does the final response stay
//! grounded in the supplied context (no fabrication)?

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer};
use serde_json::json;

use crate::scorers::judge::Judge;

const PROMPT_TEMPLATE: &str = r#"You are an evaluator scoring whether an assistant's answer stays faithful to its supporting context (no fabricated facts).

USER MESSAGE:
{user}

CONTEXT (tool calls / retrieved documents):
{context}

ASSISTANT FINAL RESPONSE:
{response}

Score faithfulness on a 0.0 to 1.0 scale. 1.0 = every claim is supported by the context. 0.0 = the response is largely fabricated.
Return JSON {"score": <float>, "rationale": <one short sentence>}."#;

#[must_use]
pub fn faithfulness() -> FaithfulnessBuilder {
    FaithfulnessBuilder::default()
}

#[derive(Default)]
pub struct FaithfulnessBuilder {
    judge: Option<Arc<dyn Judge>>,
    judge_model_override: Option<String>,
}

impl FaithfulnessBuilder {
    pub fn judge(mut self, judge: Arc<dyn Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    pub fn judge_model(mut self, model: impl Into<String>) -> Self {
        self.judge_model_override = Some(model.into());
        self
    }

    pub fn try_build(self) -> Result<Arc<dyn Scorer>, OrkError> {
        let judge = self.judge.ok_or_else(|| OrkError::Configuration {
            message: "faithfulness: .judge(...) is required before .build()".into(),
        })?;
        Ok(Arc::new(Faithfulness {
            judge,
            judge_model_override: self.judge_model_override,
        }))
    }

    #[must_use]
    pub fn build(self) -> Arc<dyn Scorer> {
        self.try_build().expect("faithfulness")
    }
}

struct Faithfulness {
    judge: Arc<dyn Judge>,
    judge_model_override: Option<String>,
}

impl Faithfulness {
    fn judge_model(&self) -> &str {
        self.judge_model_override
            .as_deref()
            .unwrap_or_else(|| self.judge.judge_model())
    }
}

#[async_trait]
impl Scorer for Faithfulness {
    fn id(&self) -> &str {
        "faithfulness"
    }

    fn description(&self) -> &str {
        "LLM-as-judge: 1.0 means every claim in the response is supported by the run's context."
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
        let context_blob =
            serde_json::to_string(&input.trace.tool_calls).unwrap_or_else(|_| "[]".into());
        let prompt = PROMPT_TEMPLATE
            .replace("{user}", input.user_message)
            .replace("{context}", &context_blob)
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
