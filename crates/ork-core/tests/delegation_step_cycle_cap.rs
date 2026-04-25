//! ADR-0006 §`Consequences/Negative` — recursion-bound and cycle-detection guards.
//!
//! Depth cap and cycle detection live in [`AgentContext::child_for_delegation`]; the
//! shared [`execute_one_shot_delegation`] helper bubbles those errors back out so
//! both the `agent_call` tool and the engine see the same `OrkError::Workflow`
//! string. The engine's per-step `delegate_to` always starts at depth 0, so the
//! integration test exercises the helper directly with a pre-saturated context.

mod common;

use ork_a2a::AgentCallInput;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::context::MAX_DELEGATION_DEPTH;
use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::agent_registry::AgentRegistry;
use ork_core::workflow::delegation::execute_one_shot_delegation;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::common::echo_agent_with_prefix;

fn root_ctx(tenant: TenantId) -> AgentContext {
    AgentContext {
        tenant_id: tenant,
        task_id: ork_a2a::TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: vec![],
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: serde_json::Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: Vec::new(),
    }
}

#[tokio::test]
async fn delegating_above_max_depth_returns_workflow_error() {
    let tenant = TenantId(Uuid::nil());
    let registry = AgentRegistry::from_agents(vec![echo_agent_with_prefix("target", "t:")]);

    let mut ctx = root_ctx(tenant);
    // Pre-saturate the chain so the next delegation hop is exactly at the cap.
    ctx.delegation_depth = MAX_DELEGATION_DEPTH;
    ctx.delegation_chain = (0..MAX_DELEGATION_DEPTH)
        .map(|i| format!("hop-{i}"))
        .collect();

    let input = AgentCallInput {
        agent: "target".into(),
        prompt: "hi".into(),
        data: None,
        files: Vec::new(),
        await_: true,
        stream: false,
    };

    let res = execute_one_shot_delegation(&ctx, &registry, None, None, None, input).await;
    match res {
        Ok(_) => panic!("delegation must reject above the cap"),
        Err(OrkError::Workflow(msg)) => {
            assert!(
                msg.contains("max_delegation_depth"),
                "error must mention the cap; got: {msg}"
            );
        }
        Err(other) => panic!("expected Workflow error, got {other:?}"),
    }
}

#[tokio::test]
async fn delegating_to_agent_already_in_chain_is_rejected_as_cycle() {
    let tenant = TenantId(Uuid::nil());
    let registry = AgentRegistry::from_agents(vec![echo_agent_with_prefix("planner", "p:")]);

    let mut ctx = root_ctx(tenant);
    ctx.delegation_depth = 1;
    ctx.delegation_chain = vec!["planner".into()];

    let input = AgentCallInput {
        agent: "planner".into(),
        prompt: "loop me".into(),
        data: None,
        files: Vec::new(),
        await_: true,
        stream: false,
    };

    let res = execute_one_shot_delegation(&ctx, &registry, None, None, None, input).await;
    match res {
        Ok(_) => panic!("delegation must detect the cycle"),
        Err(OrkError::Workflow(msg)) => {
            assert!(
                msg.to_lowercase().contains("cycle"),
                "error must mention cycle; got: {msg}"
            );
        }
        Err(other) => panic!("expected Workflow error, got {other:?}"),
    }
}
