use std::collections::HashMap;

use ork_common::error::OrkError;

use crate::models::workflow::{DelegationSpec, WorkflowAgentRef, WorkflowDefinition, WorkflowStep};

/// Represents a compiled workflow graph ready for execution.
#[derive(Debug, Clone)]
pub struct CompiledWorkflow {
    pub name: String,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
    pub entry_node: String,
}

#[derive(Debug, Clone)]
pub struct WorkflowNode {
    pub id: String,
    pub agent: WorkflowAgentRef,
    pub tools: Vec<String>,
    pub prompt_template: String,
    pub for_each: Option<String>,
    pub iteration_var: Option<String>,
    /// Optional peer delegation hop after the parent step completes (ADR 0006).
    pub delegate_to: Option<DelegationSpec>,
    /// ADR 0012 §`Selection`: optional per-step LLM provider override
    /// (highest precedence). Lifted unchanged from
    /// [`WorkflowStep::provider`]; the engine threads it onto the
    /// per-step [`crate::a2a::AgentContext::step_llm_overrides`].
    pub step_provider: Option<String>,
    /// ADR 0012 §`Selection`: optional per-step LLM model override.
    /// Lifted unchanged from [`WorkflowStep::model`].
    pub step_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowEdge {
    pub from: String,
    pub to: String,
    pub condition: Option<EdgeCondition>,
}

#[derive(Debug, Clone)]
pub enum EdgeCondition {
    OnPass,
    OnFail,
    Always,
}

/// Compiles a YAML workflow definition into a directed graph of agent tasks.
pub fn compile(definition: &WorkflowDefinition) -> Result<CompiledWorkflow, OrkError> {
    if definition.steps.is_empty() {
        return Err(OrkError::Workflow("workflow has no steps".into()));
    }

    let step_ids: HashMap<&str, &WorkflowStep> = definition
        .steps
        .iter()
        .map(|s| (s.id.as_str(), s))
        .collect();

    for step in &definition.steps {
        for dep in &step.depends_on {
            if !step_ids.contains_key(dep.as_str()) {
                return Err(OrkError::Workflow(format!(
                    "step '{}' depends on unknown step '{}'",
                    step.id, dep
                )));
            }
        }
        if let Some(spec) = &step.delegate_to {
            validate_delegation_spec(&step.id, spec)?;
        }
    }

    let nodes: Vec<WorkflowNode> = definition
        .steps
        .iter()
        .map(|s| WorkflowNode {
            id: s.id.clone(),
            agent: s.agent.clone(),
            tools: s.tools.clone(),
            prompt_template: s.prompt_template.clone(),
            for_each: s.for_each.clone(),
            iteration_var: s.iteration_var.clone(),
            delegate_to: s.delegate_to.clone(),
            step_provider: s.provider.clone(),
            step_model: s.model.clone(),
        })
        .collect();

    let mut edges = Vec::new();

    for step in &definition.steps {
        for dep in &step.depends_on {
            // `depends_on` semantically means "I need this step's output";
            // a failed parent cannot produce that output, so we emit the
            // edge as `OnPass`. Authors who want a fan-out-on-failure path
            // declare it explicitly via the parent step's `condition.on_fail`,
            // which still becomes an `OnFail` edge below. Without this, a
            // single transient failure on an upstream step would have the
            // engine fan forward through every downstream step with
            // unsubstituted prompt templates — see
            // `docs/incidents/2026-04-25-workflow-cascades-past-failed-step.md`
            // and the regression in
            // `crates/ork-core/tests/engine_failed_step_does_not_cascade.rs`.
            edges.push(WorkflowEdge {
                from: dep.clone(),
                to: step.id.clone(),
                condition: Some(EdgeCondition::OnPass),
            });
        }

        if let Some(cond) = &step.condition {
            if cond.on_pass != "complete" {
                edges.push(WorkflowEdge {
                    from: step.id.clone(),
                    to: cond.on_pass.clone(),
                    condition: Some(EdgeCondition::OnPass),
                });
            }
            if cond.on_fail != "complete" {
                edges.push(WorkflowEdge {
                    from: step.id.clone(),
                    to: cond.on_fail.clone(),
                    condition: Some(EdgeCondition::OnFail),
                });
            }
        }
    }

    let entry_node = definition
        .steps
        .iter()
        .find(|s| s.depends_on.is_empty())
        .ok_or_else(|| {
            OrkError::Workflow("no entry step found (all steps have dependencies)".into())
        })?
        .id
        .clone();

    Ok(CompiledWorkflow {
        name: definition.name.clone(),
        nodes,
        edges,
        entry_node,
    })
}

/// Validate the cross-field invariants of [`DelegationSpec`] from ADR 0006.
///
/// - `agent` must be non-empty.
/// - Either `prompt_template` or `child_workflow` must be set (something to do).
/// - `await: false` together with `child_workflow` is rejected for v1: the engine
///   would have to start a sub-engine and immediately abandon it, which is undefined
///   in the ADR. Workflow authors who want fire-and-forget should compose two
///   separate steps.
/// - `push_url` is only meaningful when `await: false`.
fn validate_delegation_spec(step_id: &str, spec: &DelegationSpec) -> Result<(), OrkError> {
    if spec.agent.trim().is_empty() {
        return Err(OrkError::Workflow(format!(
            "step '{step_id}': delegate_to.agent must be non-empty"
        )));
    }
    if spec.prompt_template.trim().is_empty() && spec.child_workflow.is_none() {
        return Err(OrkError::Workflow(format!(
            "step '{step_id}': delegate_to needs either prompt_template or child_workflow"
        )));
    }
    if !spec.await_ && spec.child_workflow.is_some() {
        return Err(OrkError::Workflow(format!(
            "step '{step_id}': delegate_to.child_workflow requires await: true (ADR 0006)"
        )));
    }
    if spec.push_url.is_some() && spec.await_ {
        return Err(OrkError::Workflow(format!(
            "step '{step_id}': delegate_to.push_url only applies when await: false"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::workflow::WorkflowTrigger;
    use chrono::Utc;
    use ork_common::types::{TenantId, WorkflowId};
    use std::time::Duration;

    fn make_def(steps: Vec<WorkflowStep>) -> WorkflowDefinition {
        WorkflowDefinition {
            id: WorkflowId::new(),
            tenant_id: TenantId::new(),
            name: "wf".into(),
            version: "1".into(),
            trigger: WorkflowTrigger::Manual,
            steps,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn step(id: &str, delegate_to: Option<DelegationSpec>) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            agent: WorkflowAgentRef::Id("writer".into()),
            tools: vec![],
            prompt_template: "ping".into(),
            provider: None,
            model: None,
            depends_on: vec![],
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to,
        }
    }

    #[test]
    fn delegate_to_propagates_to_node() {
        let spec = DelegationSpec {
            agent: "researcher".into(),
            prompt_template: "look up {{this.output}}".into(),
            await_: true,
            push_url: None,
            child_workflow: None,
            timeout: Some(Duration::from_secs(60)),
        };
        let def = make_def(vec![step("only", Some(spec.clone()))]);
        let compiled = compile(&def).expect("compiles");
        let node = compiled
            .nodes
            .iter()
            .find(|n| n.id == "only")
            .expect("node");
        let ds = node.delegate_to.as_ref().expect("delegate_to set");
        assert_eq!(ds.agent, "researcher");
        assert!(ds.await_);
    }

    #[test]
    fn rejects_empty_agent_in_delegate_to() {
        let spec = DelegationSpec {
            agent: "".into(),
            prompt_template: "hi".into(),
            await_: true,
            push_url: None,
            child_workflow: None,
            timeout: None,
        };
        let def = make_def(vec![step("only", Some(spec))]);
        let err = compile(&def).unwrap_err();
        assert!(matches!(err, OrkError::Workflow(m) if m.contains("agent must be non-empty")));
    }

    #[test]
    fn rejects_no_prompt_and_no_child_workflow() {
        let spec = DelegationSpec {
            agent: "researcher".into(),
            prompt_template: "".into(),
            await_: true,
            push_url: None,
            child_workflow: None,
            timeout: None,
        };
        let def = make_def(vec![step("only", Some(spec))]);
        let err = compile(&def).unwrap_err();
        assert!(
            matches!(err, OrkError::Workflow(m) if m.contains("prompt_template or child_workflow"))
        );
    }

    #[test]
    fn rejects_child_workflow_with_await_false() {
        let spec = DelegationSpec {
            agent: "researcher".into(),
            prompt_template: "".into(),
            await_: false,
            push_url: None,
            child_workflow: Some(WorkflowId::new()),
            timeout: None,
        };
        let def = make_def(vec![step("only", Some(spec))]);
        let err = compile(&def).unwrap_err();
        assert!(matches!(err, OrkError::Workflow(m) if m.contains("requires await: true")));
    }

    #[test]
    fn inline_agent_ref_propagates_to_node() {
        let inline = WorkflowAgentRef::Inline {
            url: "https://vendor.example.com/.well-known/agent-card.json"
                .parse()
                .unwrap(),
            auth: None,
        };
        let mut s = step("only", None);
        s.agent = inline;
        let def = make_def(vec![s]);
        let compiled = compile(&def).expect("compiles");
        let node = compiled
            .nodes
            .iter()
            .find(|n| n.id == "only")
            .expect("node");
        match &node.agent {
            WorkflowAgentRef::Inline { url, .. } => {
                assert_eq!(url.host_str(), Some("vendor.example.com"));
            }
            other => panic!("expected Inline, got {other:?}"),
        }
    }

    #[test]
    fn bare_string_agent_round_trips_via_serde_untagged() {
        let yaml = r#"
id: only
agent: writer
tools: []
prompt_template: hello
depends_on: []
"#;
        let step: WorkflowStep = serde_yaml::from_str(yaml).expect("parse");
        match &step.agent {
            WorkflowAgentRef::Id(id) => assert_eq!(id, "writer"),
            other => panic!("expected Id, got {other:?}"),
        }
    }

    #[test]
    fn inline_agent_yaml_parses_with_optional_auth() {
        let yaml = r#"
id: vendor-step
agent:
  url: "https://vendor.example.com/.well-known/agent-card.json"
  auth:
    kind: static_bearer
    value_env: VENDOR_BEARER
tools: []
prompt_template: hello
depends_on: []
"#;
        let step: WorkflowStep = serde_yaml::from_str(yaml).expect("parse");
        match &step.agent {
            WorkflowAgentRef::Inline { url, auth } => {
                assert_eq!(url.host_str(), Some("vendor.example.com"));
                assert!(auth.is_some());
            }
            other => panic!("expected Inline, got {other:?}"),
        }
    }

    #[test]
    fn step_provider_and_model_propagate_to_node() {
        // ADR 0012 §`Selection`: the compiler must lift WorkflowStep
        // provider/model onto the WorkflowNode so the engine can thread
        // them onto AgentContext.step_llm_overrides.
        let mut s = step("only", None);
        s.provider = Some("anthropic".into());
        s.model = Some("claude-3-5-sonnet".into());
        let def = make_def(vec![s]);
        let compiled = compile(&def).expect("compiles");
        let node = compiled
            .nodes
            .iter()
            .find(|n| n.id == "only")
            .expect("node");
        assert_eq!(node.step_provider.as_deref(), Some("anthropic"));
        assert_eq!(node.step_model.as_deref(), Some("claude-3-5-sonnet"));
    }

    #[test]
    fn rejects_push_url_with_await_true() {
        let spec = DelegationSpec {
            agent: "researcher".into(),
            prompt_template: "hi".into(),
            await_: true,
            push_url: Some("https://example.com/cb".parse().unwrap()),
            child_workflow: None,
            timeout: None,
        };
        let def = make_def(vec![step("only", Some(spec))]);
        let err = compile(&def).unwrap_err();
        assert!(matches!(err, OrkError::Workflow(m) if m.contains("push_url only applies")));
    }
}
