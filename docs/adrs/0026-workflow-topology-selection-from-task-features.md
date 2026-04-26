# 0026 — Workflow topology selection from task features

- **Status:** Proposed
- **Date:** 2026-04-27
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0002, 0006, 0018, 0022, 0025
- **Supersedes:** —

## Context

Workflow authors today choose a topology by hand. Looking at
[`workflow-templates/`](../../workflow-templates/), every shipped
template is a strict sequential pipeline (`change-plan.yaml`,
`release-notes.yaml`, `standup-brief.yaml`, …). The engine in
[`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
and the compiler in
[`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
support sequential `depends_on`, conditional branching and serial
`for_each`. ADR [`0018`](0018-dag-executor-enhancements.md) adds
`Parallel { branches, join }`, `Switch`, parallel `Map`, and explicit
loops; ADR [`0006`](0006-peer-delegation.md) gives every step a
`delegate_to` peer hop. Once 0018 lands the author's design space is
large enough that *picking the wrong shape* becomes the dominant
failure mode.

Google's *Towards a Science of Scaling Agent Systems* (April 2025) —
already cited from ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md)
— quantifies that failure mode:

- A predictive model over **task decomposability** and **tool count
  per step** picks the optimal topology in **87 %** of unseen
  configurations.
- For tasks with strict sequential dependencies, multi-agent variants
  *degrade* end-to-end accuracy by **39 – 70 %** versus a single
  strong agent.
- For decomposable / parallelizable tasks, a **centralized
  orchestrator** beats a single agent by **+80.9 %**.
- An **independent** (no-orchestrator) ensemble amplifies single-step
  error by **17.2×** end-to-end, versus **4.4×** for a centralized
  topology — i.e. coordination matters more than parallelism.

The five topologies the paper studies map cleanly onto shapes ork
already exposes (or unlocks under 0018):

