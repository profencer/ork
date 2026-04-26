# 0027 — Human-in-the-loop: approval steps and input requests

- **Status:** Proposed
- **Date:** 2026-04-27
- **Deciders:** ork core team
- **Phase:** 4
- **Relates to:** 0003, 0008, 0009, 0017, 0018, 0019, 0021, 0022, 0025
- **Supersedes:** —

## Context

A2A 1.0 already names two non-terminal task states that imply human
involvement: [`TaskState::InputRequired` and `TaskState::AuthRequired`](../../crates/ork-a2a/src/types.rs)
(line 309). The serialization layer carries them through to the push
outbox ([`crates/ork-push/src/outbox.rs`](../../crates/ork-push/src/outbox.rs))
and the embed status handler observes the transitions
([`crates/ork-core/src/embeds/handlers/status_update.rs`](../../crates/ork-core/src/embeds/handlers/status_update.rs)),
but the **execution path is incomplete**:

- The workflow engine ([`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs))
  has no step kind that explicitly waits for a human, no resume gate,
  and no notion of an "input request" record. `StepStatus` has only
  `Pending | Running | Completed | Failed` ([`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs)).
- The A2A endpoints ([`crates/ork-api/src/routes/a2a.rs`](../../crates/ork-api/src/routes/a2a.rs))
  accept follow-up `message/send` calls in principle, but there is no
  schema attached to the pause point and no enforcement that the next
  message must satisfy what the agent asked for.
- The Web UI ([`crates/ork-webui/`](../../crates/ork-webui/)) renders
  free-text chat and SSE streams, but has no form-rendering primitive
  for "the workflow is paused on a typed approval/input request and the
  caller must submit `{schema}` to resume."
- The verifier port (ADR [0025](0025-typed-output-validation-and-verifier-agent.md))
  retries an agent on failure — it has no escape hatch to a human when
  the model can't satisfy the schema after `N` repairs.
- ADR [0006](0006-peer-delegation.md) and [0018](0018-dag-executor-enhancements.md)
  treat agents as the only callers; "the human" is not a peer.

The result: ork can *enter* `InputRequired`, but the run effectively
stalls there. This ADR closes the loop by making "ask a human" a
first-class workflow primitive and an A2A-conformant resume protocol.

## Decision

ork **introduces** a human-in-the-loop (HITL) surface that consists of:

1. A new `HumanInputGate` port in `ork-core` that the engine uses to
   record, await and resolve a request.
2. A new workflow step kind `human_input` that pauses the run, persists
   a typed request, transitions the run + parent A2A task to
   `InputRequired`, and resumes when the request is resolved (or
   cancels/fails on timeout).
3. A new persistence table `human_input_requests`.
4. A2A-native resume: callers (Web UI, CLI, peer agents, external A2A
   clients) resume by sending `message/send` with the existing
   `taskId`, carrying a `DataPart` whose payload validates against the
   request's published `input_schema`.
5. A Web UI affordance that renders the schema as a form, gated by RBAC
   scope, and submits the resolution through the existing
   `/webui/api/conversations/{id}/messages` route.

### `HumanInputGate` port

```rust
// crates/ork-core/src/ports/human_input.rs

#[async_trait::async_trait]
pub trait HumanInputGate: Send + Sync {
    async fn open(
        &self,
        req: NewHumanInputRequest,
    ) -> Result<HumanInputRequest, HumanInputError>;

    async fn resolve(
        &self,
        request_id: HumanInputRequestId,
        decision: HumanInputDecision,
        actor: ResolverIdentity,
    ) -> Result<HumanInputRequest, HumanInputError>;

    async fn get(
        &self,
        request_id: HumanInputRequestId,
    ) -> Result<Option<HumanInputRequest>, HumanInputError>;
}

pub struct NewHumanInputRequest {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub kind: HumanInputKind,           // Approval | Input | Choice
    pub prompt: String,
    pub input_schema: serde_json::Value, // JSON Schema (Draft 2020-12)
    pub required_scopes: Vec<Scope>,    // ADR 0021
    pub expires_at: Option<DateTime<Utc>>,
    pub on_timeout: TimeoutPolicy,      // Fail | Default(value) | Escalate
}

pub enum HumanInputDecision {
    Approve { data: serde_json::Value },
    Reject { reason: String },
    Edit    { data: serde_json::Value }, // accept-with-modifications
}
```

