//! ADR-0056 §`Streaming`: central SSE encoder shared by Studio
//! (ADR-0055), the WebUI gateway (ADR-0017), and the auto-generated
//! agent/workflow stream endpoints.
//!
//! The on-the-wire shape (named events with JSON bodies) follows
//! ADR-0003. The mapping from the [`AgentEvent`] (= [`ork_a2a::TaskEvent`])
//! emitted by [`Agent::send_stream`](ork_core::ports::agent::Agent::send_stream)
//! to the named events is:
//!
//! | `TaskEvent` variant      | event name      | body shape              |
//! |--------------------------|-----------------|-------------------------|
//! | `StatusUpdate { final: false }`     | `status`        | `{kind, state}`         |
//! | `StatusUpdate { final: true }`      | `completed`     | `{kind, state, run_id?}`|
//! | `Message` (text parts)              | `delta`         | `{kind, text}`          |
//! | `Message` (data part with `tool_call`) | `tool_call`  | `{kind, id, name, args}`|
//! | `Message` (data part with `tool_result`) | `tool_result` | `{kind, id, output}` |
//! | `ArtifactUpdate`                    | `artifact`      | `{kind, artifact}`      |
//!
//! Unknown variants surface as `event: status` to keep older clients
//! parsing.

use axum::response::sse::Event;
use ork_a2a::{Part, TaskEvent, TaskState};
use serde_json::{Value, json};

/// Encode a single [`AgentEvent`] into the public SSE shape.
///
/// `run_id` is threaded into the terminal `completed` event so clients
/// don't need a separate POST/GET to correlate the stream with the
/// `AgentGenerateOutput`.
pub fn encode_agent_event(event: &TaskEvent, run_id: Option<&str>) -> Event {
    match event {
        TaskEvent::StatusUpdate(s) => {
            let state = task_state_str(s.status.state);
            if s.is_final {
                let mut body = json!({ "kind": "completed", "state": state });
                if let Some(rid) = run_id {
                    body["run_id"] = Value::String(rid.into());
                }
                Event::default().event("completed").data(body.to_string())
            } else {
                Event::default()
                    .event("status")
                    .data(json!({ "kind": "status", "state": state }).to_string())
            }
        }
        TaskEvent::Message(m) => {
            // Walk parts; emit a delta for any concatenated text, then
            // tool_call / tool_result for any data-part with the
            // canonical `kind` discriminator.
            let mut text_buf = String::new();
            let mut emitted: Option<Event> = None;
            for part in &m.parts {
                match part {
                    Part::Text { text, .. } => text_buf.push_str(text),
                    Part::Data { data, .. } => {
                        if let Some(ev) = data_part_event(data) {
                            emitted = Some(ev);
                        }
                    }
                    Part::File { .. } => {}
                }
            }
            if let Some(ev) = emitted {
                ev
            } else if !text_buf.is_empty() {
                Event::default()
                    .event("delta")
                    .data(json!({ "kind": "delta", "text": text_buf }).to_string())
            } else {
                Event::default()
                    .event("status")
                    .data(json!({ "kind": "status", "state": "working" }).to_string())
            }
        }
        TaskEvent::ArtifactUpdate(a) => {
            let body = json!({
                "kind": "artifact",
                "artifact": serde_json::to_value(&a.artifact).unwrap_or(Value::Null),
            });
            Event::default().event("artifact").data(body.to_string())
        }
    }
}

fn task_state_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::InputRequired => "input_required",
        TaskState::AuthRequired => "auth_required",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::Rejected => "rejected",
    }
}

fn data_part_event(v: &Value) -> Option<Event> {
    let kind = v.get("kind").and_then(|k| k.as_str())?;
    match kind {
        "tool_call" => Some(Event::default().event("tool_call").data(v.to_string())),
        "tool_result" => Some(Event::default().event("tool_result").data(v.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::{Message, Role, TaskId, TaskStatus, TaskStatusUpdateEvent};

    fn extract_data(ev: &Event) -> String {
        // axum's Event has no public getters, so we serialize it through
        // its Display-equivalent path: build a string by re-encoding.
        // Cheap shortcut: rebuild the wire form via the public API.
        format!("{ev:?}")
    }

    #[test]
    fn status_update_non_final_maps_to_status_event() {
        let event = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: TaskId::new(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
            },
            is_final: false,
        });
        let ev = encode_agent_event(&event, None);
        let dbg = extract_data(&ev);
        assert!(dbg.contains("status"));
    }

    #[test]
    fn status_update_final_maps_to_completed_event_with_run_id() {
        let event = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: TaskId::new(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
            },
            is_final: true,
        });
        let ev = encode_agent_event(&event, Some("r-123"));
        let dbg = extract_data(&ev);
        assert!(dbg.contains("completed"));
        assert!(dbg.contains("r-123"));
    }

    #[test]
    fn message_with_text_part_maps_to_delta_event() {
        let event = TaskEvent::Message(Message::new(
            Role::Agent,
            vec![Part::text("The temperature in")],
        ));
        let ev = encode_agent_event(&event, None);
        let dbg = extract_data(&ev);
        assert!(dbg.contains("delta"));
        assert!(dbg.contains("The temperature in"));
    }

    #[test]
    fn message_with_tool_call_data_part_maps_to_tool_call_event() {
        let data = json!({
            "kind": "tool_call",
            "id": "call_1",
            "name": "weather.lookup",
            "args": { "city": "SF" }
        });
        let event = TaskEvent::Message(Message::new(Role::Agent, vec![Part::data(data)]));
        let ev = encode_agent_event(&event, None);
        let dbg = extract_data(&ev);
        assert!(dbg.contains("tool_call"));
        assert!(dbg.contains("weather.lookup"));
    }

    #[test]
    fn message_with_tool_result_data_part_maps_to_tool_result_event() {
        let data = json!({
            "kind": "tool_result",
            "id": "call_1",
            "output": { "temp_f": 67 }
        });
        let event = TaskEvent::Message(Message::new(Role::Agent, vec![Part::data(data)]));
        let ev = encode_agent_event(&event, None);
        let dbg = extract_data(&ev);
        assert!(dbg.contains("tool_result"));
    }
}
