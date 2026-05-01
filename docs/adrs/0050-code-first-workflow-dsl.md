# 0050 — Code-first Workflow DSL with typed steps and suspend/resume

- **Status:** Implemented
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0006, 0011, 0019, 0048, 0049, 0051, 0052, 0053, 0056
- **Supersedes:** 0018, 0026, 0027

## Context

Ork's workflow engine today lives in
[`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
and consumes YAML/TOML templates from
[`workflow-templates/`](../../workflow-templates/). The engine
implements DAG semantics, retries, agent calls and tool calls; the
authoring surface is YAML. Two problems with that:

1. **The YAML type system is parallel to Rust's.** Step inputs and
   outputs are `serde_json::Value`; the type-checker cannot catch a
   step that misnames a field or expects a missing one. ADR
   [`0025`](0025-typed-output-validation-and-verifier-agent.md)
   (Proposed, now superseded by [`0048`](0048-pivot-to-code-first-rig-platform.md))
   tried to bolt validation on top; the cleaner shape is to make the
   workflow itself typed.
2. **Control flow primitives are limited.** The current engine
   handles sequence + per-step retry. Branch/parallel/loop/foreach/
   suspend-resume are partially in ADR
   [`0018`](0018-dag-executor-enhancements.md) (Proposed, superseded)
   and in HITL-shaped form in
   [`0027`](0027-human-in-the-loop.md) (Proposed, superseded). The
   pivot ADR collapses those into a single shape.

Mastra's
[`createWorkflow` / `createStep`](https://mastra.ai/docs/workflows/overview)
ships exactly the shape we want: typed I/O via zod, builder methods
`.then` / `.branch` / `.parallel` / `.dountil` / `.dowhile` /
`.foreach` / `.map`, native `suspend()` / `resume()`, and snapshot
persistence so a paused workflow survives restart. The
[control-flow page](https://mastra.ai/docs/workflows/control-flow)
shows the verbatim primitives. The
[suspend/resume page](https://mastra.ai/docs/workflows/suspend-and-resume)
shows the persistence contract.

rig has a
[`Pipeline` / `Op`](https://docs.rs/rig-core/latest/rig/pipeline/index.html)
shape but it is in-memory only and does not persist; it is not the
right primitive for ork's persistent workflow model. We use rig
inside individual `agent` and `tool` steps (via 0051 / 0052) but not
as the workflow engine.

## Decision

ork **introduces a code-first typed Workflow DSL** layered on top of
the existing
[workflow engine](../../crates/ork-core/src/workflow/engine.rs).
The DSL is a builder over typed steps with `serde::Serialize`/
`Deserialize` + [`schemars`](https://docs.rs/schemars/) inputs and
outputs; the engine — including persistence to Postgres
([`crates/ork-persistence/`](../../crates/ork-persistence/)) — stays.
YAML templates remain as a *compatibility input* that desugars to
the same `WorkflowDef` value at load time, but the canonical
authoring surface is Rust code.

```rust
use ork_workflow::{workflow, step, Workflow, RunContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, JsonSchema)] struct WeatherIn  { city: String }
#[derive(Serialize,   JsonSchema)] struct WeatherOut { f: f32, summary: String }

let fetch = step("fetch_weather")
    .input::<WeatherIn>()
    .output::<WeatherOut>()
    .execute(|ctx, WeatherIn { city }| async move {
        let raw = ctx.tools().call("weather.lookup",
            serde_json::json!({ "city": city })).await?;
        Ok(WeatherOut {
            f: raw["temp_f"].as_f64().unwrap() as f32,
            summary: raw["summary"].as_str().unwrap().to_string(),
        })
    });

let summarise = step("summarise")
    .input::<WeatherOut>()
    .output::<String>()
    .execute(|ctx, w| async move {
        ctx.agents().run("forecaster",
            format!("Forecast: {}, {} F", w.summary, w.f)).await
    });

pub fn weather_workflow() -> Workflow {
    workflow("weather")
        .input::<WeatherIn>()
        .output::<String>()
        .then(fetch)
        .then(summarise)
        .commit()
}
```

The user registers it via [`OrkApp::builder().workflow(...)`](0049-orkapp-central-registry.md)
and runs it via `app.run_workflow("weather", ctx, input).await` or by
hitting `/api/workflows/weather/run` on the auto-generated server
(ADR 0056).

### Workflow surface

```rust
// crates/ork-workflow/src/lib.rs
pub fn workflow(id: impl Into<String>) -> WorkflowBuilder<(), ()>;

