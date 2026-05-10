//! `json_schema_match` — does the final response parse as JSON and
//! conform to the configured JSON Schema?

use std::sync::Arc;

use async_trait::async_trait;
use jsonschema::Validator;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{ScoreCard, ScoreInput, ScoreSchema, Scorer};
use serde_json::Value;

/// Build a scorer that requires the final response to be valid JSON
/// matching `schema`. The schema is compiled at build time.
#[must_use]
pub fn json_schema_match(schema: Value) -> JsonSchemaMatchBuilder {
    JsonSchemaMatchBuilder { schema }
}

pub struct JsonSchemaMatchBuilder {
    schema: Value,
}

impl JsonSchemaMatchBuilder {
    pub fn build(self) -> Arc<dyn Scorer> {
        let schema_json = self.schema;
        let validator = jsonschema::Validator::new(&schema_json).ok();
        Arc::new(JsonSchemaMatch {
            validator,
            schema: schema_json,
        })
    }
}

struct JsonSchemaMatch {
    validator: Option<Validator>,
    schema: Value,
}

#[async_trait]
impl Scorer for JsonSchemaMatch {
    fn id(&self) -> &str {
        "json_schema_match"
    }

    fn description(&self) -> &str {
        "Final response must parse as JSON and validate against the configured JSON Schema."
    }

    fn schema(&self) -> ScoreSchema {
        ScoreSchema {
            id: self.id().into(),
            description: self.description().into(),
            label_set: Some(vec!["valid".into(), "invalid".into()]),
            details: self.schema.clone(),
        }
    }

    async fn score(&self, input: &ScoreInput<'_>) -> Result<ScoreCard, OrkError> {
        let Some(validator) = &self.validator else {
            return Err(OrkError::Validation(
                "json_schema_match: schema failed to compile".into(),
            ));
        };

        let parsed = match serde_json::from_str::<Value>(input.final_response) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ScoreCard {
                    score: 0.0,
                    label: Some("invalid".into()),
                    rationale: Some(format!("response is not JSON: {e}")),
                    details: serde_json::json!({ "stage": "parse" }),
                });
            }
        };

        let pass = validator.is_valid(&parsed);
        Ok(ScoreCard {
            score: if pass { 1.0 } else { 0.0 },
            label: Some(if pass {
                "valid".into()
            } else {
                "invalid".into()
            }),
            rationale: (!pass).then(|| "response did not match the configured JSON Schema".into()),
            details: serde_json::json!({ "valid": pass }),
        })
    }
}
