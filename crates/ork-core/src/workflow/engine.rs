use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use ork_a2a::{AgentCallInput, MessageId, Part, Role, TaskState};
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowRunId};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::a2a::{AgentContext, AgentEvent, AgentMessage, CallerIdentity, StepLlmOverrides};
use crate::agent_registry::AgentRegistry;
use crate::embeds::{
    EmbedContext, EmbedLimits, EmbedRegistry, embed_variables_from_workflow_input, resolve_early,
};
use crate::models::workflow::{
    DelegationSpec, StepResult, StepStatus, WorkflowAgentRef, WorkflowRun, WorkflowRunStatus,
};
use crate::ports::a2a_push_repo::{A2aPushConfigRepository, A2aPushConfigRow};
use crate::ports::a2a_task_repo::{A2aTaskRepository, A2aTaskRow};
use crate::ports::agent::Agent;
use crate::ports::artifact_store::ArtifactStore;
use crate::ports::delegation_publisher::DelegationPublisher;
use crate::ports::remote_agent_builder::RemoteAgentBuilder;
use crate::ports::repository::WorkflowRepository;
use crate::workflow::compiler::{CompiledWorkflow, EdgeCondition, WorkflowNode, compile};
use crate::workflow::delegation::{DelegationOutcome, execute_one_shot_delegation};
use crate::workflow::template::{parse_json_array, resolve_template};

/// Stable cache key used by both [`WorkflowEngine::build_inline_overlay`] and
/// [`WorkflowEngine::resolve_agent`] so identical inline URLs share one
/// transient agent within a run.
fn inline_overlay_key(url: &url::Url) -> String {
    format!("inline:{url}")
}

/// Executes a compiled workflow graph against the agent runtime.
///
/// ADR 0006 plumbing: optional `publisher` enables `delegate_to: { await: false }`,
/// optional `a2a_tasks` enables parent-task linkage for the child A2A task ledger,
/// optional `run_cancel` is the per-run [`CancellationToken`] cloned into each step
/// `AgentContext.cancel` so cancelling the run cancels in-flight steps and children.
pub struct WorkflowEngine {
    repo: Arc<dyn WorkflowRepository>,
    agents: Arc<AgentRegistry>,
    publisher: Option<Arc<dyn DelegationPublisher>>,
    a2a_tasks: Option<Arc<dyn A2aTaskRepository>>,
    run_cancel: CancellationToken,
    /// ADR-0007 §`Workflow-time inline card`. When set, every
    /// `WorkflowAgentRef::Inline` in the compiled graph is materialised once at
    /// the start of a run, stored in a per-run overlay, and dropped at run end.
    /// `None` keeps the engine usable in tests that exercise only registered
    /// agents.
    remote_builder: Option<Arc<dyn RemoteAgentBuilder>>,
    /// ADR-0009 ↔ ADR-0006 push notifications: when a `delegate_to` step
    /// declares `push_url`, the engine registers an `a2a_push_configs` row so
    /// the delivery worker emits a callback once the child task reaches a
    /// terminal state. `None` disables registration (kept optional so the
    /// existing engine constructors stay usable in unit tests).
    a2a_push_repo: Option<Arc<dyn A2aPushConfigRepository>>,
    /// ADR-0015: dynamic `«type:…»` embeds on prompts (early phase).
    embed_registry: Arc<EmbedRegistry>,
    embed_limits: EmbedLimits,
    /// ADR-0016: optional blob store for tools and spillover in workflow steps.
    artifact_store: Option<Arc<dyn ArtifactStore>>,
    /// ADR-0016: public API origin for proxy `Part::file` URIs when the store has no `presign_get`.
    artifact_public_base: Option<String>,
}

/// Abstraction for executing agent tools during workflow steps.
///
/// The `ctx` argument carries the calling agent's [`AgentContext`] so
/// peer-delegation tools (ADR 0006's `agent_call`, ADR 0010's MCP, ADR 0016's
/// `artifact_*`) can read tenant, parent task id, cancel token, caller
/// identity, and delegation chain without the per-instance caller-context
/// seam ADR 0006 introduced (since removed in ADR 0011 §`Engine cleanup`).
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError>;
}

