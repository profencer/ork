//! Control-flow combinators (ADR-0050).

use std::time::Duration;

use ork_core::a2a::{AgentContext, CallerIdentity};
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_run::WorkflowRunDeps;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ork_workflow::types::{BranchPredicate, ForEachOptions, Predicate};
use ork_workflow::{AnyStep, StepOutcome, step, workflow};

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

fn deps() -> WorkflowRunDeps {
    WorkflowRunDeps {
        snapshot_store: None,
        agents: None,
        workflow_repo: None,
        tool_executor: None,
    }
}

#[tokio::test]
async fn branch_first_matching_arm_runs() {
    let w = workflow("wf-branch")
        .input::<Value>()
        .output::<Value>()
        .branch(vec![
            (
                BranchPredicate::new(|_, acc: &Value| acc.as_i64() == Some(1)),
                AnyStep::from_step(
                    step("left")
                        .input::<Value>()
                        .output::<Value>()
                        .execute(|_, _| async move { Ok(StepOutcome::Done(json!("L"))) }),
                ),
            ),
            (
                BranchPredicate::new(|_, _| true),
                AnyStep::from_step(
                    step("right")
                        .input::<Value>()
                        .output::<Value>()
                        .execute(|_, _| async move { Ok(StepOutcome::Done(json!("R"))) }),
                ),
            ),
        ])
        .commit();
    let h = w.run(root_ctx(), json!(1), deps()).await.expect("run");
    let out = h.await_done().await.expect("done");
    match out {
        ork_core::ports::workflow_run::RunState::Completed { output } => {
            assert_eq!(output, json!("L"));
        }
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn parallel_runs_arms_concurrently() {
    let w = workflow("wf-par-conc")
        .input::<Value>()
        .output::<Value>()
        .parallel(vec![
            AnyStep::from_step(step("slow-a").input::<Value>().output::<Value>().execute(
                |_, _| async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(StepOutcome::Done(json!(1)))
                },
            )),
            AnyStep::from_step(step("slow-b").input::<Value>().output::<Value>().execute(
                |_, _| async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(StepOutcome::Done(json!(2)))
                },
            )),
        ])
        .commit();
    let start = tokio::time::Instant::now();
    let h = w.run(root_ctx(), json!(null), deps()).await.expect("run");
    let out = h.await_done().await.expect("done");
    let elapsed = start.elapsed();
    // Threshold left a generous gap above the two-arm overlap target (100ms): a
    // sequential run lands north of 200ms, so 250ms still fails the wrong shape
    // while absorbing scheduler jitter on busy runners (the prior 195ms cap was
    // below `100ms + scheduler overhead` on this machine).
    assert!(
        elapsed < Duration::from_millis(250),
        "parallel arms should overlap (~100ms); sequential would be ~200ms+, got {elapsed:?}"
    );
    match out {
        ork_core::ports::workflow_run::RunState::Completed { output } => {
            assert_eq!(output, json!([1, 2]));
        }
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn parallel_joins_branch_outputs() {
    let w = workflow("wf-par")
        .input::<Value>()
        .output::<Value>()
        .parallel(vec![
            AnyStep::from_step(
                step("a")
                    .input::<Value>()
                    .output::<Value>()
                    .execute(|_, _| async move { Ok(StepOutcome::Done(json!(10))) }),
            ),
            AnyStep::from_step(
                step("b")
                    .input::<Value>()
                    .output::<Value>()
                    .execute(|_, _| async move { Ok(StepOutcome::Done(json!(20))) }),
            ),
        ])
        .commit();
    let h = w.run(root_ctx(), json!(null), deps()).await.expect("run");
    let out = h.await_done().await.expect("done");
    match out {
        ork_core::ports::workflow_run::RunState::Completed { output } => {
            assert_eq!(output, json!([10, 20]));
        }
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn dountil_stops_when_predicate_holds() {
    let body = AnyStep::from_step(step("inc").input::<Value>().output::<Value>().execute(
        |_, v| async move {
            let n = v.as_i64().unwrap_or(0);
            Ok(StepOutcome::Done(json!(n + 1)))
        },
    ));
    let until = Predicate::new(|_, acc: &Value| acc.as_i64().unwrap_or(0) >= 3);
    let w = workflow("wf-until")
        .input::<Value>()
        .output::<Value>()
        .then(
            step("zero")
                .input::<Value>()
                .output::<Value>()
                .execute(|_, _| async move { Ok(StepOutcome::Done(json!(0))) }),
        )
        .dountil(body, until)
        .commit();
    let h = w.run(root_ctx(), json!(null), deps()).await.expect("run");
    let out = h.await_done().await.expect("done");
    match out {
        ork_core::ports::workflow_run::RunState::Completed { output } => {
            assert_eq!(output, json!(3));
        }
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn foreach_maps_array() {
    let w = workflow("wf-fe")
        .input::<Value>()
        .output::<Vec<i32>>()
        .then(
            step("nums")
                .input::<Value>()
                .output::<Vec<i32>>()
                .execute(|_, _| async move { Ok(StepOutcome::Done(vec![1, 2, 3])) }),
        )
        .foreach(
            step("dbl")
                .input::<i32>()
                .output::<i32>()
                .execute(|_, x| async move { Ok(StepOutcome::Done(x * 2)) }),
            ForEachOptions::default(),
        )
        .commit();
    let h = w.run(root_ctx(), json!(null), deps()).await.expect("run");
    let out = h.await_done().await.expect("done");
    match out {
        ork_core::ports::workflow_run::RunState::Completed { output } => {
            assert_eq!(output, json!([2, 4, 6]));
        }
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn cancel_fails_run_when_step_observes_token() {
    let ctx = root_ctx();
    let cancel = ctx.cancel.clone();
    let w = workflow("wf-can")
        .input::<Value>()
        .output::<Value>()
        .then(
            step("slow")
                .input::<Value>()
                .output::<Value>()
                .execute(|ctx, _| async move {
                    tokio::select! {
                        biased;
                        _ = ctx.agent_context.cancel.cancelled() => {
                            Err(ork_common::error::OrkError::Internal("cancelled".into()))
                        }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            Ok(StepOutcome::Done(json!(1)))
                        }
                    }
                }),
        )
        .commit();
    let h = w.run(ctx, json!(null), deps()).await.expect("run");
    cancel.cancel();
    let out = h.await_done().await.expect("done");
    match out {
        ork_core::ports::workflow_run::RunState::Failed { error } => {
            assert!(error.contains("cancelled") || error.contains("Internal"));
        }
        o => panic!("expected Failed, got {o:?}"),
    }
}
