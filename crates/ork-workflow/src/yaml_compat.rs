//! YAML template desugaring into a [`crate::Workflow`].

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::models::workflow::WorkflowRunStatus;
use ork_core::models::workflow::{WorkflowDefinition, WorkflowRun, WorkflowTrigger};
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler::compile;
use ork_core::workflow::engine::WorkflowEngine;
use serde::Deserialize;
use serde_json::Value;

use crate::Workflow;
use crate::erased::{ErasedStep, StepOutcome};
use crate::program::ProgramOp;

#[derive(Deserialize)]
struct WorkflowYaml {
    name: String,
    version: String,
    trigger: WorkflowTrigger,
    steps: Vec<ork_core::models::workflow::WorkflowStep>,
}

struct YamlGraphStep {
    id: String,
    graph: ork_core::workflow::compiler::CompiledWorkflow,
}

#[async_trait::async_trait]
impl ErasedStep for YamlGraphStep {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({})
    }

    fn output_schema(&self) -> Value {
        serde_json::json!({})
    }

    fn tool_refs(&self) -> &[String] {
        &[]
    }

    fn agent_refs(&self) -> &[String] {
        &[]
    }

    fn max_attempts(&self) -> u32 {
        1
    }

    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }

    async fn run(
        &self,
        ctx: crate::types::StepContext,
        input: Value,
    ) -> Result<StepOutcome<Value>, OrkError> {
        let agents = ctx
            .agents
            .registry_arc()
            .ok_or_else(|| OrkError::Configuration {
                message: "YAML workflow execution requires AgentRegistry on WorkflowRunDeps".into(),
            })?;
        let tenant_id = ctx.agent_context.tenant_id;
        let repo = Arc::new(NoopWorkflowRepository);
        let engine = WorkflowEngine::new(repo, agents);
        let mut run = WorkflowRun {
            id: WorkflowRunId::new(),
            workflow_id: WorkflowId::new(),
            tenant_id,
            status: WorkflowRunStatus::Pending,
            input,
            output: None,
            step_results: vec![],
            started_at: Utc::now(),
            completed_at: None,
            parent_run_id: None,
            parent_step_id: None,
            parent_task_id: None,
        };
        engine.execute(tenant_id, &mut run, &self.graph).await?;
        Ok(StepOutcome::Done(run.output.unwrap_or(Value::Null)))
    }
}

/// Build a [`Workflow`] from a YAML file on disk (demo templates).
pub fn from_template_path(path: impl AsRef<Path>) -> Result<Workflow, OrkError> {
    let path = path.as_ref();
    let yaml = std::fs::read_to_string(path)
        .map_err(|e| OrkError::Internal(format!("read {}: {e}", path.display())))?;
    let wf: WorkflowYaml =
        serde_yaml::from_str(&yaml).map_err(|e| OrkError::Workflow(format!("yaml: {e}")))?;
    let now = Utc::now();
    let def = WorkflowDefinition {
        id: WorkflowId::new(),
        tenant_id: TenantId::new(),
        name: wf.name.clone(),
        version: wf.version,
        trigger: wf.trigger,
        steps: wf.steps.clone(),
        created_at: now,
        updated_at: now,
    };
    let graph = compile(&def)?;
    let mut tool_refs: Vec<String> = Vec::new();
    let mut agent_refs: Vec<String> = Vec::new();
    for s in &wf.steps {
        tool_refs.extend(s.tools.iter().cloned());
        agent_refs.push(s.agent.display_id());
    }
    tool_refs.sort();
    tool_refs.dedup();
    agent_refs.sort();
    agent_refs.dedup();
    let inner: Arc<dyn ErasedStep> = Arc::new(YamlGraphStep {
        id: "yaml_graph".into(),
        graph,
    });
    Ok(Workflow {
        id: def.name.clone(),
        description: format!("imported from {}", path.display()),
        tool_refs,
        agent_refs,
        program: Arc::new(vec![ProgramOp::Step(inner)]),
        input_schema: serde_json::json!({}),
        output_schema: serde_json::json!({}),
        trigger: None,
    })
}