impl WorkflowEngine {
    #[must_use]
    pub fn new(repo: Arc<dyn WorkflowRepository>, agents: Arc<AgentRegistry>) -> Self {
        Self {
            repo,
            agents,
            publisher: None,
            a2a_tasks: None,
            run_cancel: CancellationToken::new(),
            remote_builder: None,
            a2a_push_repo: None,
            embed_registry: Arc::new(EmbedRegistry::with_builtins()),
            embed_limits: EmbedLimits::default(),
            artifact_store: None,
            artifact_public_base: None,
        }
    }

    /// ADR-0016: wire the shared [`ArtifactStore`] into every step [`AgentContext`].
    #[must_use]
    pub fn with_artifact_store(mut self, store: Option<Arc<dyn ArtifactStore>>) -> Self {
        self.artifact_store = store;
        self
    }

    /// ADR-0016: public base URL (no trailing path) for proxy artifact URLs; pair with
    /// [`Self::with_artifact_store`].
    #[must_use]
    pub fn with_artifact_public_base(mut self, base: Option<String>) -> Self {
        self.artifact_public_base = base;
        self
    }

    /// ADR-0009 ↔ ADR-0006 push notifications: install the
    /// [`A2aPushConfigRepository`] used to register a push config for every
    /// `delegate_to` step that supplies a `push_url`. Without this the engine
    /// silently drops `push_url` when delegating, so production wiring is
    /// strongly encouraged; tests that exercise only synchronous delegation
    /// can leave it unset.
    #[must_use]
    pub fn with_push_repo(mut self, repo: Arc<dyn A2aPushConfigRepository>) -> Self {
        self.a2a_push_repo = Some(repo);
        self
    }

    /// Engine wired for ADR 0006 delegation: pass the [`DelegationPublisher`] (for
    /// `await: false`), the [`A2aTaskRepository`] (for parent-task linkage), and a
    /// per-run [`CancellationToken`] cloned into each step's [`AgentContext`].
    #[must_use]
    pub fn with_delegation(
        mut self,
        publisher: Option<Arc<dyn DelegationPublisher>>,
        a2a_tasks: Option<Arc<dyn A2aTaskRepository>>,
        run_cancel: CancellationToken,
    ) -> Self {
        self.publisher = publisher;
        self.a2a_tasks = a2a_tasks;
        self.run_cancel = run_cancel;
        self
    }

    /// Wire the [`RemoteAgentBuilder`] used to materialise transient agents for
    /// `WorkflowAgentRef::Inline` steps. Without this, inline cards fall through
    /// to a clear `Validation` error at run-time so misconfiguration is loud
    /// instead of silently swallowed.
    #[must_use]
    pub fn with_remote_builder(mut self, builder: Arc<dyn RemoteAgentBuilder>) -> Self {
        self.remote_builder = Some(builder);
        self
    }

    /// Use the same [`EmbedRegistry`] / [`EmbedLimits`] as the API and gateways (one process-wide
    /// set of handlers and caps).
    #[must_use]
    pub fn with_embeds(mut self, registry: Arc<EmbedRegistry>, limits: EmbedLimits) -> Self {
        self.embed_registry = registry;
        self.embed_limits = limits;
        self
    }

