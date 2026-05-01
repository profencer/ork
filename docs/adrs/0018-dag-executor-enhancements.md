# 0018 — Workflow DAG executor enhancements

- **Status:** Superseded by 0050
- **Date:** 2026-04-24
- **Phase:** 4
- **Relates to:** 0002, 0006, 0011, 0015, 0019, 0022

## Context

The ork engine in [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) and the compiler in [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs) currently support:

- Sequential and conditional steps via `depends_on` + `condition: { on_pass, on_fail }`.
- Per-step iteration via `for_each` (a string template that resolves to a JSON array).

Missing capabilities that block real-world workflows and SAM parity:

- **No parallel fan-out / fan-in** — `depends_on: [a, b]` works, but `a` and `b` themselves run sequentially regardless of dependencies. There is no way to express "run these three sub-workflows concurrently and continue when all finish".
- **No `switch`** — only the binary `on_pass / on_fail` exists; multi-branch routing is impossible.
- **No `map`** — `for_each` only iterates serially; there is no parallel map with concurrency control.
- **No structured loop step** — recursion is via `condition` jumps; there is no explicit `while` / `until` step kind with a max-iterations cap.
- **Engine still calls `LlmProvider` directly** — ADR [`0002`](0002-agent-port.md) refactors this, but the engine also doesn't support cancellation propagation through nested steps.

