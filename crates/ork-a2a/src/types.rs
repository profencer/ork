use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::ids::{ContextId, MessageId, TaskId};

pub type JsonObject = serde_json::Map<String, serde_json::Value>;

/// Wire-format security scheme object (OAuth2, API key, etc.); kept as JSON for forward compatibility.
pub type SecurityScheme = serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Base64String(pub String);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub url: Option<Url>,
    pub provider: Option<AgentProvider>,
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    pub security_schemes: Option<HashMap<String, SecurityScheme>>,
    pub security: Option<Vec<HashMap<String, Vec<String>>>>,
    pub extensions: Option<Vec<AgentExtension>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentProvider {
    pub organization: String,
    pub url: Url,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
    pub state_transition_history: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub examples: Vec<String>,
    pub input_modes: Option<Vec<String>>,
    pub output_modes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentExtension {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Free-form extension parameters (e.g. `kafka_request_topic` for the ork
    /// `transport-hint` extension; ADR-0005). Optional + `default` keeps older clients
    /// (and older serialised cards) parseable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<JsonObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonObject>,
    },
    Data {
        data: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonObject>,
    },
    File {
        file: FileRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonObject>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileRef {
    Bytes {
        name: Option<String>,
        mime_type: Option<String>,
        bytes: Base64String,
    },
    Uri {
        name: Option<String>,
        mime_type: Option<String>,
        uri: Url,
    },
}

impl Part {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            metadata: None,
        }
    }

    #[must_use]
    pub fn data(data: serde_json::Value) -> Self {
        Self::Data {
            data,
            metadata: None,
        }
    }

    /// Inline file as base64.
    #[must_use]
    pub fn file_bytes(bytes: impl Into<String>, mime_type: Option<String>) -> Self {
        Self::File {
            file: FileRef::Bytes {
                name: None,
                mime_type,
                bytes: Base64String(bytes.into()),
            },
            metadata: None,
        }
    }

    /// File referenced by URL.
    #[must_use]
    pub fn file_uri(uri: Url, mime_type: Option<String>) -> Self {
        Self::File {
            file: FileRef::Uri {
                name: None,
                mime_type,
                uri,
            },
            metadata: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Agent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    pub message_id: MessageId,
    pub task_id: Option<TaskId>,
    pub context_id: Option<ContextId>,
    pub metadata: Option<JsonObject>,
}

impl Message {
    /// New message from role and parts; generates a new [`MessageId`].
    #[must_use]
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Self {
            role,
            parts,
            message_id: MessageId::new(),
            task_id: None,
            context_id: None,
            metadata: None,
        }
    }

    /// User message with the given parts.
    #[must_use]
    pub fn user(parts: Vec<Part>) -> Self {
        Self::new(Role::User, parts)
    }

    /// Agent message with the given parts.
    #[must_use]
    pub fn agent(parts: Vec<Part>) -> Self {
        Self::new(Role::Agent, parts)
    }

    /// Single user turn with one [`Part::text`].
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::user(vec![Part::text(text)])
    }

    /// Single agent turn with one [`Part::text`].
    #[must_use]
    pub fn agent_text(text: impl Into<String>) -> Self {
        Self::agent(vec![Part::text(text)])
    }

    /// Empty message of the given role; useful as an accumulator before merging
    /// streamed events (ADR 0006 `agent_call` sync-stream path).
    #[must_use]
    pub fn empty(role: Role) -> Self {
        Self::new(role, Vec::new())
    }

    /// Fold an [`AgentEvent`](TaskEvent) into this message accumulator.
    ///
    /// `Message` events have their text parts concatenated with any existing trailing
    /// text part (or appended as a new `Text` part). `Data`/`File` parts are appended
    /// as-is. Status and artifact updates are ignored — those flow back through the
    /// SSE bridge, not the tool return value.
    pub fn merge_event(&mut self, ev: TaskEvent) {
        match ev {
            TaskEvent::Message(m) => {
                for part in m.parts {
                    match part {
                        Part::Text { text, metadata } => {
                            if let Some(Part::Text { text: existing, .. }) = self.parts.last_mut() {
                                existing.push_str(&text);
                            } else {
                                self.parts.push(Part::Text { text, metadata });
                            }
                        }
                        other => self.parts.push(other),
                    }
                }
                if self.context_id.is_none() {
                    self.context_id = m.context_id;
                }
                if self.task_id.is_none() {
                    self.task_id = m.task_id;
                }
            }
            TaskEvent::StatusUpdate(_) | TaskEvent::ArtifactUpdate(_) => {}
        }
    }

    /// Flatten this message into a tool-call return value:
    /// `{ "text": "...", "data": [..], "files": [..] }`. Empty subsections are omitted.
    /// Used by ADR 0006 `agent_call` to surface the peer's reply to the caller's LLM.
    #[must_use]
    pub fn to_tool_value(&self) -> serde_json::Value {
        let mut text_buf = String::new();
        let mut data_items: Vec<serde_json::Value> = Vec::new();
        let mut file_items: Vec<serde_json::Value> = Vec::new();
        for part in &self.parts {
            match part {
                Part::Text { text, .. } => text_buf.push_str(text),
                Part::Data { data, .. } => data_items.push(data.clone()),
                Part::File { file, .. } => match file {
                    FileRef::Bytes {
                        name,
                        mime_type,
                        bytes,
                    } => file_items.push(serde_json::json!({
                        "name": name,
                        "mime_type": mime_type,
                        "bytes": bytes.0,
                    })),
                    FileRef::Uri {
                        name,
                        mime_type,
                        uri,
                    } => file_items.push(serde_json::json!({
                        "name": name,
                        "mime_type": mime_type,
                        "uri": uri.as_str(),
                    })),
                },
            }
        }
        let mut out = serde_json::Map::new();
        if !text_buf.is_empty() {
            out.insert("text".into(), serde_json::Value::String(text_buf));
        }
        if !data_items.is_empty() {
            out.insert("data".into(), serde_json::Value::Array(data_items));
        }
        if !file_items.is_empty() {
            out.insert("files".into(), serde_json::Value::Array(file_items));
        }
        serde_json::Value::Object(out)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub context_id: ContextId,
    pub status: TaskStatus,
    pub history: Vec<Message>,
    pub artifacts: Vec<Artifact>,
    pub metadata: Option<JsonObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStatusUpdateEvent {
    pub task_id: TaskId,
    pub status: TaskStatus,
    #[serde(rename = "final", default)]
    pub is_final: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskArtifactUpdateEvent {
    pub task_id: TaskId,
    pub artifact: Artifact,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskEvent {
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
    Message(Message),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_text_serde_shape() {
        let p = Part::Text {
            text: "hi".to_string(),
            metadata: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v.get("kind"),
            Some(&serde_json::Value::String("text".to_string()))
        );
    }

    #[test]
    fn file_ref_bytes_vs_uri_serde() {
        let b = FileRef::Bytes {
            name: None,
            mime_type: Some("text/plain".to_string()),
            bytes: Base64String("YWI=".to_string()),
        };
        let u = FileRef::Uri {
            name: None,
            mime_type: Some("image/png".to_string()),
            uri: "https://example.com/f.png".parse().expect("url"),
        };
        let vb = serde_json::to_value(&b).unwrap();
        let vu = serde_json::to_value(&u).unwrap();
        assert!(vb.get("bytes").is_some() && vb.get("uri").is_none());
        assert!(vu.get("uri").is_some() && vu.get("bytes").is_none());
    }

    #[test]
    fn role_serde_lowercase() {
        let s = serde_json::to_string(&Role::User).unwrap();
        assert_eq!(s, "\"user\"");
    }

    #[test]
    fn agent_extension_params_round_trip() {
        let mut params = JsonObject::new();
        params.insert(
            "kafka_request_topic".into(),
            serde_json::Value::String("ork.a2a.v1.agent.request.planner".into()),
        );
        let ext = AgentExtension {
            uri: "https://ork.dev/a2a/extensions/transport-hint".into(),
            description: None,
            params: Some(params),
        };
        let v = serde_json::to_value(&ext).expect("serialize");
        assert_eq!(
            v.get("uri").and_then(|x| x.as_str()),
            Some("https://ork.dev/a2a/extensions/transport-hint")
        );
        assert_eq!(
            v.get("params")
                .and_then(|p| p.get("kafka_request_topic"))
                .and_then(|x| x.as_str()),
            Some("ork.a2a.v1.agent.request.planner")
        );
        assert!(
            v.get("description").is_none(),
            "description omitted when None"
        );

        let back: AgentExtension = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back.uri, ext.uri);
        assert_eq!(
            back.params
                .as_ref()
                .and_then(|p| p.get("kafka_request_topic"))
                .and_then(|x| x.as_str()),
            Some("ork.a2a.v1.agent.request.planner")
        );
    }

    #[test]
    fn agent_extension_params_default_when_missing() {
        // Older clients send no `params`; we must still deserialize cleanly.
        let json = serde_json::json!({ "uri": "https://example.com/ext" });
        let ext: AgentExtension = serde_json::from_value(json).expect("deserialize");
        assert!(ext.params.is_none());
        assert!(ext.description.is_none());
    }

    #[test]
    fn task_state_snake_case() {
        let t = TaskState::InputRequired;
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v, serde_json::json!("input_required"));
        let c = TaskState::Canceled;
        let vc = serde_json::to_value(&c).unwrap();
        assert_eq!(vc, serde_json::json!("canceled"));
    }
}
