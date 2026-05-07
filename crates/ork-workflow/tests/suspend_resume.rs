//! Suspend / resume and JSON-Schema validation (ADR-0050).

use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_run::WorkflowRunDeps;
use ork_core::ports::workflow_snapshot_memory::MemoryWorkflowSnapshotStore;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use ork_workflow::{StepOutcome, step, workflow};

fn root_ctx() -> AgentContext {
    let tenant = ork_common::types::TenantId::new();
    AgentContext {
        tenant_id: tenant,
        task_id: ork_core::a2a::TaskId::new(),
        parent_task_id: None,
        cancel: CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: tenant,
            user_id: None,
            scopes: vec![],
            ..CallerIdentity::default()
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: json!(null),
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
    }
}

async fn wait_suspended(
    h: &ork_core::ports::workflow_run::WorkflowRunHandle,
) -> ork_core::ports::workflow_run::RunState {
    for _ in 0..200 {
        let st = h.poll().await.expect("poll");
        if matches!(
            st,
            ork_core::ports::workflow_run::RunState::Suspended { .. }
        ) {
            return st;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for Suspended");
}

#[tokio::test]
async fn same_process_resume_completes() {
    let store = std::sync::Arc::new(MemoryWorkflowSnapshotStore::default());
    let s = step("gate")
        .input::<serde_json::Value>()
        .output::<serde_json::Value>()
        .execute(|ctx, _v| async move {
            if let Some(r) = ctx.run.resume_data.clone() {
                return Ok(StepOutcome::Done(r));
            }
            Ok(StepOutcome::Suspend {
                payload: json!({ "wait": true }),
                resume_schema: json!({
                    "type": "object",
                    "properties": { "ok": { "type": "boolean" } },
                    "required": ["ok"]
                }),
            })
        });
    let tail = step("tail")
        .input::<serde_json::Value>()
        .output::<serde_json::Value>()
        .execute(|_, v| async move { Ok(StepOutcome::Done(v)) });
    let w = workflow("w-suspend")
        .input::<serde_json::Value>()
        .output::<serde_json::Value>()
        .then(s)
        .then(tail)
        .commit();

    let deps = WorkflowRunDeps {
        snapshot_store: Some(store.clone()),
        agents: None,
        workflow_repo: None,
        tool_executor: None,
    };
    let h = w
        .run(root_ctx(), json!({"start": 1}), deps.clone())
        .await
        .expect("spawn");

    let _st = wait_suspended(&h).await;

    h.resume("gate", json!({ "ok": true }))
        .await
        .expect("resume");

    let done = h.await_done().await.expect("done");
    match done {
        ork_core::ports::workflow_run::RunState::Completed { output } => {
            assert_eq!(output, json!({ "ok": true }));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn resume_validation_rejects_bad_payload() {
    let store = std::sync::Arc::new(MemoryWorkflowSnapshotStore::default());
    let s = step("gate")
        .input::<serde_json::Value>()
        .output::<serde_json::Value>()
        .execute(|_, _| async move {
            Ok(StepOutcome::Suspend {
                payload: json!({}),
                resume_schema: json!({ "type": "string", "minLength": 1 }),
            })
        });
    let w = workflow("w-bad-resume")
        .input::<serde_json::Value>()
        .output::<serde_json::Value>()
        .then(s)
        .commit();
    let deps = WorkflowRunDeps {
        snapshot_store: Some(store),
        agents: None,
        workflow_repo: None,
        tool_executor: None,
    };
    let h = w.run(root_ctx(), json!(null), deps).await.expect("spawn");
    let _ = wait_suspended(&h).await;
    h.resume("gate", json!("")).await.expect("deliver resume");
    let done = h.await_done().await.expect("finished");
    match done {
        ork_core::ports::workflow_run::RunState::Failed { error } => {
            assert!(
                error.contains("validation") || error.contains("JSON Schema"),
                "unexpected error: {error}"
            );
        }
        other => panic!("expected Failed state, got {other:?}"),
    }
}