pub struct WorkflowBuilder<I, O> { /* typestate */ }

impl<I, O> WorkflowBuilder<I, O>
where
    I: Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + Sync + 'static,
{
    pub fn input<X: JsonSchema + DeserializeOwned + Send + Sync + 'static>(self)
        -> WorkflowBuilder<X, O>;
    pub fn output<X: JsonSchema + Serialize + Send + Sync + 'static>(self)
        -> WorkflowBuilder<I, X>;

    pub fn description(self, s: impl Into<String>) -> Self;
    pub fn retry(self, policy: RetryPolicy) -> Self;
    pub fn timeout(self, d: Duration) -> Self;

    pub fn then<S, F, Sin, Sout>(self, step: S) -> WorkflowBuilder<I, Sout>
    where S: Into<Step<Sin, Sout>>, /* compatibility check via type-state */;

    pub fn branch(self, arms: Vec<(BranchPredicate, AnyStep)>) -> Self;
    pub fn parallel(self, steps: Vec<AnyStep>) -> Self;
    pub fn dountil<S>(self, step: S, until: Predicate) -> Self;
    pub fn dowhile<S>(self, step: S, while_: Predicate) -> Self;
    pub fn foreach<S>(self, step: S, opts: ForEachOptions) -> Self;
    pub fn map<F, X>(self, f: F) -> WorkflowBuilder<I, X>
    where F: Fn(O) -> X + Send + Sync + 'static;

    pub fn commit(self) -> Workflow;
}

pub struct Workflow { /* opaque, registered with OrkApp */ }

impl WorkflowDef for Workflow { /* implements port from ADR 0049 */ }
```

### Step surface

```rust
pub fn step(id: impl Into<String>) -> StepBuilder<(), ()>;

pub struct StepBuilder<I, O> { /* typestate */ }

impl<I, O> StepBuilder<I, O> {
    pub fn input<X: JsonSchema + DeserializeOwned + Send + Sync + 'static>(self)
        -> StepBuilder<X, O>;
    pub fn output<X: JsonSchema + Serialize + Send + Sync + 'static>(self)
        -> StepBuilder<I, X>;
    pub fn description(self, s: impl Into<String>) -> Self;
    pub fn retry(self, policy: RetryPolicy) -> Self;
    pub fn timeout(self, d: Duration) -> Self;

