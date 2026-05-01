//! Code-first typed workflow DSL (ADR [`0050`](../../docs/adrs/0050-code-first-workflow-dsl.md)).
//!
//! ## Typestate mismatch (compile-time)
//!
//! The second `.then` must consume the previous step's output type.
//!
//! ```compile_fail
//! use ork_workflow::{step, workflow};
//! # use serde::{Deserialize, Serialize};
//! # use schemars::JsonSchema;
//! #
//! # #[derive(Deserialize, Serialize, JsonSchema)]
//! # struct U;
//! let a = step("a")
//!     .input::<u32>()
//!     .output::<String>()
//!     .execute(|_, x| async move { Ok(ork_workflow::StepOutcome::Done(format!("{x}"))) });
//! let b = step("b")
//!     .input::<u32>()
//!     .output::<()>()
//!     .execute(|_, _| async move { Ok(ork_workflow::StepOutcome::Done(())) });
//! let _ = workflow("w")
//!     .input::<u32>()
//!     .output::<()>()
//!     .then(a)
//!     .then(b)
//!     .commit();
//! ```

pub mod builder;
mod engine;
mod erased;
pub mod program;
pub use program::ProgramOp;
pub mod trigger;
mod ty_eq;
pub mod types;
mod yaml_compat;

pub use builder::{AnyStep, Step, StepBuilder, WorkflowBuilder, step, workflow};
pub use engine::spawn_resumed_workflow_run;
pub use erased::StepOutcome;
pub use trigger::{SchedulerService, Trigger};

use std::sync::Arc;

use futures::future::BoxFuture;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::workflow_def::WorkflowDef;
use ork_core::ports::workflow_run::{WorkflowRunDeps, WorkflowRunHandle};
use serde_json::Value;

use crate::engine::spawn_workflow_run;
use crate::trigger::TriggerSpec;

/// Opaque committed workflow registered on `OrkApp`.
pub struct Workflow {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) tool_refs: Vec<String>,
    pub(crate) agent_refs: Vec<String>,
    pub(crate) program: Arc<Vec<ProgramOp>>,
    #[allow(dead_code)]
    pub(crate) input_schema: Value,
    #[allow(dead_code)]
    pub(crate) output_schema: Value,
    pub(crate) trigger: Option<TriggerSpec>,
}

impl Workflow {
    /// Load a legacy YAML template from disk and desugar into a [`Workflow`] (ADR-0050).
    pub fn from_template_path(path: impl AsRef<std::path::Path>) -> Result<Self, OrkError> {
        yaml_compat::from_template_path(path)
    }

    /// Trigger configuration for cron/webhook registration.
    #[must_use]
    pub fn trigger_spec(&self) -> Option<&TriggerSpec> {
        self.trigger.as_ref()
    }

    /// Program ops for resume-after-restart (internal to tests / app wiring).
    #[must_use]
    pub fn program_arc(&self) -> Arc<Vec<ProgramOp>> {
        Arc::clone(&self.program)
    }
}

impl WorkflowDef for Workflow {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn description(&self) -> &str {
        self.description.as_str()
    }

    fn referenced_tool_ids(&self) -> &[String] {
        &self.tool_refs
    }

    fn referenced_agent_ids(&self) -> &[String] {
        &self.agent_refs
    }

    fn run<'a>(
        &'a self,
        ctx: AgentContext,
        input: Value,
        deps: WorkflowRunDeps,
    ) -> BoxFuture<'a, Result<WorkflowRunHandle, OrkError>> {
        let wid = self.id.clone();
        let prog = Arc::clone(&self.program);
        Box::pin(async move { spawn_workflow_run(wid, prog, ctx, input, deps).await })
    }

    fn cron_trigger(&self) -> Option<(String, String)> {
        match &self.trigger {
            Some(TriggerSpec::Cron { expr, tz }) => Some((expr.clone(), tz.clone())),
            _ => None,
        }
    }
}