    /// ADR-0015: expand `«type:…»` after `{{…}}` templating, before the LLM or delegation hop.
    ///
    /// `«var:…»` reads [`crate::embeds::embed_variables_from_workflow_input`] (from
    /// `embed_variables` on the run JSON) plus the optional per-call `extra_variables` overlay.
    async fn resolve_prompt_embeds(
        &self,
        tenant_id: TenantId,
        task_id: Option<ork_a2a::TaskId>,
        prompt: &str,
        workflow_input: &Value,
        extra_variables: &HashMap<String, String>,
    ) -> Result<String, OrkError> {
        let mut variables = embed_variables_from_workflow_input(workflow_input);
        for (k, v) in extra_variables {
            variables.insert(k.clone(), v.clone());
        }
        let mut ctx = EmbedContext::with_limits(
            tenant_id,
            None,
            task_id,
            self.a2a_tasks.clone(),
            Utc::now(),
            variables,
            &self.embed_limits,
        );
        ctx.artifact_store = self.artifact_store.clone();
        ctx.artifact_public_base = self.artifact_public_base.clone();
        resolve_early(prompt, &ctx, &self.embed_registry, &self.embed_limits).await
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        run: &mut WorkflowRun,
        graph: &CompiledWorkflow,
    ) -> Result<(), OrkError> {
        info!(
            run_id = %run.id,
            workflow = %graph.name,
            "workflow run started - {} steps in graph",
            graph.nodes.len()
        );

        self.repo
            .update_run_status(tenant_id, run.id, WorkflowRunStatus::Running, None)
            .await?;
        run.status = WorkflowRunStatus::Running;

        // ADR-0007 §`Workflow-time inline card`. Build the per-run overlay once.
        // The overlay is keyed by the synthetic `inline:<run_id>:<step_id>` id so
        // logs/metrics line up with the step it backs. It drops at the end of
        // `execute()` — no global registry mutation.
        let overlay = self.build_inline_overlay(graph, run.id).await?;

        let mut step_outputs: HashMap<String, String> = HashMap::new();
        let mut current_node_id = graph.entry_node.clone();

        loop {
            let node = graph
                .nodes
                .iter()
                .find(|n| n.id == current_node_id)
                .ok_or_else(|| {
                    OrkError::Workflow(format!("node '{}' not found in graph", current_node_id))
                })?;

            let agent_label = node.agent.display_id();
            // `run_id` lets the demo's `ork-api.log` tail filter (which
            // greps for the polled run id) actually surface per-step
            // lifecycle events. Without it, only service-layer create /
            // start lines made it through the filter, so the timeout
            // dump was useless when more than one workflow run was in
            // flight on the engine. Mirrors the field on `step failed`
            // and `step finished` below.
            info!(
                run_id = %run.id,
                step = %node.id,
                agent = %agent_label,
                "step started - agent is running"
            );

            let step_start = Utc::now();

            let raw_result = if node.for_each.is_some() {
                self.execute_for_each_step(
                    tenant_id,
                    run.id,
                    node,
                    &step_outputs,
                    &run.input,
                    &overlay,
                )
                .await
            } else {
                let prompt =
                    resolve_template(&node.prompt_template, &step_outputs, &run.input, None);
                self.execute_agent_step(
                    tenant_id, run.id, node, &prompt, &run.input, None, &overlay,
                )
                .await
            };

            // ADR 0006: optional delegation hop after the parent step succeeds.
            let result = match (raw_result, node.delegate_to.as_ref()) {
                (Ok(parent_output), Some(spec)) => {
                    match self
                        .execute_delegated_call(
                            tenant_id,
                            run,
                            node,
                            spec,
                            &parent_output,
                            &step_outputs,
                        )
                        .await
                    {
                        Ok(child_output) => {
                            // Surface the child output via `<step_id>.delegated.output`
                            // for downstream steps (e.g. `next.prompt = ...{{step.delegated.output}}...`).
                            step_outputs
                                .insert(format!("{}.delegated", node.id), child_output.clone());
                            // The parent step's primary output remains its own.
                            Ok(parent_output)
                        }
                        Err(e) => Err(e),
                    }
                }
                (other, _) => other,
            };

            let step_result = match &result {
                Ok(output) => {
                    step_outputs.insert(node.id.clone(), output.clone());
                    StepResult {
                        step_id: node.id.clone(),
                        agent: agent_label.clone(),
                        status: StepStatus::Completed,
                        output: Some(output.clone()),
                        error: None,
                        started_at: step_start,
                        completed_at: Some(Utc::now()),
                    }
                }
                Err(e) => {
                    error!(run_id = %run.id, step = %node.id, error = %e, "step failed");
                    StepResult {
                        step_id: node.id.clone(),
                        agent: agent_label.clone(),
                        status: StepStatus::Failed,
                        output: None,
                        error: Some(e.to_string()),
                        started_at: step_start,
                        completed_at: Some(Utc::now()),
                    }
                }
            };

            self.repo
                .append_step_result(tenant_id, run.id, &step_result)
                .await?;
            run.step_results.push(step_result.clone());

            let elapsed_ms = Utc::now()
                .signed_duration_since(step_start)
                .num_milliseconds()
                .max(0);
            if result.is_ok() {
                info!(
                    run_id = %run.id,
                    step = %node.id,
                    agent = %agent_label,
                    elapsed_ms = elapsed_ms,
                    "step finished"
                );
            }

            let step_passed = result.is_ok();

            let next = graph.edges.iter().find(|e| {
                e.from == current_node_id
                    && match &e.condition {
                        Some(EdgeCondition::Always) => true,
                        Some(EdgeCondition::OnPass) => step_passed,
                        Some(EdgeCondition::OnFail) => !step_passed,
                        None => true,
                    }
            });

            match next {
                Some(edge) => {
                    current_node_id = edge.to.clone();
                }
                None => {
                    let final_status = if step_passed {
                        WorkflowRunStatus::Completed
                    } else {
                        WorkflowRunStatus::Failed
                    };

                    let output = step_outputs
                        .get(&current_node_id)
                        .map(|s| serde_json::Value::String(s.clone()));

                    self.repo
                        .update_run_status(tenant_id, run.id, final_status, output.clone())
                        .await?;
                    run.status = final_status;
                    run.output = output;
                    run.completed_at = Some(Utc::now());

                    info!(
                        run_id = %run.id,
                        status = %final_status,
                        "workflow finished - status {}",
                        final_status
                    );
                    break;
                }
            }
        }

        Ok(())
    }

