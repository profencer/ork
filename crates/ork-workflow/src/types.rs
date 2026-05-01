//! Runtime context for a workflow step.

use std::sync::Arc;
use std::time::Duration;

use ork_common::error::OrkError;
use ork_common::types::WorkflowRunId;
use ork_core::a2a::{AgentContext, Part};
use ork_core::agent_registry::AgentRegistry;
use ork_core::workflow::engine::ToolExecutor;
use serde_json::Value;

/// Retry policy for a step or workflow.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: ExponentialBackoff,
}

#[derive(Clone, Debug)]
pub struct ExponentialBackoff {
    pub initial: Duration,
    pub multiplier: f64,
    pub jitter: Duration,
    pub max: Duration,
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            multiplier: 2.0,
            jitter: Duration::from_millis(50),
            max: Duration::from_secs(30),
        }
    }
}

/// Per-run identifiers + optional resume payload (after HITL / external event).
#[derive(Clone, Debug)]
pub struct RunInfo {
    pub run_id: WorkflowRunId,
    pub attempt: u32,
    pub parent_run_id: Option<WorkflowRunId>,
    /// Validated against the step's `resume_schema` when resuming a suspended run.
    pub resume_data: Option<Value>,
}

/// Calls tools during a step (MCP / native — wired from run deps).
#[derive(Clone)]
pub struct ToolHandle {
    exec: Option<Arc<dyn ToolExecutor>>,
}

impl ToolHandle {
    #[must_use]
    pub fn new(exec: Option<Arc<dyn ToolExecutor>>) -> Self {
        Self { exec }
    }

    pub async fn call(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: Value,
    ) -> Result<Value, OrkError> {
        let Some(ex) = self.exec.as_ref() else {
            return Err(OrkError::Unsupported(format!(
                "tool `{tool_name}`: no ToolExecutor wired for this run"
            )));
        };
        ex.execute(ctx, tool_name, &input).await
    }
}

/// Delegates to other registered agents by id (ADR-0006 piggyback).
#[derive(Clone)]
pub struct AgentHandle {
    pub(crate) registry: Option<Arc<AgentRegistry>>,
}

impl AgentHandle {
    #[must_use]
    pub fn new(registry: Option<Arc<AgentRegistry>>) -> Self {
        Self { registry }
    }

    #[must_use]
    pub fn registry_arc(&self) -> Option<Arc<AgentRegistry>> {
        self.registry.clone()
    }

    pub async fn run(
        &self,
        ctx: AgentContext,
        agent_id: &str,
        prompt: impl Into<String>,
    ) -> Result<String, OrkError> {
        let Some(reg) = self.registry.as_ref() else {
            return Err(OrkError::Configuration {
                message: format!("agent `{agent_id}`: no AgentRegistry wired for this run"),
            });
        };
        let agent = reg
            .resolve(&agent_id.to_string())
            .await
            .ok_or_else(|| OrkError::NotFound(format!("agent `{agent_id}`")))?;
        let mut msg = ork_core::a2a::AgentMessage::user_text(prompt.into());
        msg.task_id = Some(ctx.task_id);
        let out = agent.send(ctx, msg).await?;
        let text = out
            .parts
            .iter()
            .filter_map(|p| {
                if let Part::Text { text, .. } = p {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(text)
    }
}

/// Placeholder for ADR-0053 memory — returns Unsupported for now.
#[derive(Clone, Default)]
pub struct MemoryHandle;

impl MemoryHandle {
    pub async fn recall(&self, _key: &str) -> Result<Value, OrkError> {
        Err(OrkError::Unsupported(
            "memory handle not wired (ADR 0053)".into(),
        ))
    }
}

/// Predicate over [`StepContext`] + current accumulator JSON (branch / loops).
pub(crate) type StepPredicateFn = dyn Fn(&StepContext, &Value) -> bool + Send + Sync;

/// Predicate for [`super::WorkflowBuilder::branch`].
#[derive(Clone)]
pub struct BranchPredicate {
    pub(crate) inner: Arc<StepPredicateFn>,
}

impl BranchPredicate {
    pub fn new(f: impl Fn(&StepContext, &Value) -> bool + Send + Sync + 'static) -> Self {
        Self { inner: Arc::new(f) }
    }
}

/// Predicate for do-while / do-until loops.
#[derive(Clone)]
pub struct Predicate {
    pub(crate) inner: Arc<StepPredicateFn>,
}

impl Predicate {
    pub fn new(f: impl Fn(&StepContext, &Value) -> bool + Send + Sync + 'static) -> Self {
        Self { inner: Arc::new(f) }
    }
}

/// Options for foreach (concurrency).
#[derive(Clone, Debug, Default)]
pub struct ForEachOptions {
    pub concurrency: usize,
}

impl ForEachOptions {
    #[must_use]
    pub fn with_concurrency(n: usize) -> Self {
        Self {
            concurrency: n.max(1),
        }
    }
}

/// Tenant/cancel/tools/agents — passed into every step closure.
#[derive(Clone)]
pub struct StepContext {
    pub agent_context: AgentContext,
    pub tools: ToolHandle,
    pub agents: AgentHandle,
    pub memory: MemoryHandle,
    pub run: RunInfo,
}