| Paper | ork realisation |
| ----- | --------------- |
| Single-agent (SAS) | one `Agent` step, no `depends_on` chain |
| Sequential pipeline | linear `depends_on` chain (today's default) |
| Independent ensemble | `Parallel { branches, join: AllSucceed }` with no synthesis step |
| Centralized orchestrator | `Parallel { … }` followed by a synthesis `Agent` step, **or** a parent `Agent` whose `delegate_to` fans out to peers (ADR 0006) |
| Decentralized / hybrid | Peer agents delegating to peers (ADR 0006), with or without a top-level orchestrator |

ork does not need to *prevent* an author from picking the wrong shape,
but it can refuse to be silent about it. Today nothing in the engine,
the YAML compiler, or the CLI is aware of "topology fit" as a concept
at all.

## Decision

ork **introduces a deterministic topology classifier** in `ork-core`
that takes a `TaskFeatures` description and returns a ranked list of
`TopologyRecommendation`s with a confidence and a rationale. The
classifier is exposed (a) as a CLI subcommand and (b) as a workflow
lint that compares the authored shape against the recommendation. It
is **never** a runtime gate — authors keep authority.

### Domain types (in `ork-core`)

```rust
// crates/ork-core/src/topology/mod.rs

/// Feature vector describing a single task. Hand-authored or produced
/// by a `TaskFeatureExtractor` port (see below). Numeric fields use
/// small bounded ranges so the classifier stays purely deterministic
/// and trivially testable.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskFeatures {
    /// True when sub-steps must observe each other's outputs in order
    /// (e.g. "fetch then summarise then sign"). Drives away from
    /// parallel topologies.
    pub sequential_dependencies: bool,
    /// True when the task can be split into independent sub-tasks
    /// whose outputs are recombined (e.g. per-repo analysis).
    pub decomposable: bool,
    /// Total number of distinct tools the task as a whole needs.
    /// The paper finds coordination overhead spikes at >= 16; ork
    /// uses the same threshold.
    pub tool_count: u32,
    /// True when a deterministic checker (schema, test, retrieval
    /// match) can verify the final output. Increases the value of
    /// adding a verifier (ADR 0025).
    pub verifiable: bool,
    /// Optional author hint: the longest plausible serial chain
    /// length, in steps. Defaults to 1 if unknown.
    #[serde(default = "one")]
    pub estimated_depth: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    SingleAgent,
    SequentialPipeline,
    IndependentEnsemble,
    CentralizedOrchestrator,
    HybridDelegation,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TopologyRecommendation {
    pub topology: Topology,
    /// 0.0 – 1.0; calibrated against the paper's reported per-class
    /// accuracy, not a learned probability.
    pub confidence: f32,
    /// Human-readable explanation referencing the features that drove
    /// the choice. Surfaced in CLI output and lint warnings.
    pub rationale: String,
}

pub trait TopologyClassifier: Send + Sync {
    fn classify(&self, features: &TaskFeatures) -> Vec<TopologyRecommendation>;
}
```

The default implementation is **`HeuristicClassifier`**, a small
deterministic decision tree directly transcribing the paper's findings
(spelled out in `Acceptance criteria`). No LLM, no training data, no
network call — the classifier is a pure function so the engine and
the lint can both call it cheaply.

### Feature extraction port

Authors will not always hand-author a `TaskFeatures`. ork defines a
port so an LLM-backed extractor can be plugged in without dragging a
provider dep into `ork-core`:

```rust
// crates/ork-core/src/ports/task_feature_extractor.rs
#[async_trait::async_trait]
pub trait TaskFeatureExtractor: Send + Sync {
    async fn extract(&self, task_description: &str)
        -> Result<TaskFeatures, ork_common::error::OrkError>;
}
```

The default binding in `ork-cli` / `ork-api` wires a thin LLM-backed
adapter that calls the active provider (ADR
[`0012`](0012-multi-llm-providers.md)) with a fixed prompt; tests use
a `StaticFeatures` fixture. Authors who hand-author features bypass
the port entirely.

### Author-facing surfaces

1. **CLI: `ork workflow recommend-topology`**
   - `--task "<free-text description>"` → uses the
     `TaskFeatureExtractor` port, then `classify`.
   - `--features path/to/features.yaml` → bypasses the extractor.
   - Prints the ranked list with rationale; exits 0 always (advisory).

2. **CLI lint: `ork workflow lint <file.yaml>`**
   - Existing lint gains a `topology` check.
   - Loads the workflow, derives a *structural topology* from the
     compiled `CompiledWorkflow` graph
     ([`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)),
     extracts features (or reads a sibling
     `<workflow>.features.yaml`), calls `classify`, and emits a
     `warning` when the authored shape is not in the top-2
     recommendations. Never an error.

3. **Workflow YAML hint block (optional, additive)** under the
   workflow's top-level `features:` key. When present the lint uses it
   verbatim and does not call the extractor:

   ```yaml
   name: change-plan
   version: "1.0"
   features:
     sequential_dependencies: true
     decomposable: true
     tool_count: 6
     verifiable: false
     estimated_depth: 3
   ```

   The block is opaque to the engine — it changes no runtime
   behaviour. Older workflows without it keep working.

### Heuristic decision rules

The default `HeuristicClassifier` ranks topologies as follows. Rules
fire top-down; the first match sets the top recommendation, the rest
populate fallbacks.

| Rule | Top topology | Confidence |
| ---- | ------------ | ---------- |
| `sequential_dependencies && estimated_depth <= 2 && tool_count <= 4` | `SingleAgent` | 0.85 |
| `sequential_dependencies && (estimated_depth > 2 \|\| tool_count > 4)` | `SequentialPipeline` | 0.80 |
| `decomposable && tool_count >= 16` | `CentralizedOrchestrator` | 0.85 |
| `decomposable && tool_count < 16 && verifiable` | `CentralizedOrchestrator` | 0.75 |
| `decomposable && tool_count < 16 && !verifiable` | `HybridDelegation` | 0.65 |
| otherwise | `SingleAgent` | 0.55 |

`IndependentEnsemble` is **never** the top recommendation. The paper's
17.2× error-amplification finding makes it a near-strict regression
versus `CentralizedOrchestrator`; the classifier still emits it as a
last-rank option so authors who deliberately want a no-synthesis
fan-out can satisfy the lint.

### Hexagonal placement

- Domain types (`TaskFeatures`, `Topology`, `TopologyRecommendation`,
  the trait) live in `ork-core`.
- The deterministic `HeuristicClassifier` lives in `ork-core` (no
  external deps).
- The LLM-backed `TaskFeatureExtractor` adapter lives in `ork-cli`
  (and `ork-api` if/when a server-side surface is added), wired
  through `LlmProvider` from ADR [`0012`](0012-multi-llm-providers.md).
- Structural topology derivation (compiled-graph → `Topology`) lives
  next to the compiler in `ork-core`, keyed off the new step kinds
  introduced by ADR [`0018`](0018-dag-executor-enhancements.md). If
  0018 has not yet landed when this ADR is implemented, structural
  derivation falls back to recognising only `SingleAgent` and
  `SequentialPipeline`; the lint still works for those shapes.

## Acceptance criteria

- [ ] `TaskFeatures`, `Topology`, `TopologyRecommendation`, and the
      `TopologyClassifier` trait defined in
      `crates/ork-core/src/topology/mod.rs` with the signatures shown
      in `Decision`.
- [ ] `HeuristicClassifier` defined in
      `crates/ork-core/src/topology/heuristic.rs` and re-exported from
      `ork_core::topology`.
- [ ] `TaskFeatureExtractor` trait defined in
      `crates/ork-core/src/ports/task_feature_extractor.rs` and
      re-exported from `ork_core::ports`.
- [ ] Unit test
      `crates/ork-core/src/topology/heuristic.rs::tests::matches_paper_table`
      asserts every row of the decision table in `Decision` produces
      the stated top topology and confidence.
- [ ] Unit test `tests::ranks_independent_last` asserts
      `IndependentEnsemble` is never `result[0]` for any input.
- [ ] Function `derive_structural_topology(&CompiledWorkflow) -> Topology`
      defined in `crates/ork-core/src/workflow/compiler.rs` (or a
      sibling module) with a unit test covering each topology shape
      that the engine actually supports at implementation time.
- [ ] CLI subcommand `ork workflow recommend-topology` registered in
      [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs)
      and accepts both `--task` and `--features <path>`.
- [ ] CLI subcommand `ork workflow lint` (existing or new) emits a
      `topology` warning when `derive_structural_topology(workflow)`
      is not present in the top 2 recommendations of
      `classify(features)`. Warning text references both the authored
      and the recommended topology by name.
- [ ] LLM-backed `LlmTaskFeatureExtractor` adapter in `ork-cli` (no
      `LlmProvider` import in `ork-core`).
- [ ] Integration test
      `crates/ork-cli/tests/topology_lint.rs::warns_on_mismatch` runs
      the lint against a fixture workflow whose authored shape
      contradicts its declared `features:` block and asserts the
      warning is emitted with exit code 0.
- [ ] Integration test `accepts_aligned_workflow` runs the lint
      against `workflow-templates/change-plan.yaml` extended with a
      hand-authored `features:` block matching its sequential pipeline
      shape and asserts no warning is emitted.
- [ ] [`workflow-templates/`](../../workflow-templates/) gains a
      `features:` block on each existing template (5 files), authored
      to reflect the existing shape so the new lint is green out of
      the box.
- [ ] [`README.md`](README.md) ADR index row added (number 0026,
      phase 4).
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- Authors get an actionable, deterministic second opinion on topology
  before runtime. Today they get nothing.
- The classifier is a pure function — cheap to call, trivial to test,
  no provider dependency, no flakiness.
- The structural-topology helper gives ADR
  [`0022`](0022-observability.md) a stable label per workflow run
  (`topology="centralized_orchestrator"`) for dashboards and SLOs.
- Decoupling features (extracted by an LLM) from the rule (fixed
  table) means the rule is auditable and the extractor is swappable
  without touching `ork-core`.

### Negative / costs

- The decision table is calibrated to one paper. If field experience
  diverges the table needs revising — but revising it is cheap (one
  file, one test) compared to retraining a model.
- An LLM-backed extractor is non-deterministic. Two `recommend-topology`
  invocations on the same prose can return different features and
  therefore different recommendations. Mitigation: the YAML
  `features:` block, plus the lint preferring author-supplied features
  over the extractor.
- The lint will emit warnings on existing third-party workflows once
  shipped. Mitigation: the lint is opt-in (a flag on the existing
  lint command); no CI gate is added by this ADR.
- "Topology" is a coarse abstraction. Real workflows mix topologies
  (a sequential outer pipeline with a parallel middle stage). The
  structural-topology derivation collapses these to the dominant
  shape; nuanced cases will get advisory mismatch warnings the author
  may correctly ignore.

### Neutral / follow-ups

- A future ADR could promote the lint into a CI gate per tenant
  policy (touches ADR [`0021`](0021-rbac-scopes.md)).
- A future ADR could add an LLM-as-judge auto-rewriter that, given a
  mismatch, proposes a replacement YAML — out of scope here.
- The `verifiable` feature dovetails with ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md): when
  the classifier picks `CentralizedOrchestrator` partly because
  `verifiable: true`, the rationale will recommend wiring the verifier.

## Alternatives considered

- **Pure LLM classifier (no rule table).** Ask the model "what
  topology fits this task?" with no structured features in between.
  Rejected: non-deterministic, unauditable, regresses on prompt-tuning
  drift, and discards the paper's empirical thresholds. The current
  decision keeps the LLM scoped to feature extraction where its
  fuzziness is acceptable.
- **Train a small ML classifier on a labelled workflow corpus.**
  Rejected for now: ork has no such corpus, and the paper's R²=0.513
  + 87 % accuracy comes from a hand-derived rule over two features —
  a learned model would have to beat that to be worth the
  data-collection burden. Revisit after a year of dogfooding.
- **Make the classifier a runtime gate that refuses bad topologies.**
  Rejected: violates the principle that authors keep authority over
  their workflows, and would block legitimate edge cases (e.g. a
  fan-out used purely for observability where end-to-end accuracy
  isn't the optimisation target). Advisory lint only.
- **Embed the rules in the YAML compiler so the compiled graph is
  rewritten.** Rejected: implicit rewrites at compile time are the
  worst of both worlds — authors lose authority *and* the rewrite is
  invisible. The lint surfaces the mismatch and lets the author
  decide.
- **Skip this ADR; rely on ADR 0025's verifier to catch bad
  topologies at runtime.** Rejected: the verifier catches *output*
  errors, not *shape* errors. A sequential task wrongly run as an
  independent ensemble will produce verifiable-looking nonsense at
  17.2× the per-step error rate; the classifier prevents that class
  of mistake from being authored at all.

## Affected ork modules

- [`crates/ork-core/src/topology/`](../../crates/ork-core/src/) — new
  module hosting `TaskFeatures`, `Topology`,
  `TopologyRecommendation`, `TopologyClassifier`,
  `HeuristicClassifier`.
- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/ports/) —
  adds `task_feature_extractor.rs`.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
  — adds `derive_structural_topology`.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — `WorkflowDefinition` gains an optional `features: Option<TaskFeatures>`
  field (additive, default `None`).
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) —
  registers `ork workflow recommend-topology` and the new lint check.
- [`crates/ork-cli/`](../../crates/ork-cli/) — adds
  `LlmTaskFeatureExtractor` adapter.
- [`workflow-templates/`](../../workflow-templates/) — each shipped
  template gains a `features:` block.

## Reviewer findings

Filled in after the required `code-reviewer` subagent pass.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Google Research, *Towards a Science of Scaling Agent Systems* (April 2025) | https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/ | `HeuristicClassifier` decision table; `Topology` enum; the `IndependentEnsemble`-never-wins rule |
| AutoGen `GroupChatManager` topology selection | https://microsoft.github.io/autogen/ | Inspires the orchestrator-vs-decentralised distinction; ork keeps it static-author-time, not runtime |
| LangGraph topology primitives (`StateGraph`, `Send`) | https://langchain-ai.github.io/langgraph/ | Inspires the structural-topology derivation pass; ork's compiler is the equivalent inspection point |

## Open questions

- Should `tool_count` be derived from the workflow YAML
  (`sum(step.tools.len())`) or supplied by the author? Implementation
  bias: derive automatically, let the author override in the
  `features:` block.
- Should the lint distinguish "advisory" (top-2 mismatch) from
  "strong" (recommended topology has confidence ≥ 0.80 and authored
  shape is not in the top 3)? Defer to first round of dogfooding
  feedback.
- How does this interact with `delegate_to` chains (ADR 0006) where
  the runtime topology depends on the called peer's own workflow?
  Initial implementation treats `delegate_to` opaquely (counts as one
  agent step); revisit once cross-workflow topology aggregation is
  needed.

## References

- *Towards a Science of Scaling Agent Systems — When and Why Agent
  Systems Work*, Google Research, April 2025:
  https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/
- ADR [`0018`](0018-dag-executor-enhancements.md) — DAG executor
  enhancements (provides the `Parallel`, `Switch`, `Map`, `Loop`
  step kinds the classifier reasons about).
- ADR [`0006`](0006-peer-delegation.md) — Peer delegation (provides
  the `HybridDelegation` realisation).
- ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md) —
  Typed-output validation and verifier-agent port (the `verifiable`
  feature ties into this).
- ADR [`0022`](0022-observability.md) — consumer of
  `derive_structural_topology` for per-run topology labels.
