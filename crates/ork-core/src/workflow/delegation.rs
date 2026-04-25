//! Peer-delegation execution helpers shared by [`WorkflowEngine`](super::engine::WorkflowEngine)
//! and the `agent_call` tool executor (ADR
//! [`0006`](../../../../docs/adrs/0006-peer-delegation.md)).
//!
//! This module owns the per-call logic so `delegate_to:` step execution and the LLM
//! `agent_call` tool route through the same code path:
//!
//! 1. Resolve the target through [`AgentRegistry`].
//! 2. Build a child [`AgentContext`] (depth+cycle checked).
//! 3. For local sync (`await:true`) targets — call `send_stream` and accumulate.
//! 4. For remote sync — return `OrkError::Unsupported` (waits on ADR 0007).
//! 5. For fire-and-forget (`await:false`) — publish via [`DelegationPublisher`].
//! 6. Persist the child task row to [`A2aTaskRepository`] in both async paths.
//!
//! Child workflows are *not* handled here; the engine forks a sub-engine inline because
//! that needs the `WorkflowRepository` and the compiled workflow loader.

use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use ork_a2a::{A2aMethod, AgentCallInput, JsonRpcRequest, MessageSendParams, TaskState};
use ork_common::error::OrkError;
use serde_json::Value;

use crate::a2a::{AgentContext, AgentEvent, AgentMessage};
use crate::agent_registry::AgentRegistry;
use crate::ports::a2a_task_repo::{A2aTaskRepository, A2aTaskRow};
use crate::ports::delegation_publisher::DelegationPublisher;

/// Outcome of a one-shot peer delegation. `task_id` is the child A2A task id; for sync
/// completions `reply` carries the accumulated child message, for fire-and-forget it is
/// the empty message returned by [`AgentMessage::empty`] and the caller should treat
/// completion as "request submitted".
pub struct DelegationOutcome {
    pub task_id: ork_a2a::TaskId,
    pub reply: AgentMessage,
    pub status: TaskState,
}

impl DelegationOutcome {
    /// Surface the outcome as the JSON value returned by `agent_call`.
    /// Shape: `{ "task_id": "<uuid>", "status": "<state>", "reply": { ... } }`.
    #[must_use]
    pub fn to_tool_value(&self) -> Value {
        serde_json::json!({
            "task_id": self.task_id.to_string(),
            "status": match self.status {
                TaskState::Submitted => "submitted",
                TaskState::Working => "working",
                TaskState::InputRequired => "input_required",
                TaskState::AuthRequired => "auth_required",
                TaskState::Completed => "completed",
                TaskState::Failed => "failed",
                TaskState::Canceled => "canceled",
                TaskState::Rejected => "rejected",
            },
            "reply": self.reply.to_tool_value(),
        })
    }
}

/// Convert `AgentCallInput` validation errors at the boundary; we cannot `impl From`
/// (orphan rule — both types live in foreign crates).
pub fn map_input_err(e: ork_a2a::AgentCallInputError) -> OrkError {
    OrkError::Validation(e.to_string())
}