SAM equivalents live across [`workflow/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/workflow) (DAG executor, agent caller) and the Orchestrator agent's tool set.

## Decision

ork **enhances the workflow engine and compiler** to support parallel composition, structured branching, parallel iteration, and explicit loops, while **decoupling agent invocation from the engine** (per ADR [`0002`](0002-agent-port.md)).

### New step kinds

[`WorkflowStep`](../../crates/ork-core/src/models/workflow.rs) becomes a tagged union of step kinds while preserving the existing flat shape as the `agent` kind for backwards compatibility:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowStep {
    /// Existing default kind — when YAML omits `kind:` we infer this.
    Agent {
        id: String,
        agent: AgentRef,
        tools: Vec<String>,
        prompt_template: String,
        #[serde(default)]
        depends_on: Vec<String>,
        condition: Option<StepCondition>,
        for_each: Option<String>,
        iteration_var: Option<String>,
        delegate_to: Option<DelegationSpec>,    // from ADR 0006
        model: Option<String>,                  // from ADR 0012
    },

    /// NEW: Parallel fan-out / fan-in container.
    Parallel {
        id: String,
        branches: Vec<Vec<WorkflowStep>>,       // each branch is its own sub-graph
        max_concurrency: Option<u32>,           // None = unbounded; capped at engine limit
        join: JoinPolicy,                       // AllSucceed | AnyFails | Quorum(N)
        depends_on: Vec<String>,
    },

    /// NEW: Multi-branch routing on a string-valued template.
    Switch {
        id: String,
        on: String,                             // template that resolves to a string
        cases: BTreeMap<String, Vec<WorkflowStep>>,   // case name -> sub-steps
        default: Option<Vec<WorkflowStep>>,
        depends_on: Vec<String>,
    },

    /// NEW: Parallel map over a collection.
    Map {
        id: String,
        items: String,                          // template -> JSON array
        item_var: String,                       // default "item"
        body: Vec<WorkflowStep>,
        max_concurrency: Option<u32>,
        on_item_failure: ItemFailurePolicy,     // FailFast | Continue | RetryWithBackoff(...)
        depends_on: Vec<String>,
    },

    /// NEW: Bounded loop with break condition.
    Loop {
        id: String,
        body: Vec<WorkflowStep>,
        until: String,                          // template; loop exits when truthy
        max_iterations: u32,                    // hard cap
        depends_on: Vec<String>,
    },
}
```

Backwards compatibility: existing YAML without `kind:` parses as `Agent` (custom serde shim). All [`workflow-templates/`](../../workflow-templates/) examples keep working.

`AgentRef` was introduced in ADR [`0007`](0007-remote-a2a-agent-client.md) (string id or inline-card).

### Compiler changes

[`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs) is generalised to lower the rich step tree into an extended `CompiledWorkflow` whose nodes know their sub-graphs:

```rust
pub struct CompiledWorkflow {
    pub name: String,
    pub root: Vec<NodeId>,                      // entry nodes
    pub nodes: HashMap<NodeId, CompiledNode>,   // graph
}

pub enum CompiledNode {
    Agent { ... },
    Parallel { branches: Vec<NodeId>, max_concurrency: Option<u32>, join: JoinPolicy },
    Switch { on: String, cases: BTreeMap<String, NodeId>, default: Option<NodeId> },
    Map { items: String, item_var: String, body: NodeId, max_concurrency: Option<u32>, on_item_failure: ItemFailurePolicy },
    Loop { body: NodeId, until: String, max_iterations: u32 },
}
```

Each container node references **child node ids**, not inlined steps, to keep the graph traversable for visualisation (Web UI workflow viewer in ADR [`0017`](0017-webui-chat-client.md)) and for a future visual editor.

### Engine changes

The engine ([`WorkflowEngine`](../../crates/ork-core/src/workflow/engine.rs)) is rewritten around an explicit recursive walker:

```rust
async fn run_node(&self, ctx: &RunContext, node: &CompiledNode) -> Result<NodeOutput, OrkError> {
    match node {
        CompiledNode::Agent(a) => self.run_agent(ctx, a).await,
        CompiledNode::Parallel { branches, max_concurrency, join } =>
            self.run_parallel(ctx, branches, *max_concurrency, *join).await,
        CompiledNode::Switch { on, cases, default } =>
            self.run_switch(ctx, on, cases, default).await,
        CompiledNode::Map { items, item_var, body, max_concurrency, on_item_failure } =>
            self.run_map(ctx, items, item_var, *body, *max_concurrency, *on_item_failure).await,
        CompiledNode::Loop { body, until, max_iterations } =>
            self.run_loop(ctx, *body, until, *max_iterations).await,
    }
}
```

`run_parallel` uses `tokio::task::JoinSet` with a `Semaphore` for `max_concurrency`. `run_map` is the same but parameterised by the resolved item list. `run_loop` evaluates the `until` template after each iteration; exceeding `max_iterations` is a step failure with reason `loop_exceeded`.

Cancellation: the `RunContext` carries a `CancellationToken`; nested executions clone the token. A2A `tasks/cancel` (ADR [`0008`](0008-a2a-server-endpoints.md)) cancels the root token, which cascades to every running child via `tokio::select!`.

### Agent invocation decoupled

`run_agent` calls `Agent::send_stream` (ADR [`0002`](0002-agent-port.md)) — no direct `LlmProvider` use. Tool execution moves into `LocalAgent` (ADR [`0011`](0011-native-llm-tool-calling.md)). The engine's role narrows to graph traversal, template resolution, persistence of intermediate results, and event publishing.

### Persistence per node

[`StepResult`](../../crates/ork-core/src/models/workflow.rs) is extended with an optional `children: Vec<StepResult>` to record per-iteration / per-branch sub-results. This is purely additive (existing rows have `children: []`).

### Limits

| Setting | Default | Purpose |
| ------- | ------- | ------- |
| `engine.max_total_concurrency` | 32 | Global per-process cap on parallel agent calls |
| `engine.max_loop_iterations` | 64 | Default cap for `Loop` steps without explicit `max_iterations` |
| `engine.max_map_concurrency` | 8 | Default cap for `Map` steps |
| `engine.max_step_depth` | 16 | Container nesting depth |

All configurable in [`config/default.toml`](../../config/default.toml).

### Telemetry

Each node emits `tracing` spans with attributes `node.id`, `node.kind`, `iteration` (for `Map`/`Loop`), `branch_idx` (for `Parallel`). ADR [`0022`](0022-observability.md) wires these into OpenTelemetry exporters.

## Consequences

### Positive

- Real workflow shapes (parallel data gathering, switch routing on intent classification, parallel map over a list of repos) become first-class.
- The engine becomes simpler in some ways — agent execution is now `Agent::send_stream`, not a hardcoded LLM call.
- Cancellation works end-to-end, finally satisfying A2A `tasks/cancel` semantics.
- Workflow visualisation (Web UI, future editor) is straightforward because containers reference child node ids.

### Negative / costs

- The compiler and engine grow meaningfully. Coverage by integration tests must grow with them; we add a YAML golden-test suite under `crates/ork-core/tests/workflow_kinds/`.
- Concurrency semantics surface user-visible knobs (`max_concurrency`, `on_item_failure`) that need clear docs.
- `WorkflowStep` becoming an enum is a breaking change to the persisted YAML schema **only** for new step kinds; existing workflows keep working.

### Neutral / follow-ups

- A future ADR may add `subworkflow` as an explicit kind that runs another `WorkflowDefinition`; currently this is achievable with `delegate_to.child_workflow` from ADR [`0006`](0006-peer-delegation.md).
- A future ADR may add streaming intermediate outputs (per-node SSE events) to the Web UI viewer.
- ADR [`0019`](0019-scheduled-tasks.md) consumes these step kinds for scheduled workflows.

## Alternatives considered

- **Adopt a third-party DAG library (e.g. `petgraph` algorithms wrapped in a runtime).** `petgraph` itself is fine for the data structure; the runtime is bespoke either way. Decision: keep the engine in-tree; depend on `petgraph` for cycle detection in the compiler.
- **Externalise to Temporal / Conductor.** Rejected: huge external dependency; ork's runtime is already the workflow engine, replacing it loses the agent integration.
- **Make every container an "agent that runs sub-steps".** Rejected: blurs the agent/orchestration distinction and makes A2A semantics weird (a Switch is not a thing you can `tasks/cancel` independently).
- **Stay with sequential + condition only; rely on agents to fan out via `agent_call`.** Rejected: pushes orchestration into prompts, which is unreliable and unobservable.

## Affected ork modules

- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) — `WorkflowStep` enum, `JoinPolicy`, `ItemFailurePolicy`, `StepResult.children`.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs) — recursive lowering, cycle detection (uses `petgraph`).
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) — node walker, concurrency primitives, cancellation propagation.
- [`crates/ork-persistence/src/postgres/workflow_repo.rs`](../../crates/ork-persistence/src/postgres/workflow_repo.rs) — write/read children rows.
- [`workflow-templates/`](../../workflow-templates/) — add example YAMLs for `parallel`, `switch`, `map`, `loop`.
- [`config/default.toml`](../../config/default.toml) — `[engine]` limits.

## Mapping to SAM

| SAM concept | Where in SAM | ork equivalent in this ADR |
| ----------- | ------------ | -------------------------- |
| `WorkflowExecutorComponent` | [`workflow/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/workflow) | `WorkflowEngine` (this ADR) |
| Parallel sub-flow | SAM workflow YAML | `Parallel` step kind |
| Switch / case | SAM Orchestrator pattern | `Switch` step kind |
| Map over list | SAM workflow YAML | `Map` step kind |
| Loop with break | implicit | `Loop` step kind |
| Cancellation propagation | SAM SAC component | `RunContext.cancel` token |

## Open questions

- Should `Parallel` / `Map` errors stream to the SSE client per-branch as they happen, or only at the join? Decision: per-branch via `AgentEvent::status_update` with a `branch_id` annotation; the join produces the final status.
- Do we want `Race` semantics (return as soon as one branch succeeds)? Defer; can be expressed via `JoinPolicy::Quorum(1)` for now, with `cancel-others` flag added later.
- Should the engine support **suspending** a workflow (write-state-and-stop, resume on demand)? Required for `TaskState::InputRequired` (ADR [`0003`](0003-a2a-protocol-model.md)). Yes — defer the implementation detail to a follow-up ADR but reserve the state machine.

## References

- `tokio::task::JoinSet`: <https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html>
- `petgraph` (cycle detection): <https://crates.io/crates/petgraph>
- SAM workflow agent caller: <https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/workflow/agent_caller.py>
