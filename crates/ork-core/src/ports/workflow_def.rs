//! Workflow registration and execution anchor for `OrkApp` (ADRs [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md), [`0050`](../../../docs/adrs/0050-code-first-workflow-dsl.md)).

use futures::future::BoxFuture;
use ork_common::error::OrkError;
use serde_json::Value;

use crate::a2a::AgentContext;
use crate::ports::workflow_run::{WorkflowRunDeps, WorkflowRunHandle};

/// Code-first workflow registration and execution anchor for `OrkApp` (crate `ork-app`).
pub trait WorkflowDef: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;

    /// Tool ids this workflow may call; used by `OrkAppBuilder::build()` to reject unresolved refs.
    fn referenced_tool_ids(&self) -> &[String];

    /// Agent ids this workflow may delegate to; used by `OrkAppBuilder::build()` to reject unresolved refs.
    fn referenced_agent_ids(&self) -> &[String];

    fn run<'a>(
        &'a self,
        ctx: AgentContext,
        input: Value,
        deps: WorkflowRunDeps,
    ) -> BoxFuture<'a, Result<WorkflowRunHandle, OrkError>>;

    /// ADR-0050: optional code-first cron (`expr`, `tz` e.g. `"UTC"`).
    fn cron_trigger(&self) -> Option<(String, String)> {
        None
    }
}