/// Run a one-shot delegation. Used by both the engine and the `agent_call` tool;
/// child-workflow delegations are handled inside the engine and skip this helper.
///
/// `parent_ctx` is the caller's [`AgentContext`]; `target_call` carries the parsed
/// input. `workflow_run_id` is recorded on the `a2a_tasks` row when present (the
/// `agent_call` tool path may pass `None` if the caller is not a workflow step).
pub async fn execute_one_shot_delegation(
    parent_ctx: &AgentContext,
    registry: &AgentRegistry,
    publisher: Option<&Arc<dyn DelegationPublisher>>,
    a2a_tasks: Option<&Arc<dyn A2aTaskRepository>>,
    workflow_run_id: Option<ork_common::types::WorkflowRunId>,
    target_call: AgentCallInput,
) -> Result<DelegationOutcome, OrkError> {
    let target_id = target_call.agent.clone();

    if !registry.knows(&target_id).await {
        return Err(OrkError::NotFound(format!(
            "rejected: unknown agent '{target_id}' for delegation"
        )));
    }

    let child_ctx = parent_ctx.child_for_delegation(&target_id)?;
    let child_task_id = child_ctx.task_id;
    let await_ = target_call.await_;
    let mut child_msg = target_call.into_message();
    child_msg.task_id = Some(child_task_id);
    child_msg.context_id = child_ctx.context_id;

    if !await_ {
        // Fire-and-forget. Try local resolver: if local, we *could* spawn a task instead,
        // but for symmetry with remote agents we always go through the publisher. When
        // there's no broker configured, return `Unsupported` (the API would not register
        // a publisher in that case; tests pass an in-memory backend).
        let publisher = publisher.ok_or_else(|| {
            OrkError::Unsupported(
                "fire-and-forget delegation requires a configured DelegationPublisher".into(),
            )
        })?;

        // Persist the child task row before publishing so a status query that races the
        // worker still finds the row.
        if let Some(repo) = a2a_tasks {
            let now = Utc::now();
            repo.create_task(&A2aTaskRow {
                id: child_task_id,
                context_id: parent_ctx
                    .context_id
                    .unwrap_or_else(ork_a2a::ContextId::new),
                tenant_id: parent_ctx.tenant_id,
                agent_id: target_id.clone(),
                parent_task_id: Some(parent_ctx.task_id),
                workflow_run_id,
                state: TaskState::Submitted,
                metadata: serde_json::json!({}),
                created_at: now,
                updated_at: now,
                completed_at: None,
            })
            .await?;
        }

        let payload = JsonRpcRequest::new(
            Some(serde_json::Value::String(child_task_id.to_string())),
            A2aMethod::MessageSend,
            Some(MessageSendParams {
                message: child_msg,
                configuration: None,
                metadata: None,
            }),
        );
        let payload_bytes = serde_json::to_vec(&payload)
            .map_err(|e| OrkError::Internal(format!("serialize JsonRpcRequest: {e}")))?;

        publisher
            .publish_request(&target_id, child_task_id, &payload_bytes)
            .await?;

        return Ok(DelegationOutcome {
            task_id: child_task_id,
            reply: AgentMessage::empty(ork_a2a::Role::Agent),
            status: TaskState::Submitted,
        });
    }

    // await:true. Local agents resolve directly; remote agents resolve through the
    // remote slot populated by an ADR-0007 builder. Either way the call is the same.
    let agent = registry.resolve(&target_id).await.ok_or_else(|| {
        OrkError::Unsupported(format!(
            "sync delegation requires a callable agent registered under '{target_id}' \
             (local or remote materialised via RemoteAgentBuilder)"
        ))
    })?;

    if let Some(repo) = a2a_tasks {
        let now = Utc::now();
        repo.create_task(&A2aTaskRow {
            id: child_task_id,
            context_id: parent_ctx
                .context_id
                .unwrap_or_else(ork_a2a::ContextId::new),
            tenant_id: parent_ctx.tenant_id,
            agent_id: target_id.clone(),
            parent_task_id: Some(parent_ctx.task_id),
            workflow_run_id,
            state: TaskState::Working,
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
        .await?;
    }

    let mut stream = agent.send_stream(child_ctx, child_msg).await?;
    let mut accumulator = AgentMessage::empty(ork_a2a::Role::Agent);
    let mut errored: Option<OrkError> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(AgentEvent::Message(m)) => {
                accumulator.merge_event(ork_a2a::TaskEvent::Message(m));
            }
            Ok(AgentEvent::StatusUpdate(_) | AgentEvent::ArtifactUpdate(_)) => {}
            Err(e) => {
                errored = Some(e);
                break;
            }
        }
    }

    let final_state = if errored.is_some() {
        TaskState::Failed
    } else {
        TaskState::Completed
    };

    if let Some(repo) = a2a_tasks {
        let _ = repo
            .update_state(parent_ctx.tenant_id, child_task_id, final_state)
            .await;
    }

    if let Some(e) = errored {
        return Err(e);
    }

    Ok(DelegationOutcome {
        task_id: child_task_id,
        reply: accumulator,
        status: final_state,
    })
}
