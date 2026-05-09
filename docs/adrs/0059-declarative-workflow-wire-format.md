# 0059 — Declarative workflow wire format, dynamic registration, and Rust loader

- **Status:** Proposed
- **Date:** 2026-05-09
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0006, 0010, 0011, 0019, 0020, 0021, 0048, 0049, 0050, 0051, 0052, 0056, 0058
- **Supersedes:** —

## Context

ADR [`0050`](0050-code-first-workflow-dsl.md) made workflows
typed Rust values built with `workflow(id).then(step).commit()`. The
compiled graph lives in
[`crates/ork-workflow/src/program.rs`](../../crates/ork-workflow/src/program.rs)
as a `ProgramOp` enum (`Step | Map | Branch | Parallel | DoUntil |
DoWhile | ForEach`) and the engine is at
[`crates/ork-workflow/src/engine.rs`](../../crates/ork-workflow/src/engine.rs).
That ADR also kept the YAML compatibility shim
([`crates/ork-workflow/src/yaml_compat.rs`](../../crates/ork-workflow/src/yaml_compat.rs))
so legacy
[`workflow-templates/`](../../workflow-templates/) load into the
same `Workflow` value.

Two needs are not addressed by [`0050`](0050-code-first-workflow-dsl.md):

1. **Tenants want to ship workflows without compiling Rust.** The
   self-serve SaaS shape from ADR
   [`0058`](0058-per-tenant-orkapp.md)'s `CatalogTenantResolver`
   accepts per-tenant config but has no schema for "tenant-supplied
   workflow definitions". Today they either edit `main.rs` and
   redeploy, or hand-author YAML against the legacy shim — which
   is execution-only (no `Trigger` from
   [`0019`](0019-scheduled-tasks.md), no typed I/O, no schema
   round-trip with the code-first DSL).
2. **Polyglot authoring needs a wire contract.** Future SDKs
   (TypeScript, Go) — covered by ADR
   [`0060`](0060-ork-sdk-generate-and-reference-sdks.md) —
   need a single canonical document to emit. Three hand-rolled
   serialisers in three languages would drift; a JSON Schema with
   one Rust loader is the truth.

This ADR introduces the **canonical workflow wire format**
(`WorkflowSpec` v1), the validating loader that turns a spec into
the same `Workflow` value [`0050`](0050-code-first-workflow-dsl.md)
produces, and the tenant-scoped upload endpoint that registers
specs at runtime against [`0058`](0058-per-tenant-orkapp.md)'s
resolver.

It does **not** introduce arbitrary user code execution. Step
bodies are restricted to a closed set of kinds — `agent_call`,
`tool_call`, `transform`, `suspend` — whose leaves resolve against
the registered catalog at load time. Anything more programmatic is
either (a) a custom Rust tool the operator publishes, (b) a custom
agent the operator publishes, or (c) future-WASM step bodies (a
separate ADR if and when needed). This preserves the
[`0048`](0048-pivot-to-code-first-rig-platform.md) pivot's
type-safety promise: every leaf in a `WorkflowSpec` is checked
against a typed Rust component the operator deployed.

## Decision

ork **introduces `WorkflowSpec` as the canonical workflow wire
format**, a JSON-Schema-versioned document that the new
`crates/ork-workflow/src/loader.rs` compiles into the same
`Workflow` value the code-first builder produces. ork **adds a
tenant-scoped upload surface** at `POST /api/workflows` that
persists specs in Postgres under RLS
([`0020`](0020-tenant-security-and-trust.md)), registers them
through ADR [`0058`](0058-per-tenant-orkapp.md)'s
`CatalogTenantResolver`, and exposes them via the auto-generated
REST surface ([`0056`](0056-auto-generated-rest-and-sse-surface.md)).

The code-first DSL ([`0050`](0050-code-first-workflow-dsl.md))
stays the canonical *Rust* authoring surface. The wire format is a
strict, declarative subset of what the code-first DSL can express;
code-first workflows that fall inside that subset round-trip
through `Workflow::to_spec()`. Workflows that use arbitrary Rust
closures in step bodies do not round-trip (intentionally, per the
hard invariants in §3 of [`AGENTS.md`](../../AGENTS.md)).

