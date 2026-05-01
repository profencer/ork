# 0025 — Typed-output validation and verifier-agent port

- **Status:** Superseded by 0052
- **Date:** 2026-04-27
- **Phase:** 4
- **Relates to:** 0002, 0003, 0011, 0018, 0022

## Context

Agent steps in [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
emit free-form text (or, when the LLM cooperates, JSON inside a text part)
that is then templated directly into the next step's prompt and tool
arguments. There is no boundary check between "step produced an output"
and "the next step or peer agent consumes it". The current shape:

- [`StepResult.output`](../../crates/ork-core/src/models/workflow.rs) is
  `Option<String>` — opaque to the engine.
- The engine has no notion of an expected schema; downstream steps deal
  with malformed inputs by either failing late (template render error,
  tool-arg coercion error) or — worse — by silently propagating garbage
  into a `delegate_to` (ADR [`0006`](0006-peer-delegation.md)) or a
  remote A2A peer (ADR [`0007`](0007-remote-a2a-agent-client.md)).
- LLM tool-calling (ADR [`0011`](0011-native-llm-tool-calling.md))
  validates **tool arguments** against the tool's JSON schema, but not
  the agent's free-form **final message** that becomes the step output.
- There is no retry/repair loop at the orchestration layer. An agent
  that returns a malformed JSON blob fails the whole run (or worse,
  poisons it) instead of being given one chance to fix the output.

Google's *Towards a Science of Scaling Agent Systems* (April 2025) finds
that **verification at step boundaries** — separate from the producer —
is the single largest determinant of whether a multi-agent system
outperforms a single-shot strong model, and that the gain compounds with
graph depth (the regime ADR [`0018`](0018-dag-executor-enhancements.md)
unlocks for ork). Without a verification gate the additional steps in a
deeper DAG amplify error rather than reducing it.

ork therefore needs (a) a deterministic, schema-driven check at every
producer/consumer boundary, and (b) an optional LLM-as-judge verifier
that can score outputs against task-specific rubrics that JSON Schema
cannot express ("did the answer cite the source it was asked to use?").
Both must run in `ork-core` **before** the next dispatch — i.e., before
the next step executes, before a peer is delegated to, and before the
final task message is returned to the A2A caller.

## Decision

ork **introduces a two-stage validation gate** in the workflow engine
that runs after every producing step and before the next dispatch:

1. **Stage 1 — schema check (deterministic, always-on when declared).**
   The step output is parsed and validated against a JSON Schema
   declared on the step. Cheap, no LLM call, no I/O.
2. **Stage 2 — verifier agent (opt-in, LLM-as-judge).** A separate
   agent — implementing the existing [`Agent`](../../crates/ork-core/src/ports/agent.rs)
   port — receives the producer's output plus a rubric and returns a
   structured `VerifierVerdict { pass, score, issues, repair_hint }`.

On failure, the engine runs a **bounded repair loop**: it re-dispatches
the producing step with the failure reason and `repair_hint` injected
into the prompt context, up to a configured retry budget. Exhausting
the budget marks the step `Failed` with a reason of
`validation_exhausted` and a structured `ValidationFailure` payload in
[`StepResult`](../../crates/ork-core/src/models/workflow.rs).

### YAML surface

A new optional `validate:` block on `WorkflowStep`:

```yaml
- id: extract_invoice
  agent: invoice_extractor
  prompt_template: "Extract structured fields from {{input}}"
  validate:
    schema:                                 # stage 1 — JSON Schema (Draft 2020-12)
      type: object
      required: [invoice_number, total_cents, currency]
      properties:
        invoice_number: { type: string, minLength: 1 }
        total_cents:    { type: integer, minimum: 0 }
        currency:       { type: string, enum: [USD, EUR, GBP] }
    verifier:                               # stage 2 — optional verifier agent
      agent: invoice_qa_judge               # AgentRef (id or inline-card per ADR 0007)
      rubric: |
        Pass only if the extracted total matches the figure on the document
        and the currency is consistent with the country code in the address.
      min_score: 0.8                        # 0..1; default 1.0 (binary pass/fail)
    on_failure:
      max_retries: 2                        # default 1
      mode: repair                          # repair | fail | continue
```

`mode` semantics:

- `repair` (default) — re-dispatch the producing step with failure
  context, up to `max_retries`.
- `fail` — mark the step failed immediately on validation failure.
- `continue` — record the failure on the step but proceed downstream
  (used for soft checks like quality scores feeding a metric).

### Rust surface

A new `Verifier` port in `ork-core` and a `ValidationGate` that the
engine invokes after each producing node:

```rust
// crates/ork-core/src/ports/verifier.rs

#[async_trait]
pub trait Verifier: Send + Sync {
    /// Stage 2 check. Implementations typically wrap an `Agent` whose
    /// card declares a `verifier` skill, but the trait is intentionally
    /// independent of `Agent` so non-LLM verifiers (rule engines,
    /// external graders) can plug in.
    async fn verify(
        &self,
        ctx: &VerifierContext,
        candidate: &VerifierInput,
    ) -> Result<VerifierVerdict, OrkError>;
}

pub struct VerifierContext {
    pub tenant_id: TenantId,
    pub task_id: TaskId,
    pub step_id: String,
    pub attempt: u32,
}

pub struct VerifierInput {
    pub output: serde_json::Value,        // post-schema-parse JSON
    pub rubric: String,
    pub min_score: f32,
    pub upstream_inputs: serde_json::Value, // resolved templates the producer saw
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifierVerdict {
    pub pass: bool,
    pub score: f32,                       // 0..=1
    pub issues: Vec<VerifierIssue>,       // structured findings
    pub repair_hint: Option<String>,      // free-form, fed back to producer on retry
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifierIssue {
    pub path: String,                     // JSON Pointer into the output
    pub message: String,
    pub severity: VerifierSeverity,       // Error | Warn | Info
}
```

The deterministic schema check lives next to the port:

```rust
// crates/ork-core/src/workflow/validation.rs

pub struct SchemaCheck { compiled: jsonschema::JSONSchema }

impl SchemaCheck {
    pub fn from_value(schema: &serde_json::Value) -> Result<Self, OrkError>;
    pub fn check(&self, output: &serde_json::Value) -> Result<(), Vec<SchemaIssue>>;
}

pub struct ValidationGate {
    schema: Option<SchemaCheck>,
    verifier: Option<Arc<dyn Verifier>>,
    min_score: f32,
    on_failure: OnFailure,
    max_retries: u32,
}

pub enum GateOutcome {
    Passed { parsed: serde_json::Value },
    Repair { reason: String, hint: Option<String> },
    Failed(ValidationFailure),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationFailure {
    pub stage: ValidationStage,           // Schema | Verifier | Parse
    pub attempts: u32,
    pub schema_issues: Vec<SchemaIssue>,
    pub verifier_verdict: Option<VerifierVerdict>,
}
```

### Engine integration

In [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
the producing-step path becomes:

```text
run_agent(step) ──► raw_output ──► gate.evaluate(raw_output)
                                       │
            ┌──────────────────────────┼──────────────────────────┐
            ▼                          ▼                          ▼
        Passed(parsed)             Repair(reason)             Failed(failure)
            │                          │                          │
   StepResult::Completed     re-dispatch with reason       StepResult::Failed
       output = parsed       injected; attempt += 1        validation = failure
            │                          │
   dispatch downstream         (loop up to max_retries)
```

Repair re-dispatch threads the failure context as a synthetic
`AgentMessage` part with role `validator`:

```json
{
  "kind": "data",
  "data": {
    "previous_output": "...",
    "schema_issues": [{ "path": "/total_cents", "message": "expected integer, got string" }],
    "repair_hint": "Re-extract numeric fields without thousands separators."
  }
}
```

`LocalAgent` (ADR [`0011`](0011-native-llm-tool-calling.md)) renders this
into a system-style turn ahead of the original user prompt. Remote
agents (ADR [`0007`](0007-remote-a2a-agent-client.md)) receive it as an
additional message part on the same task.

The gate runs **before** any of: dispatching the next compiled node,
resolving `delegate_to`, or returning the final A2A task message. This
satisfies the "before the next dispatch" requirement at every fan-out
point introduced by ADR [`0018`](0018-dag-executor-enhancements.md):
`Parallel` branches, `Switch` cases, `Map` body iterations, and `Loop`
bodies all flow through the same gate per producing leaf node.

### Verifier agent shape

The default LLM verifier is an ordinary [`Agent`](../../crates/ork-core/src/ports/agent.rs)
implementation under `crates/ork-agents/src/verifier.rs`:

- Card skill: `verifier` with input mode `data`, output mode `data`.
- Prompt scaffold renders the rubric, candidate, and upstream inputs
  into a fixed structured-output template; uses ADR
  [`0011`](0011-native-llm-tool-calling.md) tool-calling to force a
  `submit_verdict(VerifierVerdict)` call so the response is already
  schema-conformant.
- Provider/model resolved through ADR [`0012`](0012-multi-llm-providers.md);
  default in [`config/default.toml`](../../config/default.toml) is
  `[validation.verifier.default_model]`. Operators can pin a cheaper
  model for verification than for production.

Non-LLM `Verifier` implementations are explicitly supported (rule
engines, deterministic graders, external HTTP graders behind Kong) —
the trait does not require an `Agent`.

### Persistence

[`StepResult`](../../crates/ork-core/src/models/workflow.rs) gains an
optional `validation: Option<ValidationOutcome>` describing the gate's
final state — pass, repair count, or failure payload. Existing rows
deserialise with `validation: None`.

### Limits and config

Added to [`config/default.toml`](../../config/default.toml):

| Setting | Default | Purpose |
| ------- | ------- | ------- |
| `validation.schema.enabled` | `true` | Master switch for stage 1 |
| `validation.verifier.enabled` | `true` | Master switch for stage 2 |
| `validation.verifier.default_model` | unset | Verifier model when step omits one |
| `validation.repair.max_retries_default` | `1` | Used when step omits `on_failure.max_retries` |
| `validation.repair.global_budget_per_run` | `8` | Workflow-wide cap; prevents runaway repair on flaky LLMs |
| `validation.verifier.timeout_ms` | `15000` | Per-`verify` call timeout |

### Telemetry

ADR [`0022`](0022-observability.md) gains a span and metrics:

- Span `validation.gate` with attributes `step_id`, `stage`, `attempt`,
  `outcome`, `schema_issue_count`, `verifier_score`.
- Metrics: `ork_validation_attempts_total{stage,outcome}`,
  `ork_validation_repair_total{step_kind}`,
  `ork_verifier_duration_seconds`.

## Acceptance criteria

- [ ] Trait `Verifier` defined at `crates/ork-core/src/ports/verifier.rs`
      with the signature shown in `Decision`, exported from
      `crates/ork-core/src/ports/mod.rs`.
- [ ] Types `VerifierContext`, `VerifierInput`, `VerifierVerdict`,
      `VerifierIssue`, `VerifierSeverity`, `ValidationFailure`,
      `ValidationOutcome`, `ValidationStage`, `OnFailure`,
      `SchemaIssue` live in `crates/ork-core/src/workflow/validation.rs`.
- [ ] `SchemaCheck` compiles JSON Schema Draft 2020-12 via the
      `jsonschema` crate; misconfigured schemas fail at workflow-compile
      time (not at run time).
- [ ] `WorkflowStep` gains an optional `validate: Option<ValidateSpec>`
      field; existing YAML without `validate:` parses unchanged.
- [ ] [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs)
      runs the gate after every producing leaf node and before any
      downstream dispatch (next node, `delegate_to`, final A2A message).
- [ ] Repair re-dispatch injects a `data` part with `previous_output`,
      `schema_issues`, and `repair_hint`; verified by integration test.
- [ ] [`StepResult`](../../crates/ork-core/src/models/workflow.rs) gains
      an additive `validation: Option<ValidationOutcome>` field; old
      rows deserialise without migration.
- [ ] Default LLM verifier agent at
      `crates/ork-agents/src/verifier.rs` registers with id
      `ork.verifier.default` and uses ADR 0011 tool-calling to force
      `submit_verdict` structured output.
- [ ] Config keys under `[validation]` in
      [`config/default.toml`](../../config/default.toml) match the
      table in `Decision`.
- [ ] Integration test
      `crates/ork-core/tests/validation_repair_smoke.rs` covers:
      schema pass, schema fail → repair → pass, schema fail → repair →
      exhausted, verifier fail with `mode: continue`, global repair
      budget exhaustion across a multi-step workflow.
- [ ] Integration test
      `crates/ork-core/tests/validation_gate_dispatch.rs` proves the
      gate fires before `Parallel` / `Switch` / `Map` / `Loop`
      dispatches (one assertion per ADR-0018 step kind).
- [ ] `cargo test -p ork-core validation::` is green.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended (see
      [`METRICS.md`](METRICS.md)).

## Consequences

### Positive

- Boundary errors are caught locally instead of cascading. Deeper DAGs
  (ADR [`0018`](0018-dag-executor-enhancements.md)) become safer to
  build.
- Step outputs gain a typed contract that downstream steps, peers, and
  external A2A consumers can rely on — composable across the mesh.
- LLM-as-judge becomes a first-class, observable feature rather than a
  bespoke prompt trick. Verifier model can be cheaper than producer
  model, improving cost-quality tradeoff.
- Repair is bounded and metered (per-step `max_retries` and per-run
  `global_budget_per_run`); failures are structured, debuggable, and
  show up in [`0022`](0022-observability.md) traces.
- Verifier port is hexagonal and substrate-neutral — non-LLM graders
  (regex, schema-of-schemas, external HTTP) plug in without touching
  the engine.

### Negative / costs

- Every validated step adds one schema check (cheap) and, when
  enabled, one verifier-agent call (an extra LLM round-trip). Workflows
  with `verifier` on every step double their LLM cost on the happy
  path. Mitigation: stage 2 is opt-in per step; default verifier model
  defaults to a smaller model.
- Repair loops can mask producer regressions. Mitigation:
  `ork_validation_repair_total` is a first-class metric so a sudden
  rise is visible; the global per-run budget caps blast radius.
- Schema authoring is a new burden on workflow authors. Mitigation: a
  follow-up ADR may add schema inference from sample outputs; for now,
  steps without `validate:` behave exactly as today.
- The verifier itself can be wrong — an LLM judge is not a ground
  truth. We treat its verdict as a signal, not a proof; `min_score`
  defaults to a binary pass/fail to make the decision boundary
  explicit.
- `mode: continue` produces step results that are simultaneously
  "completed" and "had validation issues" — UI surfaces (ADR
  [`0017`](0017-webui-chat-client.md)) need a third state. Documented
  as a follow-up.

### Neutral / follow-ups

- A future ADR may add **schema inference** from a few sample outputs
  to lower the authoring barrier.
- A future ADR may add a **verifier ensemble** (multiple judges, voting
  on disagreement) for high-stakes steps.
- A future ADR may extend validation to **streaming partials** —
  currently the gate runs only on the terminal `AgentEvent::Message`
  per ADR [`0002`](0002-agent-port.md).
- Tenant security (ADR [`0020`](0020-tenant-security-and-trust.md)):
  the verifier agent runs under the same tenant as the producer; no
  new trust boundary is introduced. RBAC scopes
  ([`0021`](0021-rbac-scopes.md)) for invoking a verifier agent are
  inherited from the calling agent.

## Alternatives considered

- **Schema-only, no verifier agent.** Rejected: schemas catch shape
  bugs but not semantic ones (citation correctness, factual grounding,
  rubric adherence) — the precise failure modes Google's paper found
  to dominate at depth.
- **Verifier-agent only, no schema check.** Rejected: every consumer
  pays for an LLM call to detect problems a one-line JSON Schema would
  catch deterministically and for free. Two stages compose: schema
  rejects shape errors before the verifier ever sees them.
- **Push validation into the producing agent.** Rejected: violates
  separation of producer and judge — the *whole point* of the cited
  research is that an independent verifier is what makes the system
  scale. Self-validation is also vulnerable to the same blind spots
  that produced the bad output.
- **Validate only the final task message, not intermediate steps.**
  Rejected: errors detected only at the leaf are expensive (the whole
  DAG has already run) and lose the local context needed to repair
  cheaply. Repair at the producer is far cheaper than re-running
  downstream.
- **Use OpenAI-style `response_format: json_schema` natively and skip
  the gate.** Rejected: provider-specific, not all providers in ADR
  [`0012`](0012-multi-llm-providers.md) support it, and it covers only
  shape — not the semantic verifier. We *can* still set
  `response_format` on supporting providers as a producer-side
  optimisation; the gate remains the contract.
- **Implement repair as a workflow-level `Loop` (ADR
  [`0018`](0018-dag-executor-enhancements.md)) authored per step.**
  Rejected: every workflow author would re-implement the same
  validation+repair pattern with subtly different bugs. Centralising
  in the engine is the right level.
- **Externalise validation to a service behind Kong.** Rejected for
  the default path (latency, blast-radius on outage); supported as a
  pluggable `Verifier` impl for teams that already have a grader
  service.

## Affected ork modules

- [`crates/ork-core/src/ports/verifier.rs`](../../crates/ork-core/src/ports/) —
  new `Verifier` trait and value types.
- [`crates/ork-core/src/ports/mod.rs`](../../crates/ork-core/src/ports/mod.rs) —
  re-export the new module.
- [`crates/ork-core/src/workflow/validation.rs`](../../crates/ork-core/src/workflow/) —
  `SchemaCheck`, `ValidationGate`, `ValidationFailure`,
  `ValidationOutcome`, `OnFailure`, `ValidationStage`.
- [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs) —
  invoke the gate, drive the repair loop, account for the global
  per-run budget.
- [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs) —
  compile `validate:` blocks (including JSON Schema compile) at
  workflow-load time.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) —
  `WorkflowStep::validate`, `ValidateSpec`, `StepResult.validation`.
- [`crates/ork-agents/src/verifier.rs`](../../crates/ork-agents/) —
  default `LocalAgent`-backed verifier registered as
  `ork.verifier.default`.
- [`crates/ork-persistence/src/postgres/workflow_repo.rs`](../../crates/ork-persistence/src/postgres/workflow_repo.rs) —
  read/write the additive `validation` field on `step_results`.
- [`config/default.toml`](../../config/default.toml) —
  `[validation]` section.
- [`workflow-templates/`](../../workflow-templates/) — example
  validated workflow under `workflow-templates/validated-extraction.yaml`.
- New dependency: `jsonschema` crate (Apache-2.0/MIT). No new
  hexagonal-boundary violations — the crate is used inside the engine
  on serde JSON values.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff. Leave empty until the implementation lands.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Google Research, *Towards a Science of Scaling Agent Systems* | <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/> | Two-stage gate; verifier agent independent of producer |
| OpenAI Structured Outputs | `response_format: json_schema` | Stage 1 (`SchemaCheck`) — provider-agnostic, applied post-hoc |
| LangGraph `add_validator` / `add_judge` | LangGraph node hooks | `ValidationGate` invoked by the engine, not by node authors |
| ADK `OutputParser` + retry | Google ADK | `OnFailure::Repair` with bounded retries and structured failure context |
| SAM monitor hooks | [`agent/utils/monitors.py`](https://github.com/SolaceLabs/solace-agent-mesh/blob/main/src/solace_agent_mesh/agent/utils/monitors.py) | Stage 2 verifier as a typed port (Rust trait) instead of an open-ended hook |

## Open questions

- Does the verifier see **upstream inputs** verbatim, or a redacted
  view? Stance: verbatim by default; ADR
  [`0020`](0020-tenant-security-and-trust.md) RBAC may scope this
  later. Open.
- Should `min_score` failures with `mode: continue` count toward the
  global repair budget? Stance: no — they are not repair attempts.
  Confirm in implementation review.
- How are validation failures surfaced over A2A SSE to external
  callers? Stance: as `AgentEvent::StatusUpdate` with a structured
  `validation_failure` annotation; the final task state is `Failed`
  with reason `validation_exhausted`. Wire format owned by ADR
  [`0008`](0008-a2a-server-endpoints.md); confirm before flipping to
  `Accepted`.
- Should we support **streaming partial validation** (validate each
  intermediate `AgentEvent::Message` as it arrives)? Defer; the gate
  runs on the terminal message only.
- Should the engine emit a synthetic `WorkflowEvent::Repair` for the
  Web UI (ADR [`0017`](0017-webui-chat-client.md))? Likely yes; deferred
  to ADR [`0022`](0022-observability.md) follow-up.

## References

- Google Research blog: <https://research.google/blog/towards-a-science-of-scaling-agent-systems-when-and-why-agent-systems-work/>
- JSON Schema 2020-12: <https://json-schema.org/draft/2020-12>
- `jsonschema` crate: <https://crates.io/crates/jsonschema>
- Related ADRs: [`0002`](0002-agent-port.md), [`0011`](0011-native-llm-tool-calling.md),
  [`0018`](0018-dag-executor-enhancements.md),
  [`0022`](0022-observability.md)