    async fn execute_for_each_step(
        &self,
        tenant_id: TenantId,
        workflow_run_id: WorkflowRunId,
        node: &WorkflowNode,
        step_outputs: &HashMap<String, String>,
        workflow_input: &Value,
        overlay: &HashMap<String, Arc<dyn Agent>>,
    ) -> Result<String, OrkError> {
        let agent_label = node.agent.display_id();
        let for_each_tmpl = node
            .for_each
            .as_ref()
            .ok_or_else(|| OrkError::Workflow("internal: for_each step missing template".into()))?;
        let list_templated = resolve_template(for_each_tmpl, step_outputs, workflow_input, None);
        let resolved_list = self
            .resolve_prompt_embeds(
                tenant_id,
                None,
                &list_templated,
                workflow_input,
                &HashMap::new(),
            )
            .await?;
        let items: Vec<Value> = parse_json_array(&resolved_list)?;
        let var_name = node
            .iteration_var
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("item");

        let total = items.len();
        info!(
            run_id = %workflow_run_id,
            step = %node.id,
            agent = %agent_label,
            iterations = total,
            "fan-out: running this step once per repository/item (sequential)"
        );

        let mut outputs: Vec<Value> = Vec::with_capacity(items.len());
        for (idx, item) in items.into_iter().enumerate() {
            let label = item
                .get("name")
                .or_else(|| item.get("repository"))
                .and_then(|v| v.as_str())
                .unwrap_or("(item)");
            info!(
                run_id = %workflow_run_id,
                step = %node.id,
                agent = %agent_label,
                iteration = idx + 1,
                of = total,
                target = %label,
                "iteration - agent working on this target"
            );
            let prompt = resolve_template(
                &node.prompt_template,
                step_outputs,
                workflow_input,
                Some((var_name, &item)),
            );
            let text = self
                .execute_agent_step(
                    tenant_id,
                    workflow_run_id,
                    node,
                    &prompt,
                    workflow_input,
                    Some((var_name.to_string(), item.clone())),
                    overlay,
                )
                .await?;
            info!(
                run_id = %workflow_run_id,
                step = %node.id,
                iteration = idx + 1,
                of = total,
                target = %label,
                "iteration done"
            );
            outputs.push(Value::String(text));
        }

        serde_json::to_string(&outputs)
            .map_err(|e| OrkError::Workflow(format!("serialize for_each outputs: {e}")))
    }