### `WorkflowSpec` v1 — JSON shape

The top-level document is a `serde`-tagged versioned envelope. The
loader rejects unknown variants with `OrkError::Validation`.

```json
{
  "v": "V1",
  "id": "ticket-triage",
  "description": "Classify and route a support ticket.",
  "input_schema":  { /* JSON Schema (draft 2020-12) */ },
  "output_schema": { /* JSON Schema (draft 2020-12) */ },
  "retry":   { "max_attempts": 3, "backoff": { "kind": "exponential",
              "initial_ms": 200, "multiplier": 2.0, "jitter": 0.2,
              "max_ms": 30000 } },
  "timeout_ms": 60000,
  "trigger": { "kind": "cron", "expr": "0 0 * * *", "tz": "UTC" },
  "graph": [
    { "kind": "step",
      "id":   "classify",
      "body": { "kind": "agent_call",
                "agent": "classifier",
                "input_expr": "{ \"text\": input.summary }",
                "output_schema": { /* optional override */ } } },

    { "kind": "branch",
      "id":   "by-severity",
      "arms": [
        { "when": "step.classify.output.severity == \"high\"",
          "graph": [
            { "kind": "step", "id": "page",
              "body": { "kind": "tool_call",
                        "tool": "pagerduty.fire",
                        "input_expr": "{ \"summary\": input.summary }" } } ] },
        { "when": "true",
          "graph": [
            { "kind": "step", "id": "queue",
              "body": { "kind": "tool_call",
                        "tool": "jira.create_issue",
                        "input_expr": "{ \"title\": input.summary }" } } ] } ] },

    { "kind": "step",
      "id":   "ack",
      "body": { "kind": "suspend",
                "payload_expr": "{ \"awaiting\": \"customer-ack\" }",
                "resume_schema": { /* JSON Schema for resume payload */ } } }
  ]
}
```

The graph is an ordered list of nodes; node kinds are exactly the
ones [`0050`](0050-code-first-workflow-dsl.md)'s `ProgramOp`
already supports:

| Node `kind` | Maps to `ProgramOp` | Body / sub-graph |
| ----------- | ------------------- | ---------------- |
| `step`      | `Step`              | `body: StepBody` (closed set) |
| `map`       | `Map`               | `expr: <CEL>` (transform on the running value) |
| `branch`    | `Branch`            | `arms: [{ when: <CEL>, graph: [...] }]` |
| `parallel`  | `Parallel`          | `arms: [{ graph: [...] }]` |
| `dountil`   | `DoUntil`           | `graph: [...], until: <CEL>` |
| `dowhile`   | `DoWhile`           | `graph: [...], while: <CEL>` |
| `foreach`   | `ForEach`           | `step: <inline step>, concurrency, items_expr: <CEL>` |

`StepBody` is a closed enum tagged on `kind`:

| `body.kind` | Required fields | Semantics |
| ----------- | --------------- | --------- |
| `agent_call` | `agent`, `input_expr` | Resolves `agent` against the tenant's catalog ([`0058`](0058-per-tenant-orkapp.md) overlay); evaluates `input_expr` against the step's input; returns the agent's structured output. |
| `tool_call`  | `tool`,  `input_expr` | Resolves `tool` against the catalog ([`0010`](0010-mcp-tool-plane.md) MCP tools or [`0051`](0051-code-first-tool-dsl.md) native tools); same evaluation semantics. |
| `transform`  | `expr`               | Pure CEL expression over the step's input; no I/O. |
| `suspend`    | `payload_expr`, `resume_schema` | Returns `StepOutcome::Suspend`; the engine writes a snapshot per [`0050`](0050-code-first-workflow-dsl.md). |

Anything not in this list is deliberately absent from v1.

### Expression language: CEL

