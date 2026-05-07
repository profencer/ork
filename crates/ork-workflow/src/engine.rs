//! Interpreter + [`WorkflowRunHandle`](ork_core::ports::workflow_run::WorkflowRunHandle) wiring.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jsonschema::Validator;
use ork_common::error::OrkError;
use ork_common::types::WorkflowRunId;
use ork_core::a2a::AgentContext;
use ork_core::ports::workflow_run::{
    RunState, WorkflowEvent, WorkflowRunDeps, WorkflowRunDriver, WorkflowRunHandle,
};
use ork_core::ports::workflow_snapshot::{RunStateBlob, SnapshotKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use crate::erased::{ErasedStep, StepOutcome};
use crate::program::ProgramOp;
use crate::types::{AgentHandle, RunInfo, StepContext, ToolHandle};

#[derive(Debug, Serialize, Deserialize)]
struct LinearCheckpoint {
    pc: usize,
    acc: Value,
}

pub(crate) async fn spawn_workflow_run(
    workflow_id: String,
    program: Arc<Vec<ProgramOp>>,
    ctx: AgentContext,
    input: Value,
    deps: WorkflowRunDeps,
) -> Result<WorkflowRunHandle, OrkError> {
    spawn_with_checkpoint(workflow_id, program, ctx, input, deps, None, None, None).await
}

/// Resume a run after process restart using a snapshot row (ADR-0050 round-trip test).
pub async fn spawn_resumed_workflow_run(
    workflow_id: String,
    program: Arc<Vec<ProgramOp>>,
    ctx: AgentContext,
    deps: WorkflowRunDeps,
    row: ork_core::ports::workflow_snapshot::SnapshotRow,
) -> Result<WorkflowRunHandle, OrkError> {
    let cp: LinearCheckpoint = serde_json::from_value(row.run_state.0.clone())
        .map_err(|e| OrkError::Internal(format!("checkpoint: {e}")))?;
    let resume = row.payload.get("resume").cloned().unwrap_or(Value::Null);
    spawn_with_checkpoint(
        workflow_id,
        program,
        ctx,
        Value::Null,
        deps,
        Some(WorkflowRunId(row.key.run_id)),
        Some((cp, resume)),
        Some(row.resume_schema),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn spawn_with_checkpoint(
    workflow_id: String,
    program: Arc<Vec<ProgramOp>>,
    ctx: AgentContext,
    input: Value,
    deps: WorkflowRunDeps,
    run_id_override: Option<WorkflowRunId>,
    resume: Option<(LinearCheckpoint, Value)>,
    resume_schema: Option<Value>,
) -> Result<WorkflowRunHandle, OrkError> {
    let run_id = run_id_override.unwrap_or_default();
    let (ev_tx, _) = broadcast::channel::<WorkflowEvent>(128);
    let state: Arc<RwLock<RunState>> = Arc::new(RwLock::new(RunState::Running));
    let cancel = ctx.cancel.clone();
    let (resume_snd, resume_rcv) = mpsc::channel::<Value>(4);

    let tool = ToolHandle::new(deps.tool_executor.clone());
    let agents = AgentHandle::new(deps.agents.clone());
    let step_ctx = StepContext {
        agent_context: ctx.clone(),
        tools: tool,
        agents,
        memory: crate::types::MemoryHandle,
        run: RunInfo {
            run_id,
            attempt: 0,
            parent_run_id: None,
            resume_data: None,
        },
    };

    let driver = Arc::new(ProgramDriver {
        run_id,
        state: Arc::clone(&state),
        events: ev_tx.clone(),
        resume_ch: Mutex::new(Some(resume_snd)),
        cancel: cancel.clone(),
        done: tokio::sync::Notify::new(),
    });

    let wf_id = workflow_id.clone();
    let snap = deps.snapshot_store.clone();
    let prog = Arc::clone(&program);
    let d2 = Arc::clone(&driver);

    tokio::spawn(async move {
        let r = run_program_inner(
            wf_id,
            run_id,
            prog,
            step_ctx,
            input,
            snap,
            ctx,
            ev_tx,
            Arc::clone(&state),
            resume_rcv,
            cancel,
            resume,
            resume_schema,
        )
        .await;
        if let Err(e) = r {
            *state.write().await = RunState::Failed {
                error: e.to_string(),
            };
        }
        d2.done.notify_waiters();
    });

    Ok(WorkflowRunHandle::new(driver))
}

#[allow(clippy::too_many_arguments)]
async fn run_program_inner(
    workflow_id: String,
    run_id: WorkflowRunId,
    program: Arc<Vec<ProgramOp>>,
    mut step_ctx: StepContext,
    input: Value,
    snapshot_store: Option<Arc<dyn ork_core::ports::workflow_snapshot::WorkflowSnapshotStore>>,
    agent_ctx: AgentContext,
    ev_tx: broadcast::Sender<WorkflowEvent>,
    run_state: Arc<RwLock<RunState>>,
    mut resume_rx: mpsc::Receiver<Value>,
    cancel: tokio_util::sync::CancellationToken,
    mut resume: Option<(LinearCheckpoint, Value)>,
    resume_schema: Option<Value>,
) -> Result<(), OrkError> {
    let (mut pc, mut acc) = if let Some((cp, data)) = resume.take() {
        if let Some(ref schema) = resume_schema {
            validate_resume(schema, &data)?;
        }
        step_ctx.run.resume_data = Some(data);
        (cp.pc, cp.acc)
    } else {
        (0usize, input)
    };

    while pc < program.len() {
        if cancel.is_cancelled() {
            *run_state.write().await = RunState::Failed {
                error: "cancelled".into(),
            };
            return Ok(());
        }
        let op = &program[pc];
        match op {
            ProgramOp::Step(step) => {
                step_ctx.agent_context = agent_ctx.clone();
                let mut finished = false;
                while !finished {
                    if cancel.is_cancelled() {
                        *run_state.write().await = RunState::Failed {
                            error: "cancelled".into(),
                        };
                        return Ok(());
                    }
                    let _ = ev_tx.send(WorkflowEvent::StepStarted {
                        step_id: step.id().to_string(),
                        input: acc.clone(),
                    });
                    let out = execute_step_with_retry(
                        step.as_ref(),
                        step_ctx.clone(),
                        acc.clone(),
                        &ev_tx,
                        &cancel,
                    )
                    .await?;
                    match out {
                        StepOutcome::Done(v) => {
                            let _ = ev_tx.send(WorkflowEvent::StepFinished {
                                step_id: step.id().to_string(),
                                output: v.clone(),
                            });
                            acc = v;
                            step_ctx.run.resume_data = None;
                            pc += 1;
                            finished = true;
                        }
                        StepOutcome::Suspend {
                            ref payload,
                            resume_schema: rs,
                        } => {
                            let store =
                                snapshot_store
                                    .as_ref()
                                    .ok_or_else(|| OrkError::Configuration {
                                        message: "suspend requires WorkflowRunDeps.snapshot_store"
                                            .into(),
                                    })?;
                            let key = SnapshotKey {
                                workflow_id: workflow_id.clone(),
                                run_id: run_id.0,
                                step_id: step.id().to_string(),
                                attempt: 1,
                            };
                            let cp = LinearCheckpoint {
                                pc,
                                acc: acc.clone(),
                            };
                            store
                                .save(
                                    key.clone(),
                                    json!({ "suspend": payload, "resume": null }),
                                    rs.clone(),
                                    RunStateBlob(serde_json::to_value(&cp).unwrap_or(Value::Null)),
                                )
                                .await?;
                            let _ = ev_tx.send(WorkflowEvent::StepSuspended {
                                step_id: step.id().to_string(),
                                payload: payload.clone(),
                            });
                            *run_state.write().await = RunState::Suspended {
                                step_id: step.id().to_string(),
                                payload: payload.clone(),
                                resume_schema: rs.clone(),
                            };
                            let data = tokio::select! {
                                _ = cancel.cancelled() => {
                                    *run_state.write().await = RunState::Failed {
                                        error: "cancelled".into(),
                                    };
                                    return Ok(());
                                }
                                v = resume_rx.recv() => {
                                    v.ok_or_else(|| OrkError::Internal("resume channel closed".into()))?
                                }
                            };
                            validate_resume(&rs, &data)?;
                            store.mark_consumed(key).await?;
                            step_ctx.run.resume_data = Some(data);
                            *run_state.write().await = RunState::Running;
                        }
                    }
                }
            }
            ProgramOp::Map(f) => {
                acc = f(acc)?;
                pc += 1;
            }
            ProgramOp::Branch(arms) => {
                let mut chosen: Option<&Vec<ProgramOp>> = None;
                let ctx2 = step_ctx.clone();
                for (pred, block) in arms {
                    if (pred.inner)(&ctx2, &acc) {
                        chosen = Some(block);
                        break;
                    }
                }
                let block =
                    chosen.ok_or_else(|| OrkError::Workflow("branch: no arm matched".into()))?;
                acc = interpret_block(block, &step_ctx, &agent_ctx, &ev_tx, &cancel, acc).await?;
                pc += 1;
            }
            ProgramOp::Parallel(arms) => {
                let mut outs = Vec::with_capacity(arms.len());
                for block in arms {
                    let c2 = step_ctx.clone();
                    let v = acc.clone();
                    let o = interpret_block(block, &c2, &agent_ctx, &ev_tx, &cancel, v).await?;
                    outs.push(o);
                }
                acc = Value::Array(outs);
                pc += 1;
            }
            ProgramOp::DoUntil { body, until } => {
                loop {
                    if cancel.is_cancelled() {
                        *run_state.write().await = RunState::Failed {
                            error: "cancelled".into(),
                        };
                        return Ok(());
                    }
                    let c2 = step_ctx.clone();
                    acc = interpret_block(body, &c2, &agent_ctx, &ev_tx, &cancel, acc).await?;
                    if (until.inner)(&step_ctx, &acc) {
                        break;
                    }
                }
                pc += 1;
            }
            ProgramOp::DoWhile { body, while_ } => {
                loop {
                    if cancel.is_cancelled() {
                        *run_state.write().await = RunState::Failed {
                            error: "cancelled".into(),
                        };
                        return Ok(());
                    }
                    let c2 = step_ctx.clone();
                    acc = interpret_block(body, &c2, &agent_ctx, &ev_tx, &cancel, acc).await?;
                    if !(while_.inner)(&step_ctx, &acc) {
                        break;
                    }
                }
                pc += 1;
            }
            ProgramOp::ForEach { step, opts } => {
                let arr = acc
                    .as_array()
                    .ok_or_else(|| OrkError::Validation("foreach: expected JSON array".into()))?
                    .clone();
                let mut results = Vec::with_capacity(arr.len());
                let conc = opts.concurrency.max(1);
                if conc == 1 {
                    for item in arr {
                        if cancel.is_cancelled() {
                            *run_state.write().await = RunState::Failed {
                                error: "cancelled".into(),
                            };
                            return Ok(());
                        }
                        let mut c2 = step_ctx.clone();
                        c2.agent_context = agent_ctx.clone();
                        let _ = ev_tx.send(WorkflowEvent::StepStarted {
                            step_id: step.id().to_string(),
                            input: item.clone(),
                        });
                        let o = execute_step_with_retry(step.as_ref(), c2, item, &ev_tx, &cancel)
                            .await?;
                        match o {
                            StepOutcome::Done(v) => {
                                let _ = ev_tx.send(WorkflowEvent::StepFinished {
                                    step_id: step.id().to_string(),
                                    output: v.clone(),
                                });
                                results.push(v);
                            }
                            StepOutcome::Suspend { .. } => {
                                return Err(OrkError::Unsupported(
                                    "foreach + suspend not supported in v1".into(),
                                ));
                            }
                        }
                    }
                } else {
                    use futures::stream::{FuturesUnordered, StreamExt};
                    let mut stream = FuturesUnordered::new();
                    for item in arr {
                        let st = Arc::clone(step);
                        let ev = ev_tx.clone();
                        let cx = agent_ctx.clone();
                        let sx = step_ctx.clone();
                        let cc = cancel.clone();
                        stream.push(async move {
                            let mut c2 = sx.clone();
                            c2.agent_context = cx;
                            execute_step_with_retry(st.as_ref(), c2, item, &ev, &cc).await
                        });
                    }
                    while let Some(r) = stream.next().await {
                        match r? {
                            StepOutcome::Done(v) => results.push(v),
                            StepOutcome::Suspend { .. } => {
                                return Err(OrkError::Unsupported(
                                    "foreach + suspend not supported in v1".into(),
                                ));
                            }
                        }
                    }
                }
                acc = Value::Array(results);
                pc += 1;
            }
        }
    }
    *run_state.write().await = RunState::Completed { output: acc };
    Ok(())
}

async fn interpret_block(
    block: &[ProgramOp],
    step_ctx: &StepContext,
    agent_ctx: &AgentContext,
    ev_tx: &broadcast::Sender<WorkflowEvent>,
    cancel: &tokio_util::sync::CancellationToken,
    mut acc: Value,
) -> Result<Value, OrkError> {
    for op in block {
        if cancel.is_cancelled() {
            return Err(OrkError::Internal("cancelled".into()));
        }
        match op {
            ProgramOp::Step(step) => {
                let mut c2 = step_ctx.clone();
                c2.agent_context = agent_ctx.clone();
                let _ = ev_tx.send(WorkflowEvent::StepStarted {
                    step_id: step.id().to_string(),
                    input: acc.clone(),
                });
                let o = execute_step_with_retry(step.as_ref(), c2, acc, ev_tx, cancel).await?;
                match o {
                    StepOutcome::Done(v) => {
                        let _ = ev_tx.send(WorkflowEvent::StepFinished {
                            step_id: step.id().to_string(),
                            output: v.clone(),
                        });
                        acc = v;
                    }
                    StepOutcome::Suspend { .. } => {
                        return Err(OrkError::Unsupported(
                            "suspend inside nested control-flow block in v1".into(),
                        ));
                    }
                }
            }
            ProgramOp::Map(f) => acc = f(acc)?,
            ProgramOp::Branch(_)
            | ProgramOp::Parallel(_)
            | ProgramOp::DoUntil { .. }
            | ProgramOp::DoWhile { .. }
            | ProgramOp::ForEach { .. } => {
                return Err(OrkError::Unsupported(
                    "nested composite control-flow inside arm not supported in v1".into(),
                ));
            }
        }
    }
    Ok(acc)
}

async fn execute_step_with_retry(
    step: &dyn ErasedStep,
    ctx: StepContext,
    input: Value,
    ev_tx: &broadcast::Sender<WorkflowEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<StepOutcome<Value>, OrkError> {
    let max = step.max_attempts().max(1);
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(OrkError::Internal("cancelled".into()));
        }
        attempt += 1;
        let fut = step.run(ctx.clone(), input.clone());
        let out = match step.timeout() {
            Some(d) => tokio::time::timeout(d, fut)
                .await
                .map_err(|_| OrkError::Workflow(format!("step `{}` timed out", step.id())))?,
            None => fut.await,
        };
        match out {
            Ok(o) => return Ok(o),
            Err(e) if attempt < max => {
                let _ = ev_tx.send(WorkflowEvent::StepRetrying {
                    step_id: step.id().to_string(),
                    attempt,
                    after_ms: 50,
                });
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                let _ = ev_tx.send(WorkflowEvent::StepFailed {
                    step_id: step.id().to_string(),
                    error: e.to_string(),
                    retryable: false,
                });
                return Err(OrkError::Workflow(e.to_string()));
            }
        }
    }
}

fn validate_resume(schema: &Value, data: &Value) -> Result<(), OrkError> {
    if schema.is_null() {
        return Ok(());
    }
    let compiled = Validator::new(schema)
        .map_err(|e| OrkError::Internal(format!("resume_schema compile: {e}")))?;
    if !compiled.is_valid(data) {
        return Err(OrkError::Validation(
            "resume payload failed JSON schema validation".into(),
        ));
    }
    Ok(())
}

struct ProgramDriver {
    run_id: WorkflowRunId,
    state: Arc<RwLock<RunState>>,
    events: broadcast::Sender<WorkflowEvent>,
    resume_ch: Mutex<Option<mpsc::Sender<Value>>>,
    cancel: tokio_util::sync::CancellationToken,
    done: tokio::sync::Notify,
}

#[async_trait]
impl WorkflowRunDriver for ProgramDriver {
    fn run_id(&self) -> WorkflowRunId {
        self.run_id
    }

    async fn poll(&self) -> Result<RunState, OrkError> {
        Ok(self.state.read().await.clone())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.events.subscribe()
    }

    async fn resume(&self, _step_id: &str, data: Value) -> Result<(), OrkError> {
        let g = self.resume_ch.lock().await;
        let Some(ref tx) = *g else {
            return Err(OrkError::Conflict("resume channel not available".into()));
        };
        tx.send(data)
            .await
            .map_err(|_| OrkError::Internal("failed to deliver resume to run task".into()))
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    async fn await_done(&self) -> Result<RunState, OrkError> {
        self.done.notified().await;
        self.poll().await
    }
}
