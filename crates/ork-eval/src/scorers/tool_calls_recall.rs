//! `tool_calls_recall` — fraction of expected tool invocations that
//! actually occurred during the run.
//!
//! Each [`ToolCallExpectation`] is matched against `Trace::tool_calls`
//! by name (and, when configured, by an `args_match` JSON predicate).
//! `score = matched / expected.len()`. With an empty expectation
//! list the scorer returns 1.0 (vacuously satisfied) and an empty
//! `details.misses`.

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer, ToolCallRecord};
use serde_json::{Value, json};

#[must_use]
pub fn tool_calls_recall(expected: Vec<ToolCallExpectation>) -> ToolCallsRecallBuilder {
    ToolCallsRecallBuilder { expected }
}

#[derive(Clone, Debug)]
pub struct ToolCallExpectation {
    pub name: String,
    /// When `Some`, the call's `args` must equal this value (deep
    /// equality). When `None`, name match is sufficient.
    pub args_match: Option<Value>,
}

impl ToolCallExpectation {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args_match: None,
        }
    }

    #[must_use]
    pub fn with_args(mut self, args: Value) -> Self {
        self.args_match = Some(args);
        self
    }
}

pub struct ToolCallsRecallBuilder {
    expected: Vec<ToolCallExpectation>,
}

impl ToolCallsRecallBuilder {
    #[must_use]
    pub fn build(self) -> Arc<dyn Scorer> {
        Arc::new(ToolCallsRecall {
            expected: self.expected,
        })
    }
}

struct ToolCallsRecall {
    expected: Vec<ToolCallExpectation>,
}

fn matches(expectation: &ToolCallExpectation, call: &ToolCallRecord) -> bool {
    if expectation.name != call.name {
        return false;
    }
    match &expectation.args_match {
        None => true,
        Some(expected_args) => expected_args == &call.args,
    }
}

#[async_trait]
impl Scorer for ToolCallsRecall {
    fn id(&self) -> &str {
        "tool_calls_recall"
    }

    fn description(&self) -> &str {
        "Fraction of expected tool calls observed in the run trace."
    }

    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id().into(),
            description: self.description().into(),
            label_set: None,
            details: json!({ "expected": self.expected.iter().map(|e| &e.name).collect::<Vec<_>>() }),
        }
    }

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        if self.expected.is_empty() {
            return Ok(ScoreCard {
                score: 1.0,
                label: Some("vacuous".into()),
                rationale: None,
                details: json!({ "matched": 0, "expected": 0, "misses": [] }),
            });
        }

        let mut matched = 0usize;
        let mut misses: Vec<String> = Vec::new();
        for expectation in &self.expected {
            if input
                .trace
                .tool_calls
                .iter()
                .any(|c| matches(expectation, c))
            {
                matched += 1;
            } else {
                misses.push(expectation.name.clone());
            }
        }
        let score = matched as f32 / self.expected.len() as f32;
        Ok(ScoreCard {
            score,
            label: None,
            rationale: (!misses.is_empty()).then(|| format!("missed: {}", misses.join(", "))),
            details: json!({
                "matched": matched,
                "expected": self.expected.len(),
                "misses": misses,
            }),
        })
    }
}
