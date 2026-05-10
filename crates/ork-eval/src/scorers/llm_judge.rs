//! `LlmProviderJudge` — `LlmProvider`-backed concrete [`Judge`] used
//! by `answer_relevancy`, `faithfulness`, and `toxicity`
//! ([ADR-0054](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Built-in scorers`).
//!
//! The ADR's wire shape is a typed `(score, rationale)` output. We
//! ship that contract via `LlmProvider::chat`: the judge prompt is
//! sent with a system suffix asking the model to emit JSON matching
//! [`JudgeOutput`], and the response text is parsed.
//!
//! This route deliberately uses ork's `LlmProvider` port instead of
//! constructing a `rig::Extractor<M, JudgeOutput>` directly so judge
//! calls inherit ork's tenant catalog routing
//! ([`ork_llm::router::LlmRouter`]) and cost accounting. The
//! `JudgeOutput` `JsonSchema` derive carries the structural contract
//! a `rig::Extractor` would otherwise enforce.

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
#[allow(unused_imports)]
use ork_core::ports::llm::MessageRole;
use ork_core::ports::llm::{ChatMessage, ChatRequest, LlmProvider}; // re-exported for tests/external consumers

use crate::scorers::judge::{Judge, JudgeOutput, JudgeResponse, JudgeUsage};

const JUDGE_FORMAT_SUFFIX: &str = "\n\nReply with a single JSON object on one line, of the form {\"score\": <float between 0.0 and 1.0>, \"rationale\": <one-sentence string>}. Do not wrap in code fences.";

pub struct LlmProviderJudge {
    llm: Arc<dyn LlmProvider>,
    /// `provider/model` selector recorded on `scorer_results.judge_model`.
    judge_model_label: String,
    /// Provider id passed to `LlmRouter::resolve` (parsed from the
    /// `provider/model` selector). `None` means "use the tenant
    /// default", matching ork's existing fallback chain.
    provider: Option<String>,
    /// Model id passed to the resolved provider.
    model: Option<String>,
}

impl LlmProviderJudge {
    /// Construct from a parsed `provider/model` selector. Either side
    /// may be empty (`/gpt-4o-mini`, `openai/`) — empties become
    /// `None`, falling through to tenant/operator defaults.
    pub fn new(llm: Arc<dyn LlmProvider>, judge_model: impl Into<String>) -> Self {
        let label = judge_model.into();
        let (provider, model) = parse_selector(&label);
        Self {
            llm,
            judge_model_label: label,
            provider,
            model,
        }
    }
}

fn parse_selector(s: &str) -> (Option<String>, Option<String>) {
    match s.split_once('/') {
        Some((p, m)) => (
            (!p.is_empty()).then(|| p.to_string()),
            (!m.is_empty()).then(|| m.to_string()),
        ),
        None => (None, (!s.is_empty()).then(|| s.to_string())),
    }
}

#[async_trait]
impl Judge for LlmProviderJudge {
    fn judge_model(&self) -> &str {
        &self.judge_model_label
    }

    async fn judge(&self, prompt: &str) -> Result<JudgeResponse, OrkError> {
        let request = ChatRequest::simple(
            vec![ChatMessage::user(format!("{prompt}{JUDGE_FORMAT_SUFFIX}"))],
            Some(0.0),
            Some(512),
            self.model.clone(),
        );
        let request = ChatRequest {
            provider: self.provider.clone(),
            ..request
        };
        let response = self.llm.chat(request).await?;
        let output = parse_judge_output(&response.content)?;
        Ok(JudgeResponse {
            output,
            usage: JudgeUsage {
                prompt_tokens: Some(response.usage.prompt_tokens),
                completion_tokens: Some(response.usage.completion_tokens),
            },
        })
    }
}

fn parse_judge_output(text: &str) -> Result<JudgeOutput, OrkError> {
    let trimmed = text.trim();
    // Be tolerant: some models wrap JSON in code fences even when told
    // not to. Strip a leading/trailing ``` block when present.
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    serde_json::from_str::<JudgeOutput>(stripped).map_err(|e| {
        OrkError::LlmProvider(format!(
            "judge model returned non-JudgeOutput JSON: {e} (raw: {trimmed})"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let out = parse_judge_output(r#"{"score": 0.8, "rationale": "good"}"#).unwrap();
        assert!((out.score - 0.8).abs() < f32::EPSILON);
        assert_eq!(out.rationale, "good");
    }

    #[test]
    fn strips_json_code_fence() {
        let out =
            parse_judge_output("```json\n{\"score\": 0.5, \"rationale\": \"x\"}\n```").unwrap();
        assert!((out.score - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn errors_on_garbage() {
        let err = parse_judge_output("not json").unwrap_err();
        assert!(matches!(err, OrkError::LlmProvider(_)));
    }

    #[test]
    fn parses_selector_components() {
        assert_eq!(
            parse_selector("openai/gpt-4o-mini"),
            (Some("openai".into()), Some("gpt-4o-mini".into()))
        );
        assert_eq!(
            parse_selector("gpt-4o-mini"),
            (None, Some("gpt-4o-mini".into()))
        );
        assert_eq!(parse_selector("openai/"), (Some("openai".into()), None));
    }
}
