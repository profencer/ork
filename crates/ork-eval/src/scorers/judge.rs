//! Shared LLM-as-judge primitives
//! ([ADR-0054](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Built-in scorers`).
//!
//! `answer_relevancy`, `faithfulness`, and `toxicity` all share the
//! same shape: prompt the judge model, parse a `(score, rationale)`
//! structured output, return it as a [`ScoreCard`]. This module owns
//! the structured-output type and the [`Judge`] trait the scorers
//! talk to. The production-wiring impl that wraps `rig::Extractor`
//! lives at [`crate::scorers::rig_judge`]; tests stub the trait
//! directly.

use async_trait::async_trait;
use ork_common::error::OrkError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Typed output every LLM-as-judge prompt must emit.
///
/// `score ∈ [0.0, 1.0]`. The `rationale` is shown verbatim in
/// Studio's scorer panel and the offline report; it is free-form
/// English and not used as a score discriminator.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct JudgeOutput {
    pub score: f32,
    pub rationale: String,
}

/// Token usage captured from the judge LLM call. Recorded on
/// `scorer_results.judge_input_tokens` / `judge_output_tokens` for
/// the cost-accounting pillar of ADR-0054 §`scorer_results table`.
/// `None` when the judge backend does not surface usage (e.g. a
/// scripted test stub).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JudgeUsage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

/// Combined return type for a single judge invocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgeResponse {
    pub output: JudgeOutput,
    pub usage: JudgeUsage,
}

/// Adapter the judge scorers call into. Production: a rig
/// `Extractor`-backed impl. Tests: a scripted impl that returns a
/// fixed [`JudgeOutput`].
#[async_trait]
pub trait Judge: Send + Sync {
    /// The judge model identifier (e.g. `"openai/gpt-4o-mini"`).
    /// Recorded on `scorer_results.judge_model` for cost auditing.
    fn judge_model(&self) -> &str;

    /// Submit `prompt` and parse the response into a [`JudgeResponse`]
    /// (carrying both the typed output and token usage when
    /// available). Errors propagate as `OrkError::LlmProvider` per
    /// ADR convention.
    async fn judge(&self, prompt: &str) -> Result<JudgeResponse, OrkError>;
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use std::sync::Mutex;

    /// Test-only judge that returns the next pre-scripted output on
    /// each call. Panics if exhausted.
    pub struct ScriptedJudge {
        model: String,
        outputs: Mutex<Vec<JudgeOutput>>,
    }

    impl ScriptedJudge {
        pub fn new(model: impl Into<String>, outputs: Vec<JudgeOutput>) -> Self {
            Self {
                model: model.into(),
                outputs: Mutex::new(outputs),
            }
        }
    }

    #[async_trait]
    impl Judge for ScriptedJudge {
        fn judge_model(&self) -> &str {
            &self.model
        }

        async fn judge(&self, _prompt: &str) -> Result<JudgeResponse, OrkError> {
            let mut g = self.outputs.lock().expect("scripted judge poisoned");
            Ok(JudgeResponse {
                output: g.remove(0),
                usage: JudgeUsage::default(),
            })
        }
    }
}
