//! Working-memory shape validation.
//!
//! ADR 0053 §`Working memory shapes` defines three shapes:
//! [`WorkingMemoryShape::Free`] (no validation),
//! [`WorkingMemoryShape::Schema`] (caller-supplied JSON schema), and
//! [`WorkingMemoryShape::User`] (a pre-baked
//! `{ name?, preferences?, goals? }` shape Mastra ships by default).
//!
//! The enum lives in `ork-core::ports::memory_store`; this module
//! provides the validation helpers backends call before persisting a
//! write.

use ork_common::error::OrkError;
use ork_core::ports::memory_store::WorkingMemoryShape;
use serde_json::Value;

/// Validate `value` against `shape`. Returns
/// [`OrkError::Validation`] when the value does not match.
pub fn validate(shape: &WorkingMemoryShape, value: &Value) -> Result<(), OrkError> {
    match shape {
        WorkingMemoryShape::Free => Ok(()),
        WorkingMemoryShape::User => validate_user_shape(value),
        WorkingMemoryShape::Schema(schema) => validate_with_schema(schema, value),
    }
}

fn validate_user_shape(value: &Value) -> Result<(), OrkError> {
    let obj = value.as_object().ok_or_else(|| {
        OrkError::Validation("working memory: User shape requires a JSON object".into())
    })?;
    for (k, v) in obj {
        match k.as_str() {
            "name" => {
                if !v.is_string() {
                    return Err(OrkError::Validation(
                        "working memory: `name` must be a string".into(),
                    ));
                }
            }
            "preferences" | "goals" => {
                // Permissive on these — strings, arrays, or objects are
                // all reasonable shapes for free-form preferences/goals.
                if v.is_null() {
                    return Err(OrkError::Validation(format!(
                        "working memory: `{k}` must not be null"
                    )));
                }
            }
            _ => {
                return Err(OrkError::Validation(format!(
                    "working memory: `{k}` is not a recognised User-shape field; \
                     use `Free` or `Schema(...)` for arbitrary keys"
                )));
            }
        }
    }
    Ok(())
}

fn validate_with_schema(schema: &Value, value: &Value) -> Result<(), OrkError> {
    let compiled = jsonschema::Validator::new(schema)
        .map_err(|e| OrkError::Validation(format!("invalid working-memory schema: {e}")))?;
    if !compiled.is_valid(value) {
        return Err(OrkError::Validation(
            "working memory does not satisfy schema".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn free_shape_accepts_anything() {
        assert!(validate(&WorkingMemoryShape::Free, &json!({"x": 1})).is_ok());
        assert!(validate(&WorkingMemoryShape::Free, &json!("just a string")).is_ok());
    }

    #[test]
    fn user_shape_rejects_unknown_keys() {
        let err = validate(
            &WorkingMemoryShape::User,
            &json!({"name": "a", "favourite_colour": "blue"}),
        )
        .unwrap_err();
        assert!(matches!(err, OrkError::Validation(_)));
    }

    #[test]
    fn user_shape_requires_name_to_be_string() {
        let err = validate(&WorkingMemoryShape::User, &json!({"name": 42})).unwrap_err();
        assert!(matches!(err, OrkError::Validation(_)));
    }

    #[test]
    fn schema_shape_compiles_and_rejects() {
        let schema = json!({
            "type": "object",
            "properties": {"score": {"type": "integer", "minimum": 0}},
            "required": ["score"]
        });
        assert!(
            validate(
                &WorkingMemoryShape::Schema(schema.clone()),
                &json!({"score": 5})
            )
            .is_ok()
        );
        let err = validate(&WorkingMemoryShape::Schema(schema), &json!({"score": -1})).unwrap_err();
        assert!(matches!(err, OrkError::Validation(_)));
    }
}
