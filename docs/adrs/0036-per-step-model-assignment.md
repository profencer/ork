# 0036 — Per-step model assignment and cross-agent composition

- **Status:** Proposed
- **Date:** 2026-04-28
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0006, 0011, 0012, 0018, 0022, 0026, 0033, 0034, 0038, 0045
- **Supersedes:** —

## Context

The same coding task benefits from different models in different
phases. A frontier reasoning model is the right choice for *plan*
("think hard about what to change and why"), a cheaper / faster
executor model is the right choice for *edit* ("apply this diff"),
and a *materially different* model — different vendor or family — is
the right choice for *plan verification* (ADR [`0038`]) so the
verifier does not echo the planner's blind spots.

Today ork has half of this surface but not the other half:

- **Per-step provider/model overrides exist already.** ADR
  [`0012`](0012-multi-llm-providers.md) introduced
  `WorkflowStep.provider: Option<String>` and
  `WorkflowStep.model: Option<String>` at
  [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  with the resolution chain step → agent → tenant default → operator
  default. The router
  ([`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs))
  reads them on every `chat` / `chat_stream` and selects the right
  `OpenAiCompatibleProvider`. That mechanism handles the *intra-agent*
  axis: a single agent runs different steps on different models.
- **`ModelProfile` exists, but is not addressable per step.** ADR
  [`0034`](0034-per-model-capability-profiles.md) added
  `ModelProfile` (edit format, tool-catalog cap, compaction
  threshold, thinking-mode policy, verifier-model hint) and a
  `ModelProfileRegistry` keyed by either `ProfileId` or
  `(provider_id, model_id)`. ADR [`0033`](0033-coding-agent-personas.md)
  consumes a profile via `CodingPersona::model_profile_ref`. There
  is, today, no way for a workflow step to say *"this step uses
  profile `ork.profiles.frontier_planner` regardless of what the
  agent's persona resolves to"* — the only step-level overrides are
  raw `(provider, model)` strings that bypass the profile chain
  entirely. Steps that override `model:` get the right wire target
  but the wrong tuning gate (edit format, compaction threshold,
  grammar gate, thinking mode), because the profile resolves from
  `(provider, model)` and may not match the operator's intent.
- **Persona-driven model selection.** ADR
  [`0033`](0033-coding-agent-personas.md)'s `CodingPersona` carries a
  `model_profile_ref: ModelProfileRef`, but the persona's effect on
  the model is currently bound at *agent install time* — the
  installer wires the persona's profile into the `LocalAgent` and the
  same profile applies to every step that agent runs. There is no
  mechanism today for a single workflow step to say "delegate this
  step to a peer that runs the `executor` persona, on its preferred
  profile, regardless of what the parent agent uses."
- **Plan verification (ADR [`0038`]) needs a non-echoing peer.** ADR
  [`0038`]'s gate dispatches an architect's plan to one or more
  `plan_verifier` peers (per ADR [`0033`]) whose models must differ
  *materially* from the planner's. ADR [`0034`]'s
  `recommended_plan_verifier_model_id` hint is the configured-side
  pointer; what is missing is the workflow shape that reifies it —
  a template that proves "planner step uses profile A, verifier
  step uses profile B, executor step uses profile C, all in one
  run."
- **Cost/latency observability per step.** ADR
  [`0022`](0022-observability.md)'s `agent.send_stream.tick` span
  carries `tokens_in` / `tokens_out` and the metric
  `ork_llm_tokens_total{provider,model,direction}` is keyed by
  resolved `(provider, model)`. Without a `step_id` and a `profile_id`
  on those events, an operator can see "this run cost $X" but cannot
  attribute the cost to *which step* drove it. Cross-verification
  overhead (the sum over all `Verify`-phase steps) is therefore not
  measurable today.

The boundary with neighbouring ADRs is delicate and worth pinning up
front:

- **ADR [`0026`](0026-workflow-topology-selection-from-task-features.md)**
  — automatic *topology* selection (which steps run, in what shape).
  Out of scope here: this ADR is mechanism, not policy. ADR
  [`0026`]'s classifier may emit a workflow whose steps already carry
  the per-step model assignments this ADR introduces.
- **ADR [`0034`](0034-per-model-capability-profiles.md)** — the
  profile *descriptors* and the registry. This ADR's job is to make
  them addressable from the workflow step.
- **ADR [`0038`]** — the cross-verification *protocol* (handoff,
  aggregation, halt conditions). This ADR provides the workflow shape
  the protocol consumes.
- **ADR [`0045`]** — *team decomposition policy* (who plays which
  role, how diffs merge). Out of scope: this ADR ships per-step
  mechanism plus one canonical demo template; ADR [`0045`] decides
  when to assemble multi-agent teams in the first place.

## Decision

ork **introduces** a `model_profile_ref` field on `WorkflowStep`
parallel to the existing `provider` / `model` fields, extends the
router's resolution chain to consult the registry from ADR
[`0034`](0034-per-model-capability-profiles.md), wires per-step
profile resolution into peer-delegated steps (ADR
[`0006`](0006-peer-delegation.md)) so persona-default profiles flow
through delegation, ships a canonical
`workflow-templates/planner-verifier-executor.yaml` template in two
variants (single-process role-swap and three-agent peer-delegated),
and lights up `step_id` + `profile_id` on the per-step LLM events
landing in ADR [`0022`](0022-observability.md)'s task event log so
cross-verification overhead becomes measurable.

The feature is **mechanism only** — automatic decomposition policy
lives in ADR [`0045`].

### `WorkflowStep.model_profile_ref`

```rust
// crates/ork-core/src/models/workflow.rs

pub struct WorkflowStep {
    // ...existing fields (id, agent, tools, prompt_template, depends_on,
    // condition, for_each, iteration_var, delegate_to)...

    /// Per-step provider override (ADR 0012). Highest precedence in the
    /// `(provider, model)` chain; unchanged by this ADR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Per-step model override (ADR 0012). Unchanged by this ADR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Per-step *profile* override (this ADR; ADR 0034).
    ///
    /// When set, the agent loop materialises an effective profile by
    /// resolving this id against `ModelProfileRegistry` for the run's
    /// tenant, *before* falling back to the persona's profile (ADR
    /// 0033) or the `for_model` lookup. The resolved profile's
    /// `(provider_id, model_id)` is then used as the wire target unless
    /// `provider` / `model` are also set, in which case the explicit
    /// `(provider, model)` wins for the wire and the profile only
    /// drives behavioural tuning (edit format, compaction threshold,
    /// thinking mode, grammar gate, tool-catalog cap).
    ///
    /// `None` ⇒ profile falls through to the persona / `for_model` /
    /// neutral-default chain from ADR 0034.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile_ref: Option<ProfileId>,
}
```

`ProfileId` is the same type defined in
[`crates/ork-llm/src/profile.rs`](../../crates/ork-llm/src/) by ADR
[`0034`](0034-per-model-capability-profiles.md). YAML authors write
the bare string id:

```yaml
- id: plan
  agent: ork.persona.architect
  model_profile_ref: ork.profiles.frontier_planner
- id: verify
  agent: ork.persona.plan_verifier
  model_profile_ref: ork.profiles.frontier_verifier
- id: edit
  agent: ork.persona.executor
  model_profile_ref: ork.profiles.frontier_executor
```

The compiler at
[`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
propagates the field unchanged to `WorkflowNode`.

### Resolution chain (extended)

When a step starts, the agent loop materialises both an effective
`(provider, model)` pair and an effective `ModelProfile`. The two
chains are now intentionally separate:

**Wire target — `(provider, model)`** (unchanged from ADR
[`0012`](0012-multi-llm-providers.md)):

1. `WorkflowStep.provider` / `.model` (explicit step override)
2. If `WorkflowStep.model_profile_ref` is set and neither
   `provider` nor `model` is set, the resolved profile's
   `(provider_id, model_id)` (this ADR; new)
3. `AgentConfig.provider` / `.model`
4. `TenantSettings.default_provider` / `.default_model`
5. `[llm].default_provider`, then resolved provider's `default_model`

**Behavioural profile** (extended from ADR
[`0034`](0034-per-model-capability-profiles.md)):

1. `WorkflowStep.model_profile_ref` (this ADR; new — top of chain)
2. `AgentConfig.persona`'s `model_profile_ref` (ADR
   [`0033`](0033-coding-agent-personas.md))
3. `ModelProfileRegistry::for_model(tenant, resolved_provider,
   resolved_model)` (ADR
   [`0034`](0034-per-model-capability-profiles.md))
4. `ModelProfile::neutral_default(caps)` (ADR
   [`0034`](0034-per-model-capability-profiles.md))

The two chains can disagree intentionally: a step may pin
`model: claude-sonnet-4-6` for the wire while pinning
`model_profile_ref: ork.profiles.local_coder_small` for the tuning
(unusual, but useful in tests where the operator wants frontier
model output with a small-model tool-catalog cap). The agent loop
emits `tracing::warn!(target = "ork.profile",
profile_target_mismatch = true)` when the resolved profile's
`(provider_id, model_id)` differs from the resolved wire pair, so
the case is observable.

### Router integration

`LlmRouter::resolve` ([`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs))
gains a single new branch in its existing chain: if the request's
`ResolveContext` carries a `model_profile_ref` and `provider` /
`model` are unset, the router consults the
`ModelProfileRegistry::get(tenant, profile_id)` and substitutes the
profile's `(provider_id, model_id)`. The router stays
compatibility-focused and *does not* see the full profile object —
the agent loop in
[`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
remains the consumer of behavioural fields per ADR
[`0034`](0034-per-model-capability-profiles.md).

The `ResolveContext` struct gains one field:

```rust
// crates/ork-llm/src/router.rs

pub struct ResolveContext {
    pub tenant: TenantId,
    // ...existing fields...

    /// Set by the workflow engine when the current step carries
    /// `model_profile_ref` (ADR 0036). Consulted only when neither
    /// `provider` nor `model` is set on the request.
    pub model_profile_ref: Option<ProfileId>,
}
```

### Peer delegation: persona profile flows through

ADR [`0006`](0006-peer-delegation.md)'s `delegate_to` step builds a
child `AgentMessage` against the target peer. Today the child
inherits the parent step's resolved `(provider, model)` if neither
the parent step nor the parent agent specifies one. **This ADR
changes that:** when the delegation target resolves to an agent
with a persona (ADR [`0033`](0033-coding-agent-personas.md)), the
child task's `ResolveContext.model_profile_ref` defaults to the
*target persona's* `model_profile_ref`, not the parent step's.

Concretely, in `crates/ork-core/src/workflow/delegation.rs`, the
delegation builder at the engine boundary sets:

```rust
let target_profile = registry
    .get(target_persona.model_profile_ref.profile_id())
    .or_else(|| target_persona.model_profile_ref.fallback());
let mut ctx = ResolveContext::for_tenant(tenant);
ctx.model_profile_ref = step.model_profile_ref
    .clone()
    .or_else(|| target_profile.map(|p| p.id.clone()));
```

The step-level `model_profile_ref` still wins over the persona
default — operators retain a workflow-side override hatch — but the
common case ("delegate to a verifier persona; let the persona pick
its own model") needs no extra YAML.

### Reference template — `planner-verifier-executor.yaml`

A new template ships at
[`workflow-templates/planner-verifier-executor.yaml`](../../workflow-templates/)
in two variants. Both run end-to-end in CI and the demo; both end in
a single git commit on a feature branch.

#### Variant A — single-process role-swap

One `solo_coder` agent runs all three steps; the per-step
`model_profile_ref` is the *only* thing that changes the model under
the agent. This proves the intra-agent axis without any A2A wire
hops.

```yaml
name: planner-verifier-executor.solo
version: "1.0"
trigger:
  type: manual

steps:
  - id: plan
    agent: ork.persona.solo_coder
    phase: plan
    model_profile_ref: ork.profiles.frontier_planner
    tools: [read_file, code_search, recall]
    prompt_template: >
      Plan a fix for: {{input.brief}}.
      Respond with ONLY valid JSON of shape {plan: {...}, acceptance_criteria: [...]}.
    depends_on: []

  - id: verify
    agent: ork.persona.solo_coder
    phase: verify
    model_profile_ref: ork.profiles.frontier_verifier
    tools: [read_file, code_search, get_diagnostics, get_repo_map]
    prompt_template: >
      Verify the plan from {{plan.output}} against the repo state.
      Call submit_plan_verdict with your structured verdict.
    depends_on: [plan]

  - id: edit
    agent: ork.persona.solo_coder
    phase: edit
    model_profile_ref: ork.profiles.frontier_executor
    tools: [read_file, write_file, apply_patch, run_tests, git_status, git_diff, git_commit]
    prompt_template: >
      Apply the approved plan {{plan.output}}; verdict: {{verify.output}}.
    depends_on: [verify]
```

The same `LocalAgent` runs all three steps, but each step's
`model_profile_ref` triggers a fresh effective-profile resolution
inside the agent loop. Edit format, compaction threshold, and
thinking-mode policy all swap per step. The verifier step uses a
profile *distinct* from the planner's — exercising the same-model
echo problem ADR [`0038`] is meant to solve, in the smallest
possible setup.

#### Variant B — three-agent peer-delegated

Three local `LocalAgent` instances are pre-installed under three
persona ids: `architect`, `plan_verifier`, `executor`. Steps
`delegate_to:` each peer over A2A — the same wire path ADR
[`0038`]'s production cross-verification uses. The persona's
`model_profile_ref` flows through delegation by default; the step
still names a `model_profile_ref` to make the recommendation
explicit.

```yaml
name: planner-verifier-executor.team
version: "1.0"
trigger:
  type: manual

steps:
  - id: plan
    agent: ork.persona.architect
    phase: plan
    model_profile_ref: ork.profiles.frontier_planner
    tools: [read_file, code_search, recall]
    prompt_template: >
      Plan a fix for: {{input.brief}}.
    depends_on: []

  - id: verify
    agent: ork.persona.architect             # parent step's agent
    phase: verify
    delegate_to:
      agent: ork.persona.plan_verifier        # peer
      await: true
      timeout: 120s
      prompt_template: >
        {"plan": {{plan.output}},
         "repo_context": {{input.repo_context}},
         "review_brief": "{{input.review_brief}}"}
    # Profile pin for the verifier hop; recommended_plan_verifier_model_id
    # on the planner's profile (ADR 0034) is the implicit fallback when
    # this is omitted.
    model_profile_ref: ork.profiles.frontier_verifier
    depends_on: [plan]

  - id: edit
    agent: ork.persona.executor
    phase: edit
    model_profile_ref: ork.profiles.frontier_executor
    tools: [read_file, write_file, apply_patch, run_tests, git_commit]
    prompt_template: >
      Plan: {{plan.output}}
      Verdict: {{verify.delegated.output}}
      If verdict.verdict != "approve", stop and explain.
    depends_on: [verify]
```

Variant B's `verify` step uses ADR [`0006`](0006-peer-delegation.md)'s
`delegate_to:` to dispatch the plan verification to a peer. The
peer's persona is `plan_verifier`, whose `model_profile_ref` (ADR
[`0033`](0033-coding-agent-personas.md)) points at
`ork.profiles.frontier_verifier`. The step-level
`model_profile_ref` here is documentary — it asserts the
recommendation matches what the persona would have chosen anyway.
Aggregation of verdicts from N verifiers is **not** demonstrated
here; ADR [`0038`] owns it.

Both variants exit with `TaskState::Completed` and a single commit
on a feature branch; both can be executed by a new
[`demo/scripts/stage-11-planner-verifier-executor.sh`](../../demo/scripts/)
script that mirrors the convention of stages 0–10. The script
asserts that the three steps emit three *distinct* resolved model
ids in the task event log — exercising the no-echo property
end-to-end.

### Observability — per-step model and profile on the event log

ADR [`0022`](0022-observability.md)'s `agent.send_stream.tick` span
already carries `tokens_in` / `tokens_out`. This ADR adds three
attributes to that span (and to the `agent.send` span that wraps
it):

| Attribute | Source | Purpose |
| --------- | ------ | ------- |
| `step_id` | `WorkflowStep.id` of the step that drove the LLM call | attribute cost / latency to a step |
| `model_profile_id` | resolved profile's `id` | distinguish "the planner's tokens" from "the verifier's tokens" |
| `phase` | `PersonaPhase` (ADR 0033) of the step, when set | dashboards filterable by Plan / Verify / Edit |

The same triple lands on the per-task event log row
([`a2a_task_events`](../../migrations/) — ADR
[`0022`](0022-observability.md) §Pillar 3) when `kind = "tool_call"`
or `kind = "message"` and the row is produced by an LLM completion.
The `tokens_in` / `tokens_out` already on the row are sufficient to
sum cross-verification overhead by `phase = "verify"` over a run.

The `ork_llm_tokens_total` counter labels gain `step_id` and
`model_profile_id`; cardinality is bounded by
`(workflow_definition × step.id × tenant × profile_id)`, which is
the same order as the existing `ork_workflow_step_total` label set.
A budget-monitor consumer (ADR
[`0022`](0022-observability.md)'s `BudgetMonitor`) can slice cost
by phase or step without touching tracing.

### Out of scope

- **Automatic per-step model selection from task features.** Owned
  by ADR [`0026`](0026-workflow-topology-selection-from-task-features.md).
  This ADR is a *manual* assignment surface; ADR [`0026`]'s classifier
  emits the same field this ADR adds.
- **Team decomposition policy.** Owned by ADR [`0045`]: when to use
  a single solo agent vs. a planner-verifier-executor team, how to
  reconcile multi-agent diffs, when to escalate. This ADR provides
  the per-step mechanism plus one canonical template; ADR [`0045`]
  decides whether the template gets selected for a given task.
- **Cross-verification protocol semantics.** Owned by ADR [`0038`]:
  number of verifiers, halt-on-disagreement rules, retry budget,
  same-model-echo guard. This ADR provides the workflow shape and
  the per-step profile assignment; ADR [`0038`]'s gate consumes them.
- **Profile registry and descriptors.** Owned by ADR
  [`0034`](0034-per-model-capability-profiles.md). This ADR consumes
  the registry without changing its shape.
- **Budget enforcement.** ADR [`0022`](0022-observability.md)'s
  `BudgetMonitor` consumes the new metric labels but the policy
  ("halt the run when verifier cost exceeds 30% of plan cost") is
  not introduced here.

## Acceptance criteria

- [ ] Field `model_profile_ref: Option<ProfileId>` added to
      `WorkflowStep` at
      [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
      with `#[serde(default, skip_serializing_if = "Option::is_none")]`;
      existing serialised workflows round-trip unchanged.
- [ ] YAML parser accepts the bare-string id form
      (`model_profile_ref: ork.profiles.frontier_planner`); verified
      by
      `crates/ork-core/tests/workflow_parse.rs::model_profile_ref_bare_string_round_trips`.
- [ ] `WorkflowStep` -> `WorkflowNode` propagation in
      [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
      forwards `model_profile_ref` unchanged; verified by
      `crates/ork-core/tests/workflow_compile.rs::propagates_model_profile_ref`.
- [ ] `ResolveContext` at
      [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs)
      gains `model_profile_ref: Option<ProfileId>`; default value is
      `None` and pre-existing call sites compile unchanged via
      `ResolveContext::for_tenant(...)`.
- [ ] `LlmRouter::resolve` consults
      `ModelProfileRegistry::get(tenant, profile_id)` and substitutes
      the profile's `(provider_id, model_id)` only when `req.provider`
      and `req.model` are both `None` and `ctx.model_profile_ref`
      is `Some`; verified by
      `crates/ork-llm/tests/router_profile_ref.rs::profile_resolves_provider_model`.
- [ ] When `req.provider` or `req.model` is set alongside a profile
      id, the explicit pair wins for the wire and the agent loop
      emits a `tracing::warn!(target = "ork.profile",
      profile_target_mismatch = true)` event; verified by
      `crates/ork-agents/tests/profile_target_mismatch.rs::warns_when_wire_overrides_profile`.
- [ ] `LocalAgent::run` materialises the effective profile in this
      order: `WorkflowStep.model_profile_ref` →
      `AgentConfig.persona`'s `ModelProfileRef` →
      `ModelProfileRegistry::for_model` → `neutral_default`;
      verified by
      `crates/ork-agents/tests/effective_profile_chain.rs::step_overrides_persona_overrides_for_model`.
- [ ] `crates/ork-core/src/workflow/delegation.rs` builds the child
      task's `ResolveContext.model_profile_ref` from the *target
      persona*'s `model_profile_ref` when the parent step does not
      set one; verified by
      `crates/ork-core/tests/delegation_profile.rs::child_inherits_target_persona_profile`.
- [ ] Workflow template
      [`workflow-templates/planner-verifier-executor.yaml`](../../workflow-templates/)
      created with the **solo** variant shown in `Decision`; loaded
      by the workflow compiler without errors —
      `cargo test -p ork-core compiler::loads_planner_verifier_executor_solo`.
- [ ] Workflow template
      [`workflow-templates/planner-verifier-executor.team.yaml`](../../workflow-templates/)
      created with the **team** variant shown in `Decision`; loaded
      by the workflow compiler without errors —
      `cargo test -p ork-core compiler::loads_planner_verifier_executor_team`.
- [ ] Demo script
      [`demo/scripts/stage-11-planner-verifier-executor.sh`](../../demo/scripts/)
      runs the **solo** variant end-to-end against a toy crate and
      asserts the three steps emit three distinct resolved
      `model_profile_id` values in the task event log; exits 0 on
      success.
- [ ] Demo script (same file or split) runs the **team** variant
      end-to-end with three pre-installed personas
      (`architect`, `plan_verifier`, `executor`) and asserts the
      `verify` step's child task carries
      `model_profile_id = ork.profiles.frontier_verifier` regardless
      of the parent step's profile.
- [ ] Expected stdout / file-tree fixtures
      [`demo/expected/stage-11.txt`](../../demo/expected/) and
      [`demo/expected/stage-11-tree.txt`](../../demo/expected/)
      created and checked.
- [ ] Tracing span `agent.send_stream.tick` carries attributes
      `step_id`, `model_profile_id`, and `phase` (when known) on the
      events emitted by `LocalAgent::send_stream`; verified by
      `crates/ork-agents/tests/profile_observability.rs::span_carries_step_and_profile`.
- [ ] Task event log rows produced by LLM completions carry
      `step_id` and `model_profile_id` in their `payload` JSON;
      verified by
      `crates/ork-persistence/tests/task_events_profile.rs::row_carries_profile_id`.
- [ ] Prometheus counter `ork_llm_tokens_total` accepts new labels
      `step_id` and `model_profile_id`; verified by a unit test in
      [`crates/ork-api`](../../crates/ork-api/) that scrapes
      `/metrics` after a one-step run and asserts the labels are
      present.
- [ ] Engine integration test
      `crates/ork-core/tests/per_step_profile.rs::three_steps_three_profiles`
      runs the solo variant against stub `LlmProvider`s and asserts
      each step's `chat_stream` request reaches a *different* stub
      (one per profile id), proving the per-step assignment took
      effect.
- [ ] [`docs/adrs/README.md`](README.md) ADR index row for `0036`
      added.
- [ ] [`docs/adrs/metrics.csv`](metrics.csv) row appended after
      implementation lands.

## Consequences

### Positive

- The same `solo_coder` agent runs *plan*, *verify*, and *edit* on
  three different models without any code change — only YAML. The
  intra-agent axis is unblocked.
- ADR [`0038`]'s plan-verification protocol gets a workflow shape
  it can compose against: a `delegate_to:` to a peer whose persona
  pins a verifier-tuned profile. The "no same-model echo"
  invariant becomes expressible as a workflow-time assertion ("the
  planner step and verifier step resolve to different
  `model_profile_id`s"), not a runtime hope.
- Persona-driven model selection finally flows through delegation
  the way operators expect: "delegate to the verifier persona" runs
  the verifier *on its preferred profile*, not on the parent's.
- Per-step cost attribution lands in ADR
  [`0022`](0022-observability.md)'s metrics and event log, so an
  operator can read "this run cost $X, of which Y came from the
  verifier" — the precondition for any future budget rule that caps
  cross-verification overhead.
- The two-template demo (solo + team) is a load-bearing
  forward-compatibility test: ADR [`0045`] can pick up the team
  template and grow it; ADR [`0026`] can emit the solo template
  from its classifier; ADR [`0038`] can add aggregation logic
  around the team template's `verify` step. Each downstream ADR
  inherits a working baseline.

### Negative / costs

- Two parallel override chains (`(provider, model)` vs. profile)
  now run side by side. Operators have to read the resolution rules
  in this ADR's `Decision` to know which one applies; the warn
  event on mismatch is the safety net but not a substitute for
  documentation. We accept this because collapsing them — making
  the profile imply the wire and forbidding raw `(provider, model)`
  step overrides — would be a wire-breaking change to ADR
  [`0012`](0012-multi-llm-providers.md), which is already shipping.
- Cardinality on `ork_llm_tokens_total` rises by a factor of
  `step_id × profile_id`. For a tenant running ten workflows with
  five steps each across three profiles, that is up to 150 series
  per provider/model/direction triple. Within Prometheus best
  practice but worth flagging; we mitigate by recommending in the
  ADR text that operators drop `step_id` from the recording rule
  used for the cost dashboard if they want a coarser view.
- The persona-profile-flow-through-delegation behaviour is a small
  semantic shift from ADR [`0006`](0006-peer-delegation.md): today
  a delegated child runs under whatever defaults its agent has, and
  the parent's `(provider, model)` does *not* leak. We are now
  saying the *target persona's* profile leaks into the child's
  `ResolveContext`. This is the desired behaviour but it differs
  from "no leak" subtly enough to surprise a careful reader. We
  document it explicitly in the delegation builder and add the
  override hatch (step-level `model_profile_ref` wins).
- Workflow YAML grows another optional field. We have already
  accumulated `provider`, `model`, `delegate_to`, `condition`,
  `for_each`, `iteration_var`, `phase`. Adding
  `model_profile_ref` continues that drift; a future cleanup ADR
  may collapse `provider` + `model` + `model_profile_ref` into one
  `llm:` block. Out of scope here.
- The two demo variants double the maintenance surface for the
  template directory. We accept this because they exercise
  different code paths (intra-agent vs. peer-delegated) that ADR
  [`0038`] and ADR [`0045`] both depend on; merging them would
  hide the distinction this ADR is meant to surface.
- Adding `step_id` to LLM-side tracing crosses a layering boundary:
  `ork-llm` does not depend on `ork-core`'s workflow types today.
  We avoid the new dep by passing `step_id` as a `String` through
  `ResolveContext` rather than as a typed `WorkflowStepId`. The
  hexagonal boundary stays intact.

### Neutral / follow-ups

- ADR [`0038`]'s gate consumes `model_profile_id` from the event
  log to enforce the no-echo property; until 0038 lands, the
  property is asserted only by the demo script.
- ADR [`0026`]'s classifier emits workflows with
  `model_profile_ref` already populated — the classifier becomes a
  thin policy layer over this mechanism.
- ADR [`0045`] (multi-agent teams) builds the team variant of this
  template into a first-class team decomposition; the persona ids
  reserved by ADR [`0033`](0033-coding-agent-personas.md) are
  enough to keep the wire stable across the transition.
- A future ADR may surface the per-step profile in the web UI
  gateway (ADR [`0017`](0017-webui-chat-client.md)) so an end user
  can see "this answer came from `frontier_verifier`" inline with
  the message stream.
- ADR [`0022`](0022-observability.md)'s `BudgetMonitor` gains the
  ability to slice by `phase`; a downstream policy ADR may add
  rules of the form "halt the run when `phase=verify` cost exceeds
  30% of `phase=plan` cost." Not introduced here.

## Alternatives considered

- **Reuse `WorkflowStep.model: Option<String>` and let
  `ModelProfileRegistry::for_model` derive the profile from
  `(provider, model)`.** Rejected: the inverse map is many-to-one
  (the same `(provider, model)` can be the canonical target for
  multiple profiles in different deployments — e.g.
  `frontier_planner` and `frontier_executor` may both point at
  `gpt-5` initially), so `for_model` has to pick *some* profile and
  the wrong one is silently chosen. A stable `ProfileId` makes the
  intent explicit.
- **Drop the per-step override entirely; require operators to
  install one agent per persona-profile pair and use peer
  delegation.** Rejected: forces every cross-model run to take an
  A2A wire hop even in single-process deployments, doubling latency
  and complicating the toy-crate demo. The intra-agent axis is real
  and worth supporting cheaply.
- **Make the per-step assignment a *list* of profile ids and let
  the agent loop pick at runtime.** Rejected: that is automatic
  selection, which belongs to ADR
  [`0026`](0026-workflow-topology-selection-from-task-features.md).
  Mixing manual and automatic in one field would force every
  downstream consumer (compiler, router, observability) to handle
  both shapes.
- **Persist the resolved profile id on `StepResult` instead of in
  the tracing span.** Rejected as the *only* surface — the tracing
  span is what dashboards index on, and `StepResult` is per-step
  not per-LLM-tick, so a step that calls the model multiple times
  (compaction, retry, embed-resolution) loses sub-step granularity.
  We do both: `StepResult.metadata.model_profile_id` is the durable
  record, the tracing span is the queryable one.
- **Single template combining solo + team with a feature flag.**
  Rejected: a flag-gated template hides the distinction between
  intra-agent role-swap and inter-agent delegation, which is the
  conceptual point of this ADR. Two templates make the difference
  inspectable in the YAML directory listing.
- **Bake the verifier-model hint into the workflow template
  inline (e.g. `verifier_profile: ork.profiles.frontier_verifier`
  on the `delegate_to:` block).** Rejected: that duplicates the
  hint that ADR
  [`0034`](0034-per-model-capability-profiles.md) already encodes
  on the planner's profile (`recommended_plan_verifier_model_id`)
  and creates a third place an operator can pin the verifier model
  (persona, profile-hint, template). Keeping the hint on the
  profile and the explicit override on the workflow step is the
  smaller surface.
- **Push profile resolution all the way into the wire — let
  `LlmRouter::chat` accept a `ProfileId` directly and have it
  drive both target and tuning.** Rejected: the router is the
  compatibility layer for ADR
  [`0012`](0012-multi-llm-providers.md)'s `LlmProvider` trait;
  layering rich behavioural fields on it would pollute that trait
  and force every alternative provider implementation (vendor
  SDKs, mock providers, OpenAI-compatible) to ignore most of them.
  The agent loop stays the consumer of behavioural tuning, per
  ADR [`0034`](0034-per-model-capability-profiles.md).
- **Adversarial-review alternative:** *"You are committing
  workflow YAML to a particular profile id. The id is operator
  configuration; what happens when an operator renames a profile
  or removes one? Existing workflows break silently."* Rebuttal:
  the registry's `get(tenant, profile_id)` already returns
  `Option<Arc<ModelProfile>>`; the agent loop falls through to
  `for_model` then `neutral_default` when a referenced id is
  absent and emits a `tracing::warn!(target = "ork.profile",
  profile_id_unresolved = true)`. This degrades gracefully (the
  step still runs against `(provider, model)` defaults) rather
  than failing hard. The acceptance criteria require the warn
  event but not a hard failure — symmetric with how unknown
  agent ids in `delegate_to:` are handled in ADR
  [`0006`](0006-peer-delegation.md).

## Affected ork modules

- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)
  — `WorkflowStep::model_profile_ref` field, serde derives.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
  — propagate the field to `WorkflowNode`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
  — pass `model_profile_ref` into `ResolveContext` per step.
- [`crates/ork-core/src/workflow/delegation.rs`](../../crates/ork-core/src/workflow/delegation.rs)
  — derive the child task's profile from the target persona's
  `ModelProfileRef` when the parent step does not pin one.
- [`crates/ork-llm/src/router.rs`](../../crates/ork-llm/src/router.rs)
  — `ResolveContext::model_profile_ref` field; `resolve` branch
  consulting `ModelProfileRegistry::get` when wire pair is unset.
- [`crates/ork-agents/src/local.rs`](../../crates/ork-agents/src/local.rs)
  — extend the effective-profile resolution chain (step → persona →
  `for_model` → neutral); emit the `profile_target_mismatch`
  warning; thread `step_id`, `model_profile_id`, and `phase` into
  the `agent.send_stream.tick` span and the task event log payload.
- [`crates/ork-persistence/src/postgres/task_event_repo.rs`](../../crates/ork-persistence/src/) — payload JSON gains `step_id` and
  `model_profile_id` fields (additive; no migration needed since
  payload is JSONB).
- [`crates/ork-api/src/metrics.rs`](../../crates/ork-api/src/) —
  add `step_id` and `model_profile_id` labels to
  `ork_llm_tokens_total`.
- New: [`workflow-templates/planner-verifier-executor.yaml`](../../workflow-templates/)
  (solo variant) and
  [`workflow-templates/planner-verifier-executor.team.yaml`](../../workflow-templates/)
  (team variant).
- New: [`demo/scripts/stage-11-planner-verifier-executor.sh`](../../demo/scripts/)
  + matching `demo/expected/stage-11*.txt` fixtures.
- [`docs/adrs/README.md`](README.md) — ADR index row.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Aider | `--model` and `--editor-model` flags select different models for plan vs. edit | `WorkflowStep.model_profile_ref` per step |
| LangGraph | `RunnableBinding` rebinds the LLM per-node in a graph | `WorkflowStep.model_profile_ref` resolves at node start |
| Claude Code | Subagents (`.claude/agents/*.md`) declare their own model + tools and are invoked per task phase | Variant B's `delegate_to:` to per-persona peers |
| OpenHands | Per-`Agent` model assignment via `LLMConfig`; multi-agent debate composes them | Variant B's three-persona template |
| Solace Agent Mesh | `agent.llm` per-component config | Per-agent default (already in `AgentConfig.model`) plus this ADR's per-step override |
| GitHub Copilot Workspace | Plan and edit phases run different model tiers | Variant A's solo planner-verifier-executor template |

## Open questions

- **Coarser per-step LLM block.** A future cleanup may collapse
  `provider`, `model`, and `model_profile_ref` on `WorkflowStep`
  into a single `llm:` block (`{ profile: ..., provider: ...,
  model: ..., temperature: ... }`). Stance: defer until the field
  count is painful; today's three optionals are tolerable and the
  collapse is an additive YAML change.
- **Profile id versioning in workflow YAML.** When a profile id is
  renamed (e.g. `ork.profiles.frontier_planner` →
  `ork.profiles.frontier_planner.v2`), workflow templates pinned
  to the old id resolve to `None` and fall through to defaults.
  Stance: same as ADR
  [`0034`](0034-per-model-capability-profiles.md)'s open question
  on profile versioning — id-suffix when needed; the warn event
  surfaces the drift.
- **Cross-tenant verifier deployments.** A team flow might want
  to delegate plan verification to an *external* mesh's
  `plan_verifier` peer (a "verifier as a service"). The persona's
  `model_profile_ref` flowing through delegation works fine
  in-mesh; cross-mesh, the remote profile id may not exist
  locally. Stance: out of scope; ADR [`0042`]'s discovery service
  is the place to harmonise profile ids across meshes.
- **Per-step temperature override.** Some operators want
  `temperature: 0.0` for `plan` and `0.3` for `edit`. Today
  `default_temperature` is a profile field (ADR
  [`0034`](0034-per-model-capability-profiles.md)) and the request
  carries whatever the agent passes. Stance: add a step-level
  `temperature: Option<f32>` if dogfooding shows the profile
  default is insufficient — additive change.
- **Streaming verdicts and per-step profile.** When a `verify`
  step streams partial findings (ADR [`0038`] open question), do
  intermediate ticks all carry the same `model_profile_id`?
  Stance: yes — the profile is resolved once at step start and
  every emitted span / event re-uses it. Subsequent retries that
  swap models bump the value; that is the same behaviour as
  today's `(provider, model)` resolution.
- **Profile in `StepResult` metadata.** Should the resolved
  profile id be persisted on `StepResult` for offline analysis
  (run-replay, cost rollups)? Stance: yes — add
  `StepResult.metadata.model_profile_id` as an additive JSON
  field. Tracked as part of the implementation; not a separate
  acceptance criterion since the event-log row already carries it.

## References

- ADR [`0006`](0006-peer-delegation.md) — peer delegation surface
  (`agent_call`, `delegate_to:`)
- ADR [`0011`](0011-native-llm-tool-calling.md) — agent loop and
  tool catalog the per-step profile tunes
- ADR [`0012`](0012-multi-llm-providers.md) — the existing
  `(provider, model)` chain this ADR layers on top of
- ADR [`0018`](0018-dag-executor-enhancements.md) — DAG node walker
  that emits the per-step LLM events
- ADR [`0022`](0022-observability.md) — task event log and metrics
  the new attributes flow into
- ADR [`0026`](0026-workflow-topology-selection-from-task-features.md)
  — automatic emission of per-step profiles (out of scope here)
- ADR [`0033`](0033-coding-agent-personas.md) — `CodingPersona` and
  `model_profile_ref` consumed by the persona installer
- ADR [`0034`](0034-per-model-capability-profiles.md) —
  `ModelProfile`, `ProfileId`, and the registry this ADR addresses
- ADR 0038 (forthcoming) — plan cross-verification protocol
- ADR 0045 (forthcoming) — team decomposition policy