The default implementation lives in `ork-persistence` and persists to
Postgres; a Redis-pubsub fan-out wakes the engine without polling.

### Workflow step kind

```yaml
- id: legal-review
  kind: human_input
  prompt: "Approve outbound contract draft"
  input_schema:
    type: object
    properties:
      decision: { type: string, enum: [approve, reject] }
      notes: { type: string }
    required: [decision]
  required_scopes: ["workflow:approve:legal"]
  timeout: 24h
  on_timeout: fail
```

The compiler ([`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs))
emits a `Step::HumanInput`. The engine, on hitting that step:

1. Calls `HumanInputGate::open`, receives `request_id`.
2. Marks the step `Running` with a `pending_human_input = request_id`
   marker, and the run + parent A2A task `InputRequired`.
3. Emits an A2A status update whose `Message` includes a `DataPart`
   with `{"kind": "human_input_request", "request_id", "input_schema",
   "prompt"}` so SSE consumers (Web UI, peers) can render it.
4. Yields. Resume is event-driven: a Redis pubsub channel
   `ork.hitl.<request_id>` carries the resolution, the engine reloads
   the run and continues with the decision available as the step's
   output.

### A2A resume contract

When a task is in `InputRequired`, a follow-up `message/send` with the
existing `taskId` and a `DataPart` matching the published `input_schema`
**MUST** be treated as a `HumanInputDecision::Approve { data }`.
A `DataPart` with `{"kind": "rejection", "reason": ...}` maps to
`Reject`. The A2A route ([`crates/ork-api/src/routes/a2a.rs`](../../crates/ork-api/src/routes/a2a.rs))
validates the body against the active request's schema before calling
`HumanInputGate::resolve`. RBAC scopes (ADR [0021](0021-rbac-scopes.md))
are checked at the gateway; the resolver identity is recorded for
audit (ADR [0022](0022-observability.md)).

### Timeout / scheduling

Timeouts are enforced via the scheduler from ADR [0019](0019-scheduled-tasks.md):
when `expires_at` fires, the scheduler calls `HumanInputGate::resolve`
with the configured `on_timeout` policy.

### Push notifications

The push outbox already fires on state transitions (ADR [0009](0009-push-notifications.md)).
Entering `InputRequired` will now carry the `human_input_request`
payload, so external integrations (Slack approvers, email, paging) can
build their own UI on top of the same JWS-signed callbacks.

## Acceptance criteria

- [ ] Trait `HumanInputGate` defined at
      [`crates/ork-core/src/ports/human_input.rs`](../../crates/ork-core/src/ports/human_input.rs)
      with the signature shown in `Decision`.
- [ ] Types `NewHumanInputRequest`, `HumanInputRequest`,
      `HumanInputDecision`, `HumanInputKind`, `TimeoutPolicy` and
      `HumanInputError` exported from `ork-core`.
- [ ] Postgres migration `migrations/009_human_input_requests.sql`
      creates `human_input_requests(id, task_id, run_id, step_id, kind,
      prompt, input_schema jsonb, required_scopes text[], status,
      decision jsonb, resolved_by, resolved_at, expires_at, created_at)`
      with index on `(task_id, status)`.
- [ ] `PostgresHumanInputGate` implements the port at
      `crates/ork-persistence/src/postgres/human_input_gate.rs` and
      publishes resolutions on Redis channel `ork.hitl.<request_id>`.
- [ ] Workflow step kind `HumanInput` parsed by
      [`crates/ork-core/src/workflow/compiler.rs`](../../crates/ork-core/src/workflow/compiler.rs)
      and dispatched by
      [`crates/ork-core/src/workflow/engine.rs`](../../crates/ork-core/src/workflow/engine.rs).
- [ ] On entering the step the engine sets `WorkflowRunStatus::InputRequired`
      and emits an A2A status update whose message contains a
      `DataPart` with `kind = "human_input_request"`.
- [ ] A2A route validates a follow-up `message/send` carrying a typed
      `DataPart` against the request's `input_schema` (JSON Schema
      Draft 2020-12) and calls `HumanInputGate::resolve` on success.
      Validation failure returns A2A error code `-32602` with the
      schema path that failed.
- [ ] RBAC check: resolver must hold every scope in
      `required_scopes`; rejected resolutions return `-32001`
      `unauthorized`.
- [ ] Web UI renders a form for an active `human_input_request`
      message in `crates/ork-webui/` and submits it through the
      existing conversations/messages endpoint. Submit is disabled when
      the user lacks the required scopes.
- [ ] Integration test
      `crates/ork-core/tests/human_input_smoke.rs::approve_resume`
      drives a workflow `step1 → human_input → step3`, asserts the run
      pauses, calls `resolve(Approve)`, asserts step3 receives the
      submitted data and the run completes.
- [ ] Integration test `::reject_fails_run` asserts a `Reject`
      decision marks the step `Failed` with the reason.
- [ ] Integration test `::timeout_default` asserts `on_timeout =
      Default(v)` resumes with `v` after `expires_at`.
- [ ] Push outbox emits a `human_input_request` payload on entering
      `InputRequired`, signed per ADR [0009](0009-push-notifications.md).
- [ ] [`README.md`](README.md) ADR index row added for 0027.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- Closes a real gap: `InputRequired` is reachable today but
  effectively a dead end. Workflows that need approval (legal review,
  budget sign-off, "yes, send the email") become expressible without
  shelling out to a custom gateway.
- Reuses A2A's existing message-send semantics for resume — no new
  wire method, no new transport, callers that already speak A2A get
  HITL for free.
- Composes with verifier (ADR [0025](0025-typed-output-validation-and-verifier-agent.md)):
  on terminal validation failure a workflow can fall back to
  `human_input` instead of failing the run.
- Composes with RBAC (ADR [0021](0021-rbac-scopes.md)) and audit
  (ADR [0022](0022-observability.md)) without inventing parallel
  authorization or logging surfaces.

### Negative / costs

- Adds a long-lived state to the engine. Crash-recovery semantics must
  be tested: the engine must rebuild pending requests from
  `human_input_requests` on startup rather than from in-memory state.
- Introduces a new failure mode at gateways: a resolver that is
  authenticated but lacks scopes will see `-32001` from `message/send`
  on a task that they could otherwise stream — gateway error surfaces
  must not leak the schema if the caller can't see the task.
- The `Edit` decision (accept-with-modifications) lets a human alter
  the data the next step sees. This is the desired feature, but it
  also means a human can inject values the agent never produced;
  downstream type-validation (ADR [0025](0025-typed-output-validation-and-verifier-agent.md))
  is the only check.
- Push deliveries on entering `InputRequired` may carry sensitive
  prompts to external endpoints. The signing scheme already covers
  authenticity, but operators must opt in per webhook to receive
  schemas/prompts in the body.

### Neutral / follow-ups

- A future ADR may extend `HumanInputKind` with `Choice` (multi-option
  with deterministic shortcuts) or attach reusable form templates to
  agent cards.
- A "delegate to a Slack channel" gateway is out of scope; it would be
  a thin consumer of the push payload introduced here plus a
  `message/send` on resolution.

## Alternatives considered

- **A bespoke `/approvals` REST surface.** Rejected: duplicates A2A's
  message-send flow, requires a second auth path, and makes peer
  agents that already speak A2A second-class HITL participants.
- **Treat the human as a remote A2A agent (ADR [0007](0007-remote-a2a-agent-client.md))**
  invoked through `agent_call`. Rejected: works for approve-or-reject
  but loses the typed `input_schema` and the "this run is paused"
  semantics. The engine would have to model a synthetic agent for
  every approver, and timeouts would have to be re-derived from
  request_timeout. Pause-as-a-step is a better fit for the existing
  workflow model.
- **Embed approval in the verifier port (ADR [0025](0025-typed-output-validation-and-verifier-agent.md))**
  by making one of the verifier strategies "ask a human". Rejected:
  conflates two concerns — `0025` is about *correcting* model output
  against a schema; HITL is about *deciding* whether to proceed.
  Verifier may *call* HITL on terminal failure (this composition is
  preserved), but it should not subsume it.
- **Block on synchronous SSE.** Rejected: workflows can pause for
  hours or days; tying resume to an open SSE connection couples
  liveness to a transport that is allowed to drop and reconnect with
  `Last-Event-ID` ([`crates/ork-api/src/sse_buffer.rs`](../../crates/ork-api/src/sse_buffer.rs)).

## Affected ork modules

- [`crates/ork-core/src/ports/`](../../crates/ork-core/src/ports/) —
  new `human_input.rs` port and types.
- [`crates/ork-core/src/workflow/`](../../crates/ork-core/src/workflow/) —
  new `Step::HumanInput` kind, compiler + engine support, resume
  semantics.
- [`crates/ork-core/src/models/workflow.rs`](../../crates/ork-core/src/models/workflow.rs) —
  add `StepStatus::AwaitingInput` (or carry the marker on
  `Running`; see Open questions).
- [`crates/ork-persistence/`](../../crates/ork-persistence/) —
  `human_input_requests` table + `PostgresHumanInputGate`.
- [`migrations/009_human_input_requests.sql`](../../migrations/) — new
  migration.
- [`crates/ork-api/src/routes/a2a.rs`](../../crates/ork-api/src/routes/a2a.rs) —
  schema-validate follow-up messages on `InputRequired` tasks; map to
  `HumanInputGate::resolve`.
- [`crates/ork-push/src/outbox.rs`](../../crates/ork-push/src/outbox.rs) —
  include `human_input_request` payload on transition.
- [`crates/ork-webui/`](../../crates/ork-webui/) — render
  schema-driven form for active requests; gate submit by scope.
- [`crates/ork-cli/`](../../crates/ork-cli/) — `ork hitl approve` /
  `ork hitl list` subcommands.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff (see [`AGENTS.md`](../../AGENTS.md) §3, step 3).

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| LangGraph | `interrupt()` + `Command(resume=...)` checkpointing pattern | `Step::HumanInput` + `HumanInputGate::resolve` |
| A2A 1.0 | `TaskState::input-required` + follow-up `message/send` with same `taskId` | Native resume contract — no new wire method |
| Solace Agent Mesh | No first-class HITL primitive; approvers are bolted on via custom gateway components | Replaced by typed `human_input` step + RBAC-gated A2A resume |
| Temporal | Signals + `await_signal` activities | `HumanInputGate` open/resolve plus Redis-pubsub wakeup |

## Open questions

- Should `StepStatus` gain a new variant `AwaitingInput`, or should
  the marker stay on `Running` with a `pending_human_input` field?
  The former is more legible in dashboards; the latter avoids a
  migration of any existing serialized step state. Lean toward the
  new variant; deferred to implementation.
- Does the `Edit` decision require re-running the verifier (ADR
  [0025](0025-typed-output-validation-and-verifier-agent.md)) on the
  modified data? Default proposal: yes, run schema validation on
  `Edit` payloads but skip the verifier-agent stage (the human is the
  verifier).
- Should the request payload in the push notification be redactable
  per webhook (e.g. send only the `prompt` and a deep link, not the
  full `input_schema`)? Likely yes for sensitive deployments; a flag
  on the push config is the smallest viable shape.

## References

- A2A 1.0 task lifecycle: <https://a2a-protocol.org/latest/specification/#task-lifecycle>
- JSON Schema Draft 2020-12: <https://json-schema.org/draft/2020-12/release-notes>
- LangGraph human-in-the-loop: <https://langchain-ai.github.io/langgraph/concepts/human_in_the_loop/>
- Related ADRs: [0003](0003-a2a-protocol-model.md),
  [0008](0008-a2a-server-endpoints.md),
  [0009](0009-push-notifications.md),
  [0017](0017-webui-chat-client.md),
  [0018](0018-dag-executor-enhancements.md),
  [0019](0019-scheduled-tasks.md),
  [0021](0021-rbac-scopes.md),
  [0022](0022-observability.md),
  [0025](0025-typed-output-validation-and-verifier-agent.md).
