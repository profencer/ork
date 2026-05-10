//! Shared fixtures for the per-scorer integration tests
//! ([ADR-0054](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md)).

#![allow(dead_code)]

use ork_common::auth::{TrustClass, TrustTier};
use ork_common::types::TenantId;
use ork_core::a2a::{AgentContext, CallerIdentity, TaskId};
use ork_core::ports::scorer::{ToolCallRecord, Trace};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub fn ctx() -> AgentContext {
    AgentContext {
        tenant_id: TenantId(uuid::Uuid::nil()),
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: TenantId(uuid::Uuid::nil()),
            user_id: None,
            scopes: vec![],
            tenant_chain: vec![TenantId(uuid::Uuid::nil())],
            trust_tier: TrustTier::Internal,
            trust_class: TrustClass::User,
            agent_id: None,
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}

pub fn trace(tool_calls: Vec<ToolCallRecord>) -> Trace {
    Trace {
        user_message: "u".into(),
        tool_calls,
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
    }
}

pub fn trace_lasting(duration_ms: u64, tool_calls: Vec<ToolCallRecord>) -> Trace {
    let started = chrono::Utc::now();
    let completed = started + chrono::Duration::milliseconds(duration_ms as i64);
    Trace {
        user_message: "u".into(),
        tool_calls,
        started_at: started,
        completed_at: completed,
    }
}
