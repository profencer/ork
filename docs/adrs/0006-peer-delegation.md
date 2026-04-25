# 0006 — Peer delegation: `agent_call` tool and `delegate` workflow step

- **Status:** Proposed
- **Date:** 2026-04-24
- **Phase:** 2
- **Relates to:** 0002, 0004, 0005, 0007, 0008, 0018

## Context

ork's workflow engine wires agents together exclusively through the DAG: a step's `depends_on` plus the engine's edge resolver in [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) decide who runs next. Agents themselves cannot ask another agent to do work mid-step; they have no way to say "Researcher, please go look this up while I keep planning."

Real A2A interactions are conversational. SAM exposes peer delegation through:

- **`PeerAgentTool`** ([`agent/tools/peer_agent_tool.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/peer_agent_tool.py)) — discovered peers are surfaced to the LLM as tools whose descriptions are taken from each peer's `AgentCard`. The LLM calls them like any other tool.
- **`workflow_tool`** — first-class delegation inside SAM's workflow DAG executor, with the child task persisted and observable.

These two mechanisms cover two distinct needs: low-ceremony LLM-driven delegation, and high-control workflow-author-driven delegation. ork needs both.

## Decision

ork **introduces two delegation mechanisms** that share the same underlying `Agent` port from ADR [`0002`](0002-agent-port.md) and the same A2A message types from ADR [`0003`](0003-a2a-protocol-model.md).

### a) `agent_call` tool — minimum-viable peer delegation

A new built-in tool registered in [`CompositeToolExecutor`](../../crates/ork-integrations/src/tools.rs):

```jsonc
// Tool input schema (JSON-Schema)
{
  "type": "object",
  "required": ["agent", "prompt"],
  "properties": {
    "agent": { "type": "string", "description": "Target agent id (from registry)" },
    "prompt": { "type": "string" },
    "data":   { "type": "object", "description": "Optional structured payload (becomes a DataPart)" },
    "files":  { "type": "array", "items": { "$ref": "#/definitions/FileRef" } },
    "await":  { "type": "boolean", "default": true },
    "stream": { "type": "boolean", "default": false }
  }
}
```

Implementation outline:

```rust
"agent_call" => {
    let target = AgentId::parse(input["agent"].as_str()?)?;
    let target_agent = self.registry.resolve(&target)
        .ok_or_else(|| OrkError::Integration(format!("unknown agent: {target}")))?;

    let msg = AgentMessage::from_input(&input)?; // builds parts from prompt/data/files

    let ctx = AgentContext {
        tenant_id,
        task_id: TaskId::new(),
        parent_task_id: Some(caller_task_id),
        cancel: caller_cancel.clone(),
        ..Default::default()
    };

    if !input["await"].as_bool().unwrap_or(true) {
        // Fire-and-forget: publish to ork.a2a.v1.agent.request.<target> (ADR 0004)
        self.kafka.publish_request(&target, &msg, &ctx).await?;
        Ok(json!({ "task_id": ctx.task_id, "delivery": "fire_and_forget" }))
    } else if input["stream"].as_bool().unwrap_or(false) {
        // Synchronous streaming: forward AgentEvents back to caller's status channel
        let mut stream = target_agent.send_stream(ctx.clone(), msg).await?;
        let mut accumulated = AgentMessage::empty(Role::Agent);
        while let Some(ev) = stream.next().await { accumulated.merge_event(ev?); }
        Ok(json!({ "task_id": ctx.task_id, "reply": accumulated.to_tool_value() }))
    } else {
        let reply = target_agent.send(ctx.clone(), msg).await?;
        Ok(json!({ "task_id": ctx.task_id, "reply": reply.to_tool_value() }))
    }
}
```

The tool is **always** available to every agent (subject to RBAC in ADR [`0021`](0021-rbac-scopes.md)). Agents do not need to declare it in their YAML `tools:` list — it's a builtin like SAM's peer tools.

**LLM tool surface:** when ADR [`0011`](0011-native-llm-tool-calling.md) lands native tool calling, the registry expands `agent_call` into one tool per discovered peer with descriptions sourced from each peer's `AgentCard.skills`, mirroring SAM's [`PeerAgentTool`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/peer_agent_tool.py). Concretely, the registry generates tools named `peer_<agent_id>_<skill_id>` plus a generic `agent_call` for unstructured cases.

### b) `delegate` workflow step — first-class controlled delegation

A new optional field on [`WorkflowStep`](../../crates/ork-core/src/models/workflow.rs) and [`WorkflowNode`](../../crates/ork-core/src/workflow/compiler.rs):

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelegationSpec {
    /// Target agent id (resolved through AgentRegistry, may be local or remote).
    pub agent: String,
    /// Prompt template for the child task; same syntax as WorkflowStep.prompt_template.
    pub prompt_template: String,
    /// Whether the parent step blocks on the child task.
    #[serde(default = "default_true")]
    pub r#await: bool,
    /// Optional push-notification URL for fire-and-forget (await=false) results.
    pub push_url: Option<String>,
    /// Workflow id to invoke instead of a single send (creates a child WorkflowRun).
    pub child_workflow: Option<WorkflowId>,
    /// Per-call timeout; falls back to engine default.
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct WorkflowNode {
    pub id: String,
    pub agent: String,
    pub tools: Vec<String>,
    pub prompt_template: String,
    pub for_each: Option<String>,
    pub iteration_var: Option<String>,
    pub delegate_to: Option<DelegationSpec>,   // NEW
}
```

YAML example:

```yaml
- id: scan
  agent: planner
  prompt_template: "Decide what to scan for repo {{input.repo}}"
  delegate_to:
    agent: vendor.security_scanner
    prompt_template: "Scan {{input.repo}} for CVEs in scope {{this.output}}"
    await: true
    timeout: 60s
```

Engine behaviour ([`WorkflowEngine::execute_agent_step`](../../crates/ork-core/src/workflow/engine.rs)):

1. Run the parent agent step normally to produce its output.
2. If `delegate_to` is present, build a child `AgentMessage` from `delegate_to.prompt_template` (with `{{this.output}}` referring to the parent step's output).
3. **Fork** a child `WorkflowRun` row in [`PgWorkflowRepository`](../../crates/ork-persistence/src/postgres/workflow_repo.rs) with `parent_run_id` = current run, `parent_step_id` = node id. Persist the parent↔child link in the new `a2a_tasks` table from ADR [`0008`](0008-a2a-server-endpoints.md).
4. If `await: true`: call `target.send_stream(...)`, collect the final message, store as the step's downstream output (placeholder `{{<step_id>.delegated.output}}` available to later steps).
5. If `await: false`: publish on `ork.a2a.v1.agent.request.<agent>` (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)) and continue immediately. The child completion eventually fires the push URL or appears in the parent's `delegated_results` log.
6. If `child_workflow` is set: create a child `WorkflowRun` of that workflow definition rather than a single `Agent::send` call. Same parent linkage applies.

**Output addressing in templates:** `{{<step_id>.output}}` keeps its existing meaning (the parent step's own output); `{{<step_id>.delegated.output}}` resolves to the most recent completed delegation. ADR [`0015`](0015-dynamic-embeds.md) extends this with embed expressions.

### Discovery and tool surface

For both mechanisms the target agent must resolve through [`AgentRegistry`](../../crates/ork-agents/src/registry.rs) (local + remote, per ADR [`0005`](0005-agent-card-and-devportal-discovery.md)). Unknown agent ids are an immediate error with `TaskState::Rejected`; agents that are "known but TTL-expired" return `TaskState::Failed` with reason `"peer_offline"`.

### Cancellation propagation

The parent's `CancellationToken` (introduced in ADR [`0002`](0002-agent-port.md)) is cloned into `AgentContext.cancel` for the child task. When a parent task is cancelled via A2A `tasks/cancel` (ADR [`0008`](0008-a2a-server-endpoints.md)) or workflow cancel, all `await: true` children cancel; `await: false` children are best-effort cancelled by publishing a cancel event on `ork.a2a.v1.agent.cancel` keyed by `task_id`.

## Consequences

### Positive

- LLM-driven and workflow-author-driven delegation share one substrate, removing the long-standing mismatch between [`AgentRegistry::invoke`](../../crates/ork-agents/src/registry.rs) and the engine.
- The `agent_call` tool is the single smallest viable A2A hop — landable in a small PR right after ADRs [`0002`](0002-agent-port.md) + [`0011`](0011-native-llm-tool-calling.md).
- The `delegate_to` step yields a persisted parent↔child linkage, which gives us an audit trail for SLA debugging that SAM's tool-only path lacks.
- Fire-and-forget delegation rides Kafka and integrates with push notifications (ADR [`0009`](0009-push-notifications.md)) for free.

### Negative / costs

- Two delegation surfaces means two places where RBAC and budget checks must apply. ADR [`0021`](0021-rbac-scopes.md) defines the scope `agent:<target>:delegate` and requires both code paths to call the same authorisation helper.
- Recursive delegation cycles are possible. We add a hard cap (`max_delegation_depth`, default 8) tracked through `AgentContext` and an explicit cycle check on `parent_task_id` chain.
- The engine becomes meaningfully more complex for `child_workflow` mode (it has to start a sub-engine). Tested in isolation.

### Neutral / follow-ups

- ADR [`0018`](0018-dag-executor-enhancements.md) extends the engine for parallel fan-out, which interacts with multi-`delegate_to` steps.
- ADR [`0009`](0009-push-notifications.md) covers the callback path for `await: false`.
- A future ADR may add deadline/budget propagation across delegation chains.

## Alternatives considered

- **Tool-only (no `delegate_to` step).** Rejected: workflow authors lose static visibility of delegation topology; can't audit/observe at design time.
- **`delegate_to` only (no `agent_call` tool).** Rejected: the LLM cannot decide at runtime to ask a peer for help, which is the core value of an agent mesh.
- **Synchronous-only delegation.** Rejected: blocks the parent task on long peer work; defeats the point of an event-driven mesh and breaks for slow/expensive peers.
- **Make `agent_call` an integration adapter rather than a builtin.** Rejected: it is fundamental to the agent runtime, not an external integration.

## Affected ork modules

- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) — add `delegate_to: Option<DelegationSpec>` to `WorkflowStep`.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs) — propagate `delegate_to` to `WorkflowNode`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) — `execute_agent_step` honours `delegate_to`; new `execute_delegated_call` helper.
- [`crates/ork-integrations/src/tools.rs`](../../crates/ork-integrations/src/tools.rs) — add `agent_call` arm in [`CompositeToolExecutor`](../../crates/ork-integrations/src/tools.rs).
- [`crates/ork-agents/src/registry.rs`](../../crates/ork-agents/src/registry.rs) — `resolve(&AgentId) -> Option<Arc<dyn Agent>>` and `peer_tool_descriptions()` helpers.
- [`crates/ork-persistence/src/postgres/workflow_repo.rs`](../../crates/ork-persistence/src/postgres/workflow_repo.rs) — record `parent_run_id`, `parent_step_id`, `parent_task_id`.
- New SQL migration `migrations/003_delegation.sql` adding columns + the `a2a_tasks` parent linkage (joined with the migration in ADR [`0008`](0008-a2a-server-endpoints.md)).

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `PeerAgentTool` (LLM-facing per-peer tool) | [`agent/tools/peer_agent_tool.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/peer_agent_tool.py) | `agent_call` tool + per-peer expansion in registry |
| `workflow_tool` / `workflow/agent_caller.py` | [`src/solace_agent_mesh/workflow/agent_caller.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/workflow/agent_caller.py) | `delegate_to: DelegationSpec` step field |
| `get_agent_request_topic` per-agent topic | [`common/a2a/protocol.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/common/a2a/protocol.py) | `ork.a2a.v1.agent.request.<agent_id>` (ADR [`0004`](0004-hybrid-kong-kafka-transport.md)) |
| Delegation cancellation | SAM workflow cancel + peer status topics | `AgentContext.cancel` propagation + cancel topic |
| Inter-agent allow/deny lists | Orchestrator YAML | Enforced via RBAC scopes (ADR [`0021`](0021-rbac-scopes.md)) |

## Open questions

- Should `agent_call` results stream back through the parent agent's SSE channel automatically (so the end user sees nested progress), or only when `stream: true` is requested? Default: `stream: false` to keep tool-call semantics; opt-in surfaces nested status.
- Should `delegate_to` accept an array (one parent step → many parallel children)? Punted to ADR [`0018`](0018-dag-executor-enhancements.md) when fan-out becomes first-class.

## References

- [`future-a2a.md` §5](../../future-a2a.md)
- SAM `peer_agent_tool.py`: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/tools/peer_agent_tool.py>
- SAM `workflow/agent_caller.py`: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/workflow/agent_caller.py>