    pub fn execute<F, Fut>(self, f: F) -> Step<I, O>
    where
        F: Fn(StepContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<StepOutcome<O>, OrkError>> + Send + 'static;
}

pub enum StepOutcome<O> {
    /// Step completed; `O` is the output value.
    Done(O),
    /// Step is paused awaiting external resume. The `payload` is
    /// stored in the snapshot and surfaced to clients (Studio / REST)
    /// so they can show "what is this workflow waiting for".
    Suspend { payload: serde_json::Value, resume_schema: Schema },
}

pub struct StepContext {
    /// Tenant, cancel token, caller — same shape as `AgentContext`.
    pub agent_context: AgentContext,
    pub tools: ToolHandle,    // step-scoped tool catalog
    pub agents: AgentHandle,  // call other registered agents (ADR 0006 piggyback)
    pub memory: MemoryHandle, // optional, see ADR 0053
    pub run: RunInfo,         // run id, attempt, parent run id
}
```

### Suspend / resume

A step returns `StepOutcome::Suspend { payload, resume_schema }`.
The engine writes a snapshot row keyed by `(workflow_id, run_id,
step_id, attempt)` to the
[`crates/ork-persistence/`](../../crates/ork-persistence/) Postgres
backend. The surface for resuming is on the run handle:

```rust
let run = app.run_workflow("weather", ctx, input).await?;
match run.poll().await? {
    RunState::Running => /* keep streaming */,
    RunState::Suspended { step_id, payload, resume_schema } => {
        // Show this to a human (Studio / REST), gather data:
        let resume_data = serde_json::json!({ "approved": true });
        run.resume(step_id, resume_data).await?;
    }
    RunState::Completed { output } => /* done */,
    RunState::Failed { error } => /* error */,
}
```

The resume contract:

- The engine validates `resume_data` against the `resume_schema`
  the step declared when it suspended. Mismatch ⇒
  `OrkError::Validation`.
- Resume is at-least-once. Steps must be idempotent against the
  same resume payload, or the step must store its own dedup token.
  Documented in the step builder's `description`-area docs.
- Snapshots survive process restart. `app.run_workflow` resumes
  in-progress runs on startup if `ServerConfig::resume_on_startup`
  is set (ADR 0049's `ServerConfig`).

This replaces ADR [`0027`](0027-human-in-the-loop.md)'s separate
HITL surface with a single primitive.

### Streaming

A run handle exposes both a polled `state()` and an event stream:

```rust
impl WorkflowRunHandle {
    pub fn id(&self) -> RunId;
    pub async fn poll(&self) -> Result<RunState, OrkError>;
    pub fn events(&self) -> impl Stream<Item = WorkflowEvent>;
    pub async fn resume(&self, step_id: StepId, data: serde_json::Value)
        -> Result<(), OrkError>;
    pub async fn cancel(&self) -> Result<(), OrkError>;
    pub async fn await_done(self) -> Result<RunState, OrkError>;
}

pub enum WorkflowEvent {
    StepStarted   { step_id: StepId, input: serde_json::Value },
    StepFinished  { step_id: StepId, output: serde_json::Value },
    StepSuspended { step_id: StepId, payload: serde_json::Value },
    StepFailed    { step_id: StepId, error: String, retryable: bool },
    StepRetrying  { step_id: StepId, attempt: u32, after: Duration },
    Heartbeat,
}
```

`events()` feeds the SSE stream that ADR 0056 mounts at
`/api/workflows/:id/run/:run_id/stream`, the Studio panel that ADR
0055 paints, and the OTel spans that ADR 0058 emits. One source of
truth.

### Triggers (cron, webhook)

ADR [`0019`](0019-scheduled-tasks.md)'s scheduled-task surface lands
as a builder method:

```rust
let nightly = workflow("nightly_summary")
    .trigger(Trigger::cron("0 0 * * *", "UTC"))
    .input::<()>().output::<Summary>()
    .then(...)
    .commit();
```

The `Trigger` primitive is the place ADR 0019's open questions
resolve (timezone, retry on missed window, dedupe-by-fired-at).
0050 owns the surface; the triggering loop lives in a
`SchedulerService` in `ork-app::serve()` per ADR 0019.

## Acceptance criteria

- [ ] New crate `crates/ork-workflow/` with `Cargo.toml` declaring
      `ork-core`, `ork-common`, `serde`, `schemars`, `tokio`,
      `futures`. No infra deps (per AGENTS.md §3 hexagonal rule).
- [ ] `workflow(id) -> WorkflowBuilder<(), ()>` and
      `step(id) -> StepBuilder<(), ()>` exported from
      `crates/ork-workflow/src/lib.rs` with the signatures shown in
      `Decision`.
- [ ] `WorkflowBuilder` enforces typestate: `.then(step)` fails to
      compile if the previous step's output type does not match the
      next step's input type. Demonstrated by a `compile_fail` doc
      test in `crates/ork-workflow/src/lib.rs`.
- [ ] Control-flow methods implemented: `branch`, `parallel`,
      `dountil`, `dowhile`, `foreach`, `map`. Each has an
      integration test under
      `crates/ork-workflow/tests/control_flow.rs` covering: input
      typing, happy path, error path, cancellation responsiveness.
- [ ] `StepOutcome::Suspend` round-trips through Postgres snapshot
      storage. Test
      `crates/ork-workflow/tests/suspend_resume.rs::round_trip`
      builds a workflow with one suspending step, runs it,
      restarts a fresh `OrkApp`, calls `run.resume(...)`, and
      asserts completion.
- [ ] Snapshot schema migration added to
      [`migrations/`](../../migrations/) under
      `NNNN_workflow_snapshots.sql`, with `(workflow_id, run_id,
      step_id, attempt) UNIQUE`, `payload JSONB`,
      `resume_schema JSONB`, `created_at`, `consumed_at`.
- [ ] `WorkflowRunHandle::events()` yields `WorkflowEvent` per the
      enum in `Decision`; the SSE encoder in
      [`crates/ork-api/`](../../crates/ork-api/) consumes it and
      emits SSE per ADR
      [`0003`](0003-a2a-protocol-model.md)'s typed-parts shape.
- [ ] Trigger surface implemented for `Trigger::cron(...)`. Test
      `crates/ork-workflow/tests/trigger_cron.rs` registers a
      workflow with a 1-second cron expression, advances time via a
      mock clock, asserts the run fires within 2 ticks.
- [ ] YAML compatibility shim implemented:
      [`workflow-templates/`](../../workflow-templates/) files load
      via a `Workflow::from_template_path` function that desugars
      to the same `Workflow` value. Existing demo templates still
      run, asserted by a regression test under
      `crates/ork-workflow/tests/yaml_compat.rs`.
- [ ] `OrkAppBuilder::workflow(w)` accepts the new `Workflow`
      type via the `WorkflowDef` port from ADR 0049.
- [ ] Per-step `RetryPolicy { max_attempts, backoff:
      ExponentialBackoff { initial, multiplier, jitter, max } }`
      threaded through; failed step retries documented in the
      event stream as `StepRetrying`.
- [ ] CI grep: no file under `crates/ork-workflow/` imports
      `axum`, `sqlx`, `reqwest`, `rmcp`, or `rskafka`.
- [ ] [`README.md`](README.md) ADR index row added and ADR 0018,
      0026, 0027 status flipped to `Superseded by 0050`.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- The Rust compiler catches workflow refactors. Renaming a field on
  `WeatherOut` breaks `.then(summarise)` at `cargo check` time, not
  at run time after a 30-second LLM call.
- One control-flow surface for branches, parallel, loops, foreach
  collapses ADR 0018's deferred work and ADR 0026's classifier
  shape into the workflow author's hands. The classifier becomes
  *just an agent that returns a structured output* feeding a
  `branch`.
- Suspend/resume as a primitive (not a separate "HITL" surface)
  lets every step pause for any reason: human approval, awaiting a
  webhook, waiting for a long async tool. ADR 0027's surface
  collapses into this one.
- The WorkflowDef port shipped in 0049 means the auto-generated
  REST surface (0056) and Studio (0055) get workflow introspection
  for free.
- YAML stays loadable. Existing demos do not regress; a team can
  migrate workflow-by-workflow.

### Negative / costs

- The typestate `WorkflowBuilder<I, O>` is ergonomic for the linear
  case (`then` chains) and ugly for branches that produce
  heterogeneous outputs. We document the escape hatch
  (`AnyStep`-via-`map` flatten pattern) and accept the
  intermediate boilerplate.
- Schemars adds a build dep that ripples into the user's project.
  Mitigation: it is already pulled in by ADR 0047's rig adoption,
  so the cost is carried.
- Snapshots in Postgres mean every workflow that suspends becomes
  a row write per pause. For high-fanout workflows this is fine;
  for ten-thousand-RPS hot-path workflows it is not. We do not
  target that load profile in v1; documented under `Open
  questions`.
- Per-step compile-time wiring means workflow definitions live in
  the binary. Hot-reload (ADR 0057's `ork dev`) recompiles the
  workspace; no runtime YAML edit. The YAML compat shim covers the
  case where a user wants the runtime-edit shape.

### Neutral / follow-ups

- `Pipeline`/`Op` from rig is intentionally not adopted as the
  workflow engine. The two coexist: rig's pipeline is in-memory
  for inside-an-agent composition (e.g., chained extractors); ork's
  workflow is the persistent, A2A-callable, observable surface.
- The branch arm signature `(BranchPredicate, AnyStep)` may move
  to a typestate per arm in a follow-up. Today the unification
  through `AnyStep` keeps the v1 surface tractable.
- A `workflow! {}` declarative macro could be added later to tighten
  the syntax (à la `axum::Router::new().route(...)` macros). Out
  of scope.
- ADR 0019's surface ships *as part of this ADR's `Trigger`
  primitive*; the standalone 0019 stays Proposed but its body is
  effectively absorbed. We do not flip 0019 to Superseded; it
  documents the storage-of-schedules concerns 0050 inherits.

## Alternatives considered

- **Keep YAML as the canonical authoring surface, just add typing
  on top.** Rejected. ADR 0025 was the attempt; the seam it
  produced (parse YAML → coerce JSON Value → validate against
  schema → coerce to Rust type) is the source of the bugs. Rust
  code with `#[derive(Deserialize, JsonSchema)]` is the same
  contract one layer up.
- **Adopt rig's `Pipeline` as the workflow engine.** Rejected.
  rig pipelines are in-memory; they do not persist, do not
  resume, do not survive restart. The shape is fine for
  *inside-an-agent* composition; the workflow surface needs the
  Postgres-backed snapshot story.
- **Use a state-machine derive crate (e.g., `statig`,
  `state_machine_future`).** Rejected. Workflows are not finite
  state machines in the traditional sense; the steps are async
  computations whose set is open-ended (`foreach`,
  `dountil`-with-runtime-data). The builder shape Mastra
  validates is the right level.
- **Adopt `temporal-rs`-style "deterministic replay".** Rejected
  for now. Deterministic replay is the right answer for
  fault-tolerant long-running workflows but imposes a strict
  shape on steps (no IO-without-recording). Mastra does not go
  that far; we match Mastra and revisit if a customer needs
  Temporal-grade durability. Open question.
- **One generic step type with `Box<dyn Any>` IO.** Rejected. We
  did this in v0 (the engine is `serde_json::Value`-shaped today)
  and the typestate version is the upgrade we are paying for.

## Affected ork modules

- New: [`crates/ork-workflow/`](../../crates/) — DSL crate; depends
  on `ork-core`, `ork-common`.
- [`crates/ork-core/src/workflow/`](../../crates/ork-core/src/workflow/)
  — engine internals stay; runtime accepts the new `Workflow`
  shape via the `WorkflowDef` port from ADR 0049.
- [`crates/ork-persistence/`](../../crates/ork-persistence/) —
  workflow run + snapshot tables.
- [`crates/ork-api/`](../../crates/ork-api/) — SSE encoder for
  `WorkflowEvent` (ADR 0056 fleshes this out).
- [`migrations/`](../../migrations/) — snapshot table migration.
- [`workflow-templates/`](../../workflow-templates/) — consumed by
  the YAML compat shim (no schema changes).

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Critical | Process-level `resume_on_startup` must not replay snapshots with a synthetic `AgentContext` + `resume: null` (breaks faithful HITL resume). | **Fixed in-session** by logging pending rows only; full replay deferred to follow-up (wire explicit resume or rehydrate suspended handles). |
| Major | `WorkflowRunHandle` uses `subscribe_events()` vs ADR’s `events()` naming; no HTTP route mounts workflow SSE yet. | **Deferred** to ADR 0056 (encoder shipped in `ork-api::sse::workflow`). |
| Major | `Trigger::cron(expr, tz)` previously dropped `tz`. | **Mitigated:** warn when `tz` is not `UTC`; evaluation remains UTC until timezone support lands. |
| Minor | YAML compat drops legacy template trigger metadata (`trigger: None`). | **Recorded:** shim is execution-focused; cron from YAML templates not preserved on `Workflow`. |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Workflows overview](https://mastra.ai/docs/workflows/overview) | `workflow()` + `step()` builder |
| Mastra | [Control flow](https://mastra.ai/docs/workflows/control-flow) | `then` / `branch` / `parallel` / `dountil` / `dowhile` / `foreach` / `map` |
| Mastra | [Suspend/Resume](https://mastra.ai/docs/workflows/suspend-and-resume) | `StepOutcome::Suspend { payload, resume_schema }` + Postgres snapshots |
| rig | [`Pipeline`/`Op`](https://docs.rs/rig-core/latest/rig/pipeline/index.html) | informative only — used inside agents, not as the engine |
| LangGraph | `StateGraph` typed nodes | typed-IO is the same idea; ork does not encode the graph as state nodes |
| Temporal | deterministic replay workflows | not adopted; see `Alternatives` and `Open questions` |

## Open questions

- **Branch heterogeneous outputs.** Today `branch` requires arms to
  return the same type. Mastra's branch keys the union by step id.
  We can either (a) lift the constraint and return a tagged enum
  (compile-time matching) or (b) require uniform output and use
  `map` to unify. Default: (b); revisit if we hit a real workflow
  that wants a tagged enum.
- **Long-running workflows and snapshot bloat.** A workflow that
  loops 10k times accumulates 10k events. Need an event-table
  retention policy and a "compact run" job. Spike during
  implementation.
- **Workflow versioning.** Once a workflow is registered, what
  happens when the user changes the code and re-deploys while a
  paused run exists? Mastra surfaces this as "version mismatch"
  and lets the user decide. We default to "fail the resume with
  `OrkError::Validation`"; better behaviour deferred.
- **Determinism.** Steps may call non-deterministic services. We
  do not promise replayability v1; revisit if a customer needs
  Temporal-grade durability.
- **`AnyStep` ergonomics.** The unifying step type may need a
  declarative macro. Tracking, not blocking.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — `OrkApp`.
- ADR [`0019`](0019-scheduled-tasks.md) — triggers; absorbed.
- ADR [`0011`](0011-native-llm-tool-calling.md),
  [`0006`](0006-peer-delegation.md) — the agent_call/tool semantics
  inside a step.
- Mastra workflows: <https://mastra.ai/docs/workflows/overview>
- Mastra control flow: <https://mastra.ai/docs/workflows/control-flow>
- Mastra suspend/resume:
  <https://mastra.ai/docs/workflows/suspend-and-resume>
- rig pipeline: <https://docs.rs/rig-core/latest/rig/pipeline/index.html>
