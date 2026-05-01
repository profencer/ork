//! SSE wire helpers for workflow run events (ADR [`0050`](../../../docs/adrs/0050-code-first-workflow-dsl.md)).
//!
//! Payload shape matches [`TaskEvent::ArtifactUpdate`] with a single [`Part::Data`] typed part
//! (`spec: application/json` parity with ADR [`0003`](../../../docs/adrs/0003-a2a-protocol-model.md) streaming).

use ork_a2a::{Artifact, JsonRpcResponse, Part, TaskArtifactUpdateEvent, TaskEvent, TaskId};
use ork_common::types::WorkflowRunId;
use ork_core::ports::workflow_run::WorkflowEvent;

/// Serialize a [`WorkflowEvent`] into the same JSON-RPC + SSE **data** shape used by
/// [`crate::routes::a2a`]: `JsonRpcResponse<TaskEvent>`.
pub fn encode_workflow_event_sse_payload(
    jsonrpc_id: Option<serde_json::Value>,
    run_id: WorkflowRunId,
    event: &WorkflowEvent,
) -> Result<Vec<u8>, serde_json::Error> {
    let ev_json = serde_json::to_value(event)?;
    let task_event = TaskEvent::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: TaskId(run_id.0),
        artifact: Artifact {
            artifact_id: format!("workflow-run-{}", run_id.0),
            name: Some("workflow_event".into()),
            description: None,
            parts: vec![Part::data(ev_json)],
            metadata: None,
        },
    });
    serde_json::to_vec(&JsonRpcResponse::ok(jsonrpc_id, task_event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_jsonrpc_ok_wrapping_artifact_update_with_data_part() {
        let run_id = WorkflowRunId::new();
        let ev = WorkflowEvent::StepStarted {
            step_id: "s1".into(),
            input: json!({"x": 1}),
        };
        let bytes = encode_workflow_event_sse_payload(Some(json!("rid-1")), run_id, &ev)
            .expect("serialize");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "rid-1");
        assert!(v["error"].is_null());
        let result = v["result"].as_object().expect("result object");
        assert_eq!(result["kind"], "artifact_update");
        let parts = result["artifact"]["parts"].as_array().expect("parts array");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["kind"], "data");
        assert_eq!(parts[0]["data"]["kind"], "step_started");
        assert_eq!(parts[0]["data"]["step_id"], "s1");
    }
}