Transforms, branch predicates, and `dountil`/`dowhile` conditions
use [Common Expression Language (CEL)](https://github.com/google/cel-spec)
via the `cel-interpreter` Rust crate. CEL is:

- **Bounded** — no recursion, no unbounded loops, no I/O.
- **Sandboxed** — no host-resource access; only the bindings the
  loader injects (`input`, `step.<id>.output`, `run`, `tenant`).
- **Battle-tested** — used in Envoy, Kubernetes admission, GCP IAM.

The evaluator is wrapped by an `ExprEnv` type in
`crates/ork-workflow/src/expr.rs` that injects context bindings and
caps expression evaluation at:

- **Memory:** 1 MiB intermediate per evaluation (default;
  configurable via `[workflow.expr]` in
  [`config/default.toml`](../../config/default.toml)).
- **Wall time:** 100 ms per evaluation.
- **Output size:** 256 KiB JSON.

### Loader — `WorkflowSpec` → `Workflow`

```rust
// crates/ork-workflow/src/loader.rs
use crate::{Workflow, ProgramOp};
use crate::types::{RetryPolicy, ForEachOptions};
use ork_app::OrkApp;
use ork_common::OrkError;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "v")]
pub enum WorkflowSpec {
    V1(WorkflowSpecV1),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSpecV1 {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema:  schemars::schema::RootSchema,
    pub output_schema: schemars::schema::RootSchema,
    #[serde(default)]
    pub retry:   Option<RetryPolicy>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub trigger: Option<TriggerSpec>,
    pub graph: Vec<GraphNode>,
}

pub struct LoadOptions<'a> {
    /// Catalog the leaves resolve against. Typically `OrkApp::manifest()`
    /// for the tenant returned by ADR 0058's resolver.
    pub catalog: &'a CatalogView<'a>,
    /// Strictness — `Strict` (default) rejects unknown agents/tools;
    /// `Lenient` warns and substitutes a stub that fails at run time.
    /// `Lenient` exists only for migration; Strict is the production mode.
    pub strict: Strictness,
}

impl Workflow {
    /// Compile a spec into a registered-ready `Workflow`. Validates:
    /// (a) leaf refs resolve in the catalog,
    /// (b) input/output schemas match each step's neighbours,
    /// (c) CEL expressions parse and type-check against the binding shape,
    /// (d) graph is a DAG (no cycles outside the explicit `dountil`/`dowhile`),
    /// (e) suspend points carry well-formed `resume_schema`.
    pub fn from_spec(spec: &WorkflowSpec, opts: &LoadOptions)
        -> Result<Self, OrkError>;

    /// Inverse direction: produce a spec from a code-first `Workflow`,
    /// when the workflow is "declarative-shaped" (every step body is
    /// one of the four `StepBody` kinds). Returns `Err` for workflows
    /// that contain arbitrary Rust closures.
    pub fn to_spec(&self) -> Result<WorkflowSpec, OrkError>;
}

#[derive(Clone, Copy, Debug)]
pub enum Strictness { Strict, Lenient }
```

`CatalogView<'a>` is a lightweight read-only projection over
`OrkApp::manifest()` that exposes the agent/tool ids, their input
schemas, and their output schemas. The loader does not hold an
`Arc<OrkApp>`; it consumes the projection.

### Validation rules (normative)

The loader rejects with `OrkError::Validation` when any of these fail:

1. **Schema validity** — `input_schema` and `output_schema` parse as
   JSON Schema 2020-12. (`schemars` plus `jsonschema` for validation.)
2. **Id charset** — workflow `id` matches `^[a-z0-9][a-z0-9-]{0,62}$`
   (the same regex from ADR [`0049`](0049-orkapp-central-registry.md)).
3. **Step id uniqueness** — within one workflow, every `step.id` and
   every `branch`/`parallel`/`dountil`/`dowhile`/`foreach` `id` is
   unique. (The loader generates anonymous ids if the user omits
   them, and surfaces them in error messages.)
4. **Catalog refs** — every `agent_call.agent` and `tool_call.tool`
   id exists in the catalog under `Strictness::Strict`.
5. **Schema compatibility** — for each `agent_call` /  `tool_call`,
   the CEL `input_expr` produces a JSON value that conforms to the
   target's input schema; the target's output schema is recorded as
   the step's output type. Schema comparison is structural
   ([`jsonschema-transpiler`](https://docs.rs/jsonschema-transpiler/)
   normalisation, not byte equality).
6. **CEL well-formedness** — every expression parses, type-checks
   against the binding shape, and respects the host limits.
7. **Graph well-formedness** — no `step` references its own output
   in `input_expr` (no self-cycle); `branch` arms have a
   total-coverage `when` (final arm `"when": "true"` or equivalent)
   or the loader emits `OrkError::Validation` with `branch.<id>.uncovered`.
8. **Suspend safety** — `suspend.resume_schema` is a valid JSON
   Schema; `payload_expr` evaluates to a JSON object.
9. **Trigger validity** — `cron.expr` parses against
   [`croner`](https://docs.rs/croner/) and `cron.tz` resolves via
   [`chrono-tz`](https://docs.rs/chrono-tz/); webhook triggers carry
   a path matching `^/[a-z0-9/_-]{1,128}$`.

Errors carry a JSON-Pointer path into the spec
(`/graph/2/arms/0/graph/0/body/input_expr`) so SDKs can highlight
the offending node.

### Persistence

A new tenant-scoped table holds uploaded specs:

```sql
-- migrations/014_workflow_specs.sql
CREATE TABLE workflow_specs (
    tenant_id  UUID NOT NULL,
    workflow_id TEXT NOT NULL,
    version    INTEGER NOT NULL,
    spec       JSONB NOT NULL,
    sha256     TEXT NOT NULL,
    catalog_manifest_sha TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    archived_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, workflow_id, version)
);

ALTER TABLE workflow_specs ENABLE ROW LEVEL SECURITY;

CREATE POLICY workflow_specs_tenant_rls ON workflow_specs
    USING       (tenant_id = current_setting('app.current_tenant_id')::uuid)
    WITH CHECK  (tenant_id = current_setting('app.current_tenant_id')::uuid);

CREATE OR REPLACE FUNCTION notify_workflow_spec_changed()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify(
        'workflow_spec_changed',
        json_build_object('tenant_id', NEW.tenant_id,
                          'workflow_id', NEW.workflow_id,
                          'version', NEW.version)::text);
    RETURN NEW;
END $$;

CREATE TRIGGER workflow_specs_notify
    AFTER INSERT OR UPDATE ON workflow_specs
    FOR EACH ROW EXECUTE FUNCTION notify_workflow_spec_changed();
```

`version` is monotonic per `(tenant_id, workflow_id)`. New uploads
do not delete prior versions; `archived_at` is set on supersede.
Active version is `MAX(version) WHERE archived_at IS NULL`.

`sha256` is the canonicalised JSON hash of `spec` (RFC 8785 JSON
Canonicalisation). `catalog_manifest_sha` records *which catalog*
the spec was validated against — used for drift detection on
re-validate.

### REST surface (extends ADR [`0056`](0056-auto-generated-rest-and-sse-surface.md))

```
POST   /api/workflows
       body: WorkflowSpec
       requires: workflow:write
       on success: 201 Created, body { workflow_id, version, sha256 }

GET    /api/workflows
       requires: workflow:read
       returns: [{ workflow_id, active_version, description, ... }]

GET    /api/workflows/:id
       requires: workflow:read
       returns: { workflow_id, version, spec, sha256, created_by, ... }

GET    /api/workflows/:id/versions
       requires: workflow:read
       returns: [{ version, sha256, archived_at, ... }]

POST   /api/workflows/:id/versions/:version/activate
       requires: workflow:admin
       activates a prior version (sets archived_at on the current,
       clears it on the named version)

DELETE /api/workflows/:id
       requires: workflow:admin
       soft-archives all versions

POST   /api/workflows/:id/run                 (already in ADR 0056)
       runs the active version
```

Three new RBAC scopes coined here, in the
`workflow:<action>` shape from ADR
[`0021`](0021-rbac-scopes.md):

- `workflow:read` — list / get specs.
- `workflow:write` — upload new specs.
- `workflow:admin` — activate prior versions, archive workflows.

These slot into ADR [`0021`](0021-rbac-scopes.md)'s `Vocabulary`
table (deferred to a follow-up ADR-0021 widening, noted in that
ADR's Reviewer findings under "Workflows / schedules / tenants
routes did not migrate to `require_scope!`").

### Hot registration through ADR [`0058`](0058-per-tenant-orkapp.md)'s resolver

Upload flow:

1. `POST /api/workflows` body deserialises into `WorkflowSpec`.
2. The loader validates against the caller tenant's
   `CatalogView` (resolved via [`0058`](0058-per-tenant-orkapp.md)'s
   `TenantAppResolver`).
3. On success, the spec is persisted and `pg_notify` fires.
4. The `CatalogTenantResolver` listens on
   `workflow_spec_changed`, invalidates the affected tenant's
   cached `Arc<OrkApp>`, and rebuilds on the next request — the
   newly uploaded workflow appears at
   `/api/workflows/<id>/run` automatically.

A `WorkflowSpecFactory` ships in `crates/ork-app/src/multi_tenant.rs`
(per [`0058`](0058-per-tenant-orkapp.md)'s factory model) and reads
the active spec for the tenant at build time.

### YAML/TOML compatibility

Existing
[`workflow-templates/`](../../workflow-templates/)
files continue to load through
[`crates/ork-workflow/src/yaml_compat.rs`](../../crates/ork-workflow/src/yaml_compat.rs).
The shim is rewritten to convert YAML/TOML → `WorkflowSpec` →
`Workflow`, dropping the parallel parser. Triggers from YAML
templates are now preserved through the spec (closes a
[`0050`](0050-code-first-workflow-dsl.md) Reviewer finding:
"YAML compat drops legacy template trigger metadata").

## Acceptance criteria

- [ ] `WorkflowSpec` (`#[serde(tag = "v")]` `V1` variant) and
      `WorkflowSpecV1` defined at
      `crates/ork-workflow/src/spec.rs` with `schemars::JsonSchema`
      derived. The generated JSON Schema is committed to
      `crates/ork-workflow/schema/workflow_spec.v1.json` and
      regenerated by a `cargo xtask gen-schemas` task.
- [ ] `Workflow::from_spec` and `Workflow::to_spec` defined at
      `crates/ork-workflow/src/loader.rs` with the signatures shown
      in `Decision`. Round-trip property test
      `crates/ork-workflow/tests/spec_roundtrip.rs::roundtrip_property`
      uses `proptest` to generate declarative-shaped workflows and
      asserts `from_spec(to_spec(w)) == w` (engine-level equality).
- [ ] CEL evaluator wrapper at
      `crates/ork-workflow/src/expr.rs` enforces the per-evaluation
      memory / wall-time / output-size limits stated in `Decision`.
      Test `crates/ork-workflow/tests/expr_limits.rs` asserts each
      limit is hit and surfaced as `OrkError::Validation`.
- [ ] Loader normative validation rules (1–9 in `Decision`) each
      have a dedicated test in
      `crates/ork-workflow/tests/loader_validation.rs`, with the
      JSON-Pointer path on the error asserted exactly.
- [ ] Migration `migrations/014_workflow_specs.sql` adds the
      `workflow_specs` table with RLS policies (`USING` + `WITH
      CHECK`) and the `workflow_spec_changed` `LISTEN`/`NOTIFY`
      trigger.
- [ ] `POST /api/workflows`, `GET /api/workflows`,
      `GET /api/workflows/:id`, `GET /api/workflows/:id/versions`,
      `POST /api/workflows/:id/versions/:version/activate`, and
      `DELETE /api/workflows/:id` defined under
      [`crates/ork-api/src/routes/workflows.rs`](../../crates/ork-api/src/routes/),
      each gated by the relevant `require_scope!` from ADR
      [`0021`](0021-rbac-scopes.md).
- [ ] `workflow:read`, `workflow:write`, `workflow:admin` registered
      in the scope vocabulary at
      [`crates/ork-security/src/scopes.rs`](../../crates/ork-security/),
      with default-token mappings added to
      [`config/default.toml`](../../config/default.toml).
- [ ] `CatalogTenantResolver` from ADR
      [`0058`](0058-per-tenant-orkapp.md) listens on
      `workflow_spec_changed` and invalidates the affected
      tenant's cached `Arc<OrkApp>` on each event. Test
      `crates/ork-app/tests/spec_hot_register.rs` uploads a spec
      via the API, then issues `POST /api/workflows/:id/run`
      against the same connection and asserts the new workflow
      runs without a process restart.
- [ ] `WorkflowSpecFactory` defined in
      `crates/ork-app/src/multi_tenant.rs` per
      [`0058`](0058-per-tenant-orkapp.md)'s factory model; reads
      the active spec from `workflow_specs` for the building
      tenant.
- [ ] YAML compat shim rewritten to convert YAML/TOML →
      `WorkflowSpec` → `Workflow`. Existing
      [`workflow-templates/`](../../workflow-templates/) files
      load and run unchanged; trigger metadata is preserved.
      Regression test
      `crates/ork-workflow/tests/yaml_compat.rs::triggers_preserved`.
- [ ] Integration test
      `crates/ork-workflow/tests/cross_tenant_isolation.rs`
      asserts that a spec uploaded by tenant A is not visible to
      tenant B (RLS) and that requesting tenant B's
      `/api/workflows/<a-id>/run` yields 404 with
      `audit.tenant_catalog_filter`.
- [ ] No file under `crates/ork-workflow/` imports `axum`,
      `sqlx`, `reqwest`, `rmcp`, `rskafka` (CI grep). The CEL
      crate is added to the allow-list.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- **Tenants ship workflows without a redeploy.** A `POST
  /api/workflows` with a valid `WorkflowSpec` produces a running
  workflow at `/api/workflows/:id/run` on the next request, scoped
  to the tenant via [`0058`](0058-per-tenant-orkapp.md).
- **One canonical wire format.** The same JSON Schema that the
  Rust loader consumes is what TS / Go SDKs (ADR
  [`0060`](0060-ork-sdk-generate-and-reference-sdks.md)) emit.
  No three-language drift.
- **The [`0048`](0048-pivot-to-code-first-rig-platform.md) pivot's
  type-safety promise survives.** The four `StepBody` kinds all
  resolve to typed Rust components; CEL is bounded; arbitrary
  code is explicitly absent. A spec that references a
  non-existent agent / tool fails *at upload time*, not at run
  time.
- **YAML compat consolidates.** Today's parallel YAML parser
  collapses into "YAML → spec → workflow", removing duplicated
  parse logic.
- **Versioning + audit.** Every uploaded spec carries `sha256`,
  `created_by`, `created_at`. Activating a prior version is one
  call. Mistakes are reversible.

### Negative / costs

- **Step bodies are constrained.** No inline JS/Python/Lua, no
  ad-hoc HTTP calls, no inline regex. Operators must publish a
  custom tool ([`0051`](0051-code-first-tool-dsl.md)) or agent
  ([`0052`](0052-code-first-agent-dsl.md)) for anything that
  doesn't fit `agent_call`/`tool_call`/`transform`/`suspend`.
  This is a deliberate trade for sandbox safety.
- **CEL is yet another language.** Authors writing transforms
  and predicates learn it. CEL is small (one-page reference) and
  battle-tested, but it is not Rust.
- **Schema drift between catalog and spec.** A spec validated
  against catalog manifest `sha=X` may fail to run after the
  operator updates the catalog to `sha=Y`. Mitigated by recording
  `catalog_manifest_sha` on upload and re-validating on every
  catalog change; failed re-validations surface as
  `audit.spec_revalidate_failed` and the spec is auto-archived
  (operator must re-upload). Not free; documented under
  `Open questions`.
- **Storage cost.** Every uploaded spec persists; archived
  versions stay for audit. A tenant pushing 100 versions a day
  produces ~36k rows/year. JSONB compresses well, but at 10k
  tenants this is real. Default retention: keep the last 50
  archived versions per workflow + a 90-day tail. Configurable.
- **Suspend semantics break with arbitrary CEL state.** The
  current snapshot model assumes step output is the only thing
  preserved across pause; CEL state inside a `dountil` body
  isn't part of the snapshot today. We preserve only the
  step-output state and re-evaluate predicates on resume.
  Documented; revisit if customers hit it.

### Neutral / follow-ups

- **WASM step bodies** (the "any-language code" story) are a
  separate ADR. The closed `StepBody` enum can grow a `wasm`
  variant without breaking v1 spec consumers — they'd reject the
  variant under `Strict`. v1 ships without it.
- **Workflow `Trigger` storage** lands here for
  cron/webhook variants; the scheduler service from
  [`0019`](0019-scheduled-tasks.md) consumes the
  `workflow_specs.trigger` field. ADR
  [`0019`](0019-scheduled-tasks.md)'s open questions resolve in
  the loader (timezone, dedupe-by-fired-at).
- **Studio support** ([`0055`](0055-studio-local-dev-ui.md)) gets
  a "workflows" tab that lists tenant uploads, shows diffs
  between versions, lets the operator activate a prior version.
  Out of scope here; the REST surface is the wire shape.
- **DevPortal** ([`0005`](0005-agent-card-and-devportal-discovery.md))
  becomes a natural place to publish "available catalogs" so
  external SDK users can discover what to write specs *against*.
  Future ADR.

## Alternatives considered

- **Keep YAML as the upload format.** Rejected. YAML's parser
  permissiveness (anchors, merge keys, type coercion) is a
  liability for a tenant-supplied input; JSON is strict and
  matches what every SDK serialises natively. YAML stays as
  *input* through the compat shim, but JSON is the canonical
  store + wire shape.
- **Use a Rust-only embedded scripting language** (Rhai, mlua,
  Starlark) for step bodies. Rejected for v1. Each adds a
  surface area we'd have to defend (sandbox escape, version
  drift, tooling). CEL covers transforms and predicates; richer
  step bodies are deferred to WASM.
- **Allow arbitrary `tool_call` to a "shell" tool that runs
  code.** Rejected. Bypasses the closed-StepBody contract;
  operators who want this can publish such a tool *if they
  trust their tenants*, and the catalog mechanism makes that an
  explicit operator decision rather than a default.
- **Compile specs to bytecode at upload, run the bytecode.**
  Rejected for v1. Today's `ProgramOp` is already the bytecode;
  spec → `ProgramOp` is the compile step. A separate IR adds
  surface area for no current benefit.
- **One global workflow registry** (no tenant scoping). Rejected.
  Every realistic deployment needs per-tenant workflow
  authorship; bolting that on later would break the URL shape.
  Tenant scoping is a v1 invariant, not an extension.
- **Skip versioning; latest spec wins.** Rejected. Mistakes are
  inevitable; rollback to a prior version must be one call.
  Versioning is cheap to ship and load-bearing for ops trust.

## Affected ork modules

- [`crates/ork-workflow/`](../../crates/ork-workflow/) — new
  modules `spec.rs`, `loader.rs`, `expr.rs`; YAML compat shim
  rewritten on top of the loader; existing builder unchanged.
- [`crates/ork-api/src/routes/workflows.rs`](../../crates/ork-api/src/routes/) —
  new upload + version-management routes; existing
  `POST /api/workflows/:id/run` unchanged.
- [`crates/ork-app/src/multi_tenant.rs`](../../crates/ork-app/src/multi_tenant.rs) —
  `WorkflowSpecFactory` per
  [`0058`](0058-per-tenant-orkapp.md)'s factory model; the
  resolver listens on `workflow_spec_changed` for hot
  invalidation.
- [`crates/ork-security/src/scopes.rs`](../../crates/ork-security/) —
  `workflow:read` / `workflow:write` / `workflow:admin`.
- New: `migrations/014_workflow_specs.sql` — `workflow_specs`
  table + RLS policies + `LISTEN`/`NOTIFY` trigger.
- [`config/default.toml`](../../config/default.toml) — new
  `[workflow.expr]` and `[workflow.specs]` sections.
- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) —
  `ork lint` extends to validate any local `*.workflow.json`
  against a target catalog manifest.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [`createWorkflow`](https://mastra.ai/docs/workflows/overview) JSON serialisation in `mastra build` | `WorkflowSpec` v1 |
| Temporal | [Workflow definition language proposal](https://docs.temporal.io/) — Temporal does not yet ship a wire format; their workflows are language SDKs | this ADR commits to a wire format first, SDKs second |
| Conductor | [Conductor JSON DSL](https://conductor-oss.github.io/conductor/documentation/configuration/workflowdef/index.html) | closest existing parallel; their `tasks: []` is our `graph: []` |
| Argo Workflows | [`Workflow` CRD](https://argoproj.github.io/argo-workflows/) | YAML-shaped DAG with templates; we adopt the closed-set body model and the JSON canonical form |
| CEL | [Common Expression Language](https://github.com/google/cel-spec) | `transform`, `branch.when`, `dountil.until`, `dowhile.while`, `foreach.items_expr` |

## Open questions

- **Catalog drift handling.** When the operator's catalog changes
  in a way that invalidates an active spec, do we (a) refuse
  catalog updates that break specs (operator-friendly, tenant-
  hostile), (b) auto-archive broken specs (current default —
  tenant-friendly, may surprise operators at migration), or (c)
  emit a deprecation event and let a configurable grace period
  expire? Default: (b) with a `audit.spec_revalidate_failed`
  event; revisit after first ops report.
- **CEL extensions.** Authors will eventually want richer
  built-ins (regex, JSON Pointer, time math). CEL has standard
  extensions (`cel-spec` includes them); we ship the base set
  in v1 and add extensions in a follow-up ADR rather than open
  the door now.
- **Spec-level imports / fragments.** `parallel.arms` and
  `branch.arms` will repeat patterns across specs. A
  `$ref`-style import (à la JSON Schema) is tempting but adds a
  resolver step; v1 has no imports. Revisit when real specs
  show duplication.
- **Resume-time CEL re-evaluation.** When a workflow resumes
  from a snapshot, predicates with non-pure CEL (e.g.
  `time.now()`-style; we don't ship that, but the question
  exists for future extensions) must re-evaluate. v1's CEL
  binding set is pure; this is a future-extension question.
- **Maximum spec size.** Default cap: 1 MiB compressed
  (per upload). Configurable. Larger specs likely indicate a
  composition gap (no `$ref`).

## References

- ADR [`0050`](0050-code-first-workflow-dsl.md) — code-first DSL
  this ADR is a wire-format companion to.
- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot;
  type-safety promise this ADR honours.
- ADR [`0058`](0058-per-tenant-orkapp.md) — `TenantAppResolver`,
  `CatalogTenantResolver`, factory model.
- ADR [`0056`](0056-auto-generated-rest-and-sse-surface.md) —
  REST surface this ADR extends.
- ADR [`0021`](0021-rbac-scopes.md) — scope vocabulary.
- ADR [`0020`](0020-tenant-security-and-trust.md) — RLS contract
  used by `workflow_specs`.
- ADR [`0019`](0019-scheduled-tasks.md) — `Trigger` semantics.
- CEL spec: <https://github.com/google/cel-spec>
- RFC 8785 (JSON Canonicalisation): <https://datatracker.ietf.org/doc/html/rfc8785>
- Conductor workflow DSL: <https://conductor-oss.github.io/conductor/documentation/configuration/workflowdef/index.html>
- Argo Workflows: <https://argoproj.github.io/argo-workflows/>
