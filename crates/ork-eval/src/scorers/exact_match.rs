//! `exact_match` scorer — deterministic equality check on the final
//! response.
//!
//! Builder accepts:
//! - `.expected_string("...")` — compare against a fixed string
//! - `.expected_json_field("path.to.field")` — pull the expected
//!   string from the dataset's `expected` JSON
//! - `.case_sensitive(false)` — default true
//!
//! The scorer reads `ScoreInput::expected` first; only when no field
//! is configured does it fall back to the builder's static expected.

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer};
use serde_json::Value;

#[must_use]
pub fn exact_match() -> ExactMatchBuilder {
    ExactMatchBuilder::default()
}

#[derive(Default)]
pub struct ExactMatchBuilder {
    expected_string: Option<String>,
    expected_field: Option<String>,
    case_sensitive: Option<bool>,
}

impl ExactMatchBuilder {
    pub fn expected_string(mut self, s: impl Into<String>) -> Self {
        self.expected_string = Some(s.into());
        self
    }

    /// Dotted JSON path within the dataset example's `expected` value.
    /// e.g. `"answer"` reads `expected.answer` as a string.
    pub fn expected_field(mut self, path: impl Into<String>) -> Self {
        self.expected_field = Some(path.into());
        self
    }

    pub fn case_sensitive(mut self, on: bool) -> Self {
        self.case_sensitive = Some(on);
        self
    }

    #[must_use]
    pub fn build(self) -> Arc<dyn Scorer> {
        Arc::new(ExactMatch {
            expected_string: self.expected_string,
            expected_field: self.expected_field,
            case_sensitive: self.case_sensitive.unwrap_or(true),
        })
    }
}

struct ExactMatch {
    expected_string: Option<String>,
    expected_field: Option<String>,
    case_sensitive: bool,
}

impl ExactMatch {
    fn resolve_expected(&self, input: &ScoreInput<'_>) -> Option<String> {
        if let Some(path) = &self.expected_field
            && let Some(expected) = input.expected
        {
            return lookup_path(expected, path).map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
        self.expected_string.clone()
    }
}

fn lookup_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for segment in path.split('.') {
        cur = cur.get(segment)?;
    }
    Some(cur)
}

#[async_trait]
impl Scorer for ExactMatch {
    fn id(&self) -> &str {
        "exact_match"
    }

    fn description(&self) -> &str {
        "Deterministic equality between the final response and the configured expected string."
    }

    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id().into(),
            description: self.description().into(),
            label_set: Some(vec!["match".into(), "miss".into()]),
            details: serde_json::json!({ "case_sensitive": self.case_sensitive }),
        }
    }

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        let Some(expected) = self.resolve_expected(input) else {
            return Err(OrkError::Validation(
                "exact_match: no expected string configured and dataset row has none".into(),
            ));
        };
        let (lhs, rhs) = if self.case_sensitive {
            (input.final_response.to_string(), expected.clone())
        } else {
            (input.final_response.to_lowercase(), expected.to_lowercase())
        };
        let pass = lhs.trim() == rhs.trim();
        Ok(ScoreCard {
            score: if pass { 1.0 } else { 0.0 },
            label: Some(if pass { "match".into() } else { "miss".into() }),
            rationale: None,
            details: serde_json::json!({ "expected": expected }),
        })
    }
}
