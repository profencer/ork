//! Wire schema for the `agent_call` tool from
//! ADR [`0006`](../../../docs/adrs/0006-peer-delegation.md) §`a) agent_call tool — minimum-viable peer delegation`.
//!
//! Lives in `ork-a2a` (not `ork-integrations`) so the parent A2A model and the tool's
//! input shape stay in one crate; the tool executor in `ork-integrations` consumes
//! [`AgentCallInput`] but doesn't own the schema.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::types::{FileRef, Message, Part, Role};

/// Strongly-typed view of the JSON input passed to the `agent_call` tool. The schema
/// lives in the ADR and is pinned by the unit tests at the bottom of this file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCallInput {
    /// Target agent id (resolved via the registry; ADR 0005).
    pub agent: String,
    /// Free-text prompt, becomes a [`Part::Text`].
    pub prompt: String,
    /// Optional structured payload, becomes a [`Part::Data`] when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Optional file references, become [`Part::File`]s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileRef>,
    /// Whether the caller blocks on the child task. Default `true` per ADR 0006.
    #[serde(default = "default_true", rename = "await")]
    pub await_: bool,
    /// Whether to forward intermediate `AgentEvent`s back to the caller's status channel.
    /// Default `false` per ADR 0006 to keep tool-call semantics.
    #[serde(default)]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

/// Errors raised when the JSON input fails the ADR-0006 schema.
///
/// Local error type (rather than `OrkError`) keeps `ork-a2a` free of an `ork-common` dep
/// and matches the rest of this crate's error story (`thiserror`-only).
#[derive(Debug, Error)]
pub enum AgentCallInputError {
    #[error("agent_call: input must be a JSON object")]
    NotAnObject,
    #[error("agent_call: missing required field `{0}`")]
    MissingField(&'static str),
    #[error("agent_call: invalid field `{field}`: {reason}")]
    InvalidField { field: &'static str, reason: String },
}

impl AgentCallInput {
    /// Parse a JSON value into a validated [`AgentCallInput`].
    pub fn from_value(input: &Value) -> Result<Self, AgentCallInputError> {
        let obj = input.as_object().ok_or(AgentCallInputError::NotAnObject)?;

        let agent = obj
            .get("agent")
            .ok_or(AgentCallInputError::MissingField("agent"))?
            .as_str()
            .ok_or(AgentCallInputError::InvalidField {
                field: "agent",
                reason: "must be a string".into(),
            })?
            .to_string();
        if agent.is_empty() {
            return Err(AgentCallInputError::InvalidField {
                field: "agent",
                reason: "must not be empty".into(),
            });
        }

        let prompt = obj
            .get("prompt")
            .ok_or(AgentCallInputError::MissingField("prompt"))?
            .as_str()
            .ok_or(AgentCallInputError::InvalidField {
                field: "prompt",
                reason: "must be a string".into(),
            })?
            .to_string();

        let data = match obj.get("data") {
            None | Some(Value::Null) => None,
            Some(v) if v.is_object() => Some(v.clone()),
            Some(_) => {
                return Err(AgentCallInputError::InvalidField {
                    field: "data",
                    reason: "must be a JSON object".into(),
                });
            }
        };

        let files = match obj.get("files") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    serde_json::from_value::<FileRef>(item.clone()).map_err(|e| {
                        AgentCallInputError::InvalidField {
                            field: "files",
                            reason: format!("invalid file ref: {e}"),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(AgentCallInputError::InvalidField {
                    field: "files",
                    reason: "must be an array".into(),
                });
            }
        };

        let await_ = match obj.get("await") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(AgentCallInputError::InvalidField {
                    field: "await",
                    reason: "must be a boolean".into(),
                });
            }
        };

        let stream = match obj.get("stream") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(AgentCallInputError::InvalidField {
                    field: "stream",
                    reason: "must be a boolean".into(),
                });
            }
        };

        Ok(Self {
            agent,
            prompt,
            data,
            files,
            await_,
            stream,
        })
    }

    /// Build the [`Message`] that will be sent to the target agent. Always [`Role::User`]
    /// (the parent agent is the user from the child's perspective).
    #[must_use]
    pub fn into_message(self) -> Message {
        let mut parts = Vec::with_capacity(1 + usize::from(self.data.is_some()) + self.files.len());
        parts.push(Part::Text {
            text: self.prompt,
            metadata: None,
        });
        if let Some(data) = self.data {
            parts.push(Part::Data {
                data,
                metadata: None,
            });
        }
        for f in self.files {
            parts.push(Part::File {
                file: f,
                metadata: None,
            });
        }
        Message::new(Role::User, parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_for_await_and_stream() {
        let input = AgentCallInput::from_value(&json!({
            "agent": "researcher",
            "prompt": "look this up",
        }))
        .expect("valid");
        assert_eq!(input.agent, "researcher");
        assert_eq!(input.prompt, "look this up");
        assert!(input.await_, "await defaults to true");
        assert!(!input.stream, "stream defaults to false");
        assert!(input.data.is_none());
        assert!(input.files.is_empty());
    }

    #[test]
    fn rejects_missing_agent() {
        let err = AgentCallInput::from_value(&json!({ "prompt": "hi" })).unwrap_err();
        assert!(matches!(err, AgentCallInputError::MissingField("agent")));
    }

    #[test]
    fn rejects_missing_prompt() {
        let err = AgentCallInput::from_value(&json!({ "agent": "r" })).unwrap_err();
        assert!(matches!(err, AgentCallInputError::MissingField("prompt")));
    }

    #[test]
    fn rejects_empty_agent() {
        let err = AgentCallInput::from_value(&json!({ "agent": "", "prompt": "hi" })).unwrap_err();
        assert!(matches!(
            err,
            AgentCallInputError::InvalidField { field: "agent", .. }
        ));
    }

    #[test]
    fn data_part_round_trip_into_message() {
        let input = AgentCallInput::from_value(&json!({
            "agent": "researcher",
            "prompt": "look at this",
            "data": { "topic": "rust async" },
            "await": false,
            "stream": true,
        }))
        .expect("valid");
        assert!(!input.await_);
        assert!(input.stream);

        let msg = input.into_message();
        assert!(matches!(msg.role, Role::User));
        assert_eq!(msg.parts.len(), 2);
        assert!(matches!(&msg.parts[0], Part::Text { text, .. } if text == "look at this"));
        assert!(matches!(&msg.parts[1], Part::Data { .. }));
    }

    #[test]
    fn file_uri_part_round_trip() {
        let input = AgentCallInput::from_value(&json!({
            "agent": "researcher",
            "prompt": "ingest these",
            "files": [
                { "uri": "https://example.com/x.pdf", "mime_type": "application/pdf" }
            ],
        }))
        .expect("valid");
        let msg = input.into_message();
        assert_eq!(msg.parts.len(), 2);
        assert!(matches!(&msg.parts[1], Part::File { .. }));
    }

    #[test]
    fn rejects_non_object_data() {
        let err = AgentCallInput::from_value(&json!({
            "agent": "r", "prompt": "hi", "data": [1, 2]
        }))
        .unwrap_err();
        assert!(matches!(
            err,
            AgentCallInputError::InvalidField { field: "data", .. }
        ));
    }
}