    // ADR 0006 + ADR 0012 + ADR 0018 incrementally added per-step inputs
    // (workflow_run_id for `a2a_tasks` linkage, iteration for `for_each`,
    // workflow_input for prompt templating). Splitting them into a struct
    // would shuffle every test fixture without making the call site
    // clearer; allow the lint until the engine is folded under a builder.
    #[allow(clippy::too_many_arguments)]
    async fn execute_agent_step(
        &self,
        tenant_id: TenantId,
        workflow_run_id: WorkflowRunId,
        node: &WorkflowNode,
        prompt: &str,
        workflow_input: &Value,
        iteration: Option<(String, Value)>,
        overlay: &HashMap<String, Arc<dyn Agent>>,
    ) -> Result<String, OrkError> {
        let agent = self.resolve_agent(&node.agent, overlay).await?;
        let step_task_id = ork_a2a::TaskId::new();
        let prompt = self
            .resolve_prompt_embeds(
                tenant_id,
                Some(step_task_id),
                prompt,
                workflow_input,
                &HashMap::new(),
            )
            .await?;

        // ADR 0012 §`Selection`: lift WorkflowStep provider/model overrides
        // (already on `node` via the compiler) onto the per-step
        // `AgentContext` so `LocalAgent::send_stream` can prefer them over
        // its own `AgentConfig` defaults when building the `ChatRequest`.
        // `None` on both fields means "no step-level override active".
        let step_llm_overrides = match (node.step_provider.as_ref(), node.step_model.as_ref()) {
            (None, None) => None,
            _ => Some(StepLlmOverrides {
                provider: node.step_provider.clone(),
                model: node.step_model.clone(),
            }),
        };

        let ctx = AgentContext {
            tenant_id,
            task_id: step_task_id,
            parent_task_id: None,
            cancel: self.run_cancel.child_token(),
            caller: CallerIdentity {
                tenant_id,
                user_id: None,
                scopes: vec![],
                ..CallerIdentity::default()
            },
            push_notification_url: None,
            trace_ctx: None,
            context_id: None,
            workflow_input: workflow_input.clone(),
            iteration,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
            step_llm_overrides,
            artifact_store: self.artifact_store.clone(),
            artifact_public_base: self.artifact_public_base.clone(),
        };

        // ADR 0006 §`Persistence` / demo `Known engine gaps` regression:
        // any `agent_call` / `peer_*` / `delegate_to:` the agent makes
        // below inserts a child a2a_tasks row with
        // `parent_task_id = ctx.task_id`. If we do not insert the parent
        // first, Postgres rejects the child with
        // `a2a_tasks_parent_task_id_fkey`. Persist the parent row here so
        // the FK holds; we mark it Completed/Failed once the agent stream
        // finishes.
        self.persist_parent_task_row(
            tenant_id,
            workflow_run_id,
            agent.id().clone(),
            &ctx,
            &node.id,
        )
        .await?;

        let msg = AgentMessage {
            role: Role::User,
            parts: vec![Part::Text {
                text: prompt.to_string(),
                metadata: None,
            }],
            message_id: MessageId::new(),
            task_id: Some(ctx.task_id),
            context_id: ctx.context_id,
            metadata: None,
        };

        let parent_task_id = ctx.task_id;
        let stream_result = agent.send_stream(ctx, msg).await;
        let mut content = String::new();
        let mut errored: Option<OrkError> = None;
        match stream_result {
            Ok(mut stream) => {
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(AgentEvent::Message(m)) => {
                            for part in &m.parts {
                                if let Part::Text { text, .. } = part {
                                    content.push_str(text);
                                }
                            }
                        }
                        Ok(AgentEvent::StatusUpdate(_) | AgentEvent::ArtifactUpdate(_)) => {}
                        Err(e) => {
                            errored = Some(e);
                            break;
                        }
                    }
                }
            }
            Err(e) => errored = Some(e),
        }

        // Best-effort terminal-state update. If this fails we still surface
        // the agent outcome — the row stays in `Working` and the engine's
        // `step_results` table is the source of truth for the workflow.
        if let Some(repo) = &self.a2a_tasks {
            let final_state = if errored.is_some() {
                TaskState::Failed
            } else {
                TaskState::Completed
            };
            if let Err(e) = repo
                .update_state(tenant_id, parent_task_id, final_state)
                .await
            {
                warn!(
                    run_id = %workflow_run_id,
                    error = %e,
                    step = %node.id,
                    task_id = %parent_task_id,
                    "ADR-0006: failed to update parent a2a_tasks state"
                );
            }
        }

        if let Some(e) = errored {
            return Err(e);
        }
        Ok(content)
    }

    /// Insert the parent row in `a2a_tasks` for an agent step or
    /// delegation hop. No-op when the engine was constructed without an
    /// `A2aTaskRepository` (e.g. CLI / unit tests). Called from
    /// [`Self::execute_agent_step`] and [`Self::execute_delegated_call`]
    /// before any code path that may insert a child row referencing
    /// `parent_task_id = ctx.task_id`.
    async fn persist_parent_task_row(
        &self,
        tenant_id: TenantId,
        workflow_run_id: WorkflowRunId,
        agent_id: crate::a2a::AgentId,
        ctx: &AgentContext,
        step_id: &str,
    ) -> Result<(), OrkError> {
        let Some(repo) = &self.a2a_tasks else {
            return Ok(());
        };
        let now = Utc::now();
        repo.create_task(&A2aTaskRow {
            id: ctx.task_id,
            context_id: ctx.context_id.unwrap_or_default(),
            tenant_id,
            agent_id,
            // The engine-minted parent is the top-level task for this
            // step — it has no caller-side parent in `a2a_tasks`. Cross-
            // run linkage (`parent_run.parent_task_id`) is intentionally
            // dropped here because that id was minted by another engine
            // run that may not have persisted it; threading the chain
            // safely is ADR-0008 follow-up territory.
            parent_task_id: None,
            workflow_run_id: Some(workflow_run_id),
            state: TaskState::Working,
            metadata: serde_json::json!({
                "step_id": step_id,
                "source": "workflow.engine",
            }),
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
        .await
    }

    /// Execute a `delegate_to:` hop after the parent step's output is available.
    ///
    /// Returns the *child*'s output as a flat string (text concatenation for sync;
    /// `{"task_id":...}` JSON for fire-and-forget; the child run's serialized output
    /// for `child_workflow`). The string is written into `step_outputs` under
    /// `<step_id>.delegated.output` for downstream template references.
    async fn execute_delegated_call(
        &self,
        tenant_id: TenantId,
        parent_run: &WorkflowRun,
        parent_node: &WorkflowNode,
        spec: &DelegationSpec,
        parent_output: &str,
        step_outputs: &HashMap<String, String>,
    ) -> Result<String, OrkError> {
        // TODO(ADR-0021): once the central RBAC helper lands, gate this on the
        //                 `agent:<spec.agent>:delegate` scope of the calling identity.

        info!(
            run_id = %parent_run.id,
            step = %parent_node.id,
            target = %spec.agent,
            await_ = spec.await_,
            child_workflow = spec.child_workflow.is_some(),
            "delegate_to hop starting"
        );

        if let Some(child_workflow_id) = spec.child_workflow {
            return self
                .execute_child_workflow(
                    tenant_id,
                    parent_run,
                    parent_node,
                    child_workflow_id,
                    parent_output,
                )
                .await;
        }

        // Build the parent context. We mirror what `execute_agent_step` would build, then
        // ask the shared delegation helper to fork a child off it.
        let mut step_outputs_with_self = step_outputs.clone();
        step_outputs_with_self.insert("this".into(), parent_output.to_string());
        let prompt_templ = resolve_template(
            &spec.prompt_template,
            &step_outputs_with_self,
            &parent_run.input,
            None,
        );
        let delegate_ctx_task_id = ork_a2a::TaskId::new();
        let prompt = self
            .resolve_prompt_embeds(
                tenant_id,
                Some(delegate_ctx_task_id),
                &prompt_templ,
                &parent_run.input,
                &HashMap::new(),
            )
            .await?;
        let parent_ctx = AgentContext {
            tenant_id,
            task_id: delegate_ctx_task_id,
            parent_task_id: parent_run.parent_task_id,
            cancel: self.run_cancel.child_token(),
            caller: CallerIdentity {
                tenant_id,
                user_id: None,
                scopes: vec![],
                ..CallerIdentity::default()
            },
            push_notification_url: spec.push_url.clone(),
            trace_ctx: None,
            context_id: None,
            workflow_input: parent_run.input.clone(),
            iteration: None,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
            step_llm_overrides: None,
            artifact_store: self.artifact_store.clone(),
            artifact_public_base: self.artifact_public_base.clone(),
        };

        // ADR 0006 §`Persistence` / demo `Known engine gaps` regression:
        // the helper below inserts the *child* row with
        // `parent_task_id = parent_ctx.task_id`. Without inserting the
        // parent first, Postgres rejects the child with
        // `a2a_tasks_parent_task_id_fkey`. Use the synthesized step-agent
        // id for the row's `agent_id` since this parent context represents
        // the workflow step's delegation hop, not the delegation target.
        self.persist_parent_task_row(
            tenant_id,
            parent_run.id,
            parent_node.agent.display_id(),
            &parent_ctx,
            &parent_node.id,
        )
        .await?;

        let input = AgentCallInput {
            agent: spec.agent.clone(),
            prompt,
            data: None,
            files: Vec::new(),
            await_: spec.await_,
            stream: false,
        };

        let outcome: DelegationOutcome = execute_one_shot_delegation(
            &parent_ctx,
            &self.agents,
            self.publisher.as_ref(),
            self.a2a_tasks.as_ref(),
            Some(parent_run.id),
            input,
        )
        .await?;

        // ADR-0009 ↔ ADR-0006: register a push config row keyed on the child
        // task id whenever the spec carries a `push_url`. The config is only
        // meaningful for `await: false`; the compiler already enforces that in
        // `validate_delegation_spec`. We still gate on `is_some` here because
        // spec-level validation is best-effort across versions.
        if let (Some(push_url), Some(repo)) = (spec.push_url.as_ref(), self.a2a_push_repo.as_ref())
        {
            let row = A2aPushConfigRow {
                id: uuid::Uuid::now_v7(),
                task_id: outcome.task_id,
                tenant_id,
                url: push_url.clone(),
                token: None,
                authentication: None,
                metadata: serde_json::json!({
                    "source": "workflow.delegate_to",
                    "parent_step": parent_node.id,
                    "parent_run_id": parent_run.id.to_string(),
                }),
                created_at: chrono::Utc::now(),
            };
            if let Err(e) = repo.upsert(&row).await {
                warn!(
                    run_id = %parent_run.id,
                    error = %e,
                    step = %parent_node.id,
                    child_task_id = %outcome.task_id,
                    "ADR-0009: failed to register delegate_to push config"
                );
            }
        }

        if !spec.await_ {
            return Ok(outcome.to_tool_value().to_string());
        }

        // Sync path: surface the concatenated text reply, falling back to the JSON form
        // when the reply has no text parts (e.g. data-only responses).
        let reply = &outcome.reply;
        let text_only = reply
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if text_only.is_empty() {
            Ok(outcome.to_tool_value().to_string())
        } else {
            Ok(text_only)
        }
    }

    /// Resolve a [`WorkflowAgentRef`] to a callable [`Agent`]. Bare ids go through
    /// the registry; inline cards are looked up in the per-run `overlay` (never
    /// the registry, never globally registered) so each run is hermetic.
    async fn resolve_agent(
        &self,
        agent_ref: &WorkflowAgentRef,
        overlay: &HashMap<String, Arc<dyn Agent>>,
    ) -> Result<Arc<dyn Agent>, OrkError> {
        match agent_ref {
            WorkflowAgentRef::Id(id) => self
                .agents
                .resolve(&id.clone())
                .await
                .ok_or_else(|| OrkError::NotFound(format!("agent '{id}' not registered"))),
            WorkflowAgentRef::Inline { url, .. } => {
                let key = inline_overlay_key(url);
                overlay.get(&key).cloned().ok_or_else(|| {
                    OrkError::Workflow(format!(
                        "inline agent '{key}' was not built into the per-run overlay; \
                         did the engine receive a RemoteAgentBuilder via with_remote_builder?"
                    ))
                })
            }
        }
    }

    /// Build the per-run overlay of transient agents for every
    /// `WorkflowAgentRef::Inline` step in `graph`. Returns an empty map when no
    /// inline cards are present (so registered-only workflows incur zero cost).
    /// Errors here fail the whole run before any step is executed — partial
    /// runs with half-resolved inline cards are not allowed.
    async fn build_inline_overlay(
        &self,
        graph: &CompiledWorkflow,
        run_id: ork_common::types::WorkflowRunId,
    ) -> Result<HashMap<String, Arc<dyn Agent>>, OrkError> {
        let mut overlay: HashMap<String, Arc<dyn Agent>> = HashMap::new();
        let inline_steps: Vec<&WorkflowNode> = graph
            .nodes
            .iter()
            .filter(|n| matches!(&n.agent, WorkflowAgentRef::Inline { .. }))
            .collect();
        if inline_steps.is_empty() {
            return Ok(overlay);
        }
        let builder = self.remote_builder.as_ref().ok_or_else(|| {
            OrkError::Validation(
                "workflow contains inline agent cards but the engine has no \
                 RemoteAgentBuilder; wire one via WorkflowEngine::with_remote_builder()"
                    .into(),
            )
        })?;
        for node in inline_steps {
            let WorkflowAgentRef::Inline { url, auth } = &node.agent else {
                unreachable!("filtered above");
            };
            let key = inline_overlay_key(url);
            if overlay.contains_key(&key) {
                continue;
            }
            let agent = builder.build_inline(url.clone(), auth.clone()).await?;
            info!(
                run_id = %run_id,
                step = %node.id,
                inline_key = %key,
                "ADR-0007: built transient inline A2aRemoteAgent"
            );
            overlay.insert(key, agent);
        }
        Ok(overlay)
    }

    /// Fork a child [`WorkflowRun`] for `delegate_to: { child_workflow: <id> }` and
    /// return its serialized output.
    async fn execute_child_workflow(
        &self,
        tenant_id: TenantId,
        parent_run: &WorkflowRun,
        parent_node: &WorkflowNode,
        child_workflow_id: ork_common::types::WorkflowId,
        parent_output: &str,
    ) -> Result<String, OrkError> {
        let definition = self
            .repo
            .get_definition(tenant_id, child_workflow_id)
            .await?;
        let compiled = compile(&definition)?;

        let child_task_id = ork_a2a::TaskId::new();
        let mut child_run = WorkflowRun {
            id: ork_common::types::WorkflowRunId::new(),
            workflow_id: child_workflow_id,
            tenant_id,
            status: WorkflowRunStatus::Pending,
            input: serde_json::json!({
                "parent_step_output": parent_output,
                "parent_input": parent_run.input.clone(),
            }),
            output: None,
            step_results: Vec::new(),
            started_at: Utc::now(),
            completed_at: None,
            parent_run_id: Some(parent_run.id),
            parent_step_id: Some(parent_node.id.clone()),
            parent_task_id: Some(child_task_id),
        };
        child_run = self.repo.create_run(&child_run).await?;

        if let Some(repo) = &self.a2a_tasks {
            let now = Utc::now();
            repo.create_task(&A2aTaskRow {
                id: child_task_id,
                context_id: ork_a2a::ContextId::new(),
                tenant_id,
                agent_id: format!("workflow:{child_workflow_id}"),
                parent_task_id: parent_run.parent_task_id,
                workflow_run_id: Some(child_run.id),
                state: TaskState::Working,
                metadata: serde_json::json!({}),
                created_at: now,
                updated_at: now,
                completed_at: None,
            })
            .await?;
        }

        // Sub-engine inherits the same wiring; the child run's cancel is a child of ours.
        let sub_engine = WorkflowEngine {
            repo: self.repo.clone(),
            agents: self.agents.clone(),
            publisher: self.publisher.clone(),
            a2a_tasks: self.a2a_tasks.clone(),
            run_cancel: self.run_cancel.child_token(),
            remote_builder: self.remote_builder.clone(),
            a2a_push_repo: self.a2a_push_repo.clone(),
            embed_registry: self.embed_registry.clone(),
            embed_limits: self.embed_limits.clone(),
            artifact_store: self.artifact_store.clone(),
            artifact_public_base: self.artifact_public_base.clone(),
        };

        // Box the recursive `execute` future to break the otherwise-infinite future size.
        let exec_result = Box::pin(sub_engine.execute(tenant_id, &mut child_run, &compiled)).await;

        let final_state = if exec_result.is_ok() && child_run.status == WorkflowRunStatus::Completed
        {
            TaskState::Completed
        } else {
            TaskState::Failed
        };
        if let Some(repo) = &self.a2a_tasks {
            let _ = repo
                .update_state(tenant_id, child_task_id, final_state)
                .await;
        }

        if let Err(e) = exec_result {
            warn!(
                run_id = %parent_run.id,
                step = %parent_node.id,
                child_run_id = %child_run.id,
                error = %e,
                "child workflow failed"
            );
            return Err(e);
        }

        Ok(child_run.output.map(|v| v.to_string()).unwrap_or_default())
    }
}
