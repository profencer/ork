# 0060 — `ork sdk generate` and reference TS / Go workflow SDKs

- **Status:** Proposed
- **Date:** 2026-05-09
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0048, 0049, 0050, 0056, 0057, 0058, 0059
- **Supersedes:** —

## Context

ADR [`0059`](0059-declarative-workflow-wire-format.md) commits ork
to a canonical workflow wire format (`WorkflowSpec` v1) plus a
tenant-scoped upload endpoint. The Rust loader at
[`crates/ork-workflow/src/loader.rs`](../../crates/ork-workflow/src/loader.rs)
reads specs and produces the same `Workflow` value the code-first
DSL ([`0050`](0050-code-first-workflow-dsl.md)) builds. That makes
JSON the truth — but writing JSON by hand is the wrong DX for
anyone who is not a JSON Schema savant.

Polyglot authoring needs **language-side builders** that emit valid
specs. Two more pieces are needed before an external developer can
ship a workflow without reading the JSON Schema:

1. **A codegen entry point** that turns a JSON Schema (the
   workflow-spec contract from
   [`0059`](0059-declarative-workflow-wire-format.md)) plus an
   operator-specific `OrkApp::manifest()` snapshot
   ([`0049`](0049-orkapp-central-registry.md)) into typed
   bindings the SDK consumes.
2. **Reference SDK packages** in the two ecosystems most likely to
   onboard non-Rust customers — TypeScript and Go — that wrap
   those typed bindings in an idiomatic builder API.

This ADR locks in the **contract** for both: the `ork sdk generate`
CLI verb (extending [`0057`](0057-ork-cli-dev-build-start.md)), the
shape of generated artefacts, the manifest-pinning safety contract,
and the public API surface of the reference TS and Go packages.

The reference SDKs ship in **separate repositories** (one per
ecosystem) so they can use idiomatic packaging (`npm`, Go modules)
without bloating the main workspace; this ADR is what those repos
implement against.

## Decision

ork **introduces `ork sdk generate`**, the CLI command that emits
typed SDK bindings from a `WorkflowSpec` JSON Schema and an
`AppManifest` snapshot. ork **publishes two reference SDK
packages** — `@ork/workflow-sdk` (TypeScript, npm) and
`github.com/ork-dev/workflow-sdk-go` (Go module) — that wrap the
generated bindings in an idiomatic builder API and emit valid
`WorkflowSpec` v1 documents. The Rust code-first DSL
([`0050`](0050-code-first-workflow-dsl.md)) gains an
`into_spec()` method (already on
[`0059`](0059-declarative-workflow-wire-format.md)'s acceptance
list) and is, structurally, the third SDK.

The wire format (`WorkflowSpec` v1) is the only contract these
three SDKs share. None of them depends on any other; all three
deserialise into the same Rust `Workflow` value via
`Workflow::from_spec`.

### `ork sdk generate` — CLI surface

`ork sdk generate` is a new subcommand under
[`0057`](0057-ork-cli-dev-build-start.md)'s `Cmd` enum:

```rust
// crates/ork-cli/src/main.rs
#[derive(Subcommand)]
enum Cmd {
    // ... existing variants from ADR 0057 ...

    /// Generate typed SDK bindings for a target language.
    Sdk {
        #[command(subcommand)]
        cmd: SdkCmd,
    },
}

#[derive(Subcommand)]
enum SdkCmd {
    /// Emit typed bindings for the catalog at `--from`.
    Generate {
        /// Source manifest. URL (`https://api.example.com/api/manifest`)
        /// or path (`./target/manifest.json`).
        #[arg(long)] from: ManifestSource,
        /// Target language: `ts` | `go` | `rust`.
        #[arg(long, value_enum)] target: SdkTarget,
        /// Output directory.
        #[arg(long, short)] out: PathBuf,
        /// Pin the manifest hash into the generated artefacts.
        #[arg(long, default_value_t = true)] pin: bool,
        /// Bearer token for `--from <URL>` (admin scope).
        #[arg(long, env = "ORK_SDK_TOKEN")] token: Option<String>,
    },
}

#[derive(ValueEnum, Clone)]
enum SdkTarget { Ts, Go, Rust }
```

A typical flow:

```bash
# Operator ships a catalog. SDK consumer points at it:
ork sdk generate --from https://acme.example.com/api/manifest \
                 --target ts \
                 --out  ./generated/acme

# Or, for offline / CI use:
curl -H "Authorization: Bearer $TOKEN" \
     https://acme.example.com/api/manifest > acme.manifest.json
ork sdk generate --from ./acme.manifest.json --target go --out ./gen/acme
```

The command writes a directory tree the SDK package consumes. The
shape per target is documented under "Generated artefact layout"
below.

### Manifest pinning — the safety contract

Every generated artefact embeds:

- **`manifest_sha`** — SHA-256 of the canonicalised manifest JSON
  (RFC 8785), recorded as a constant in the generated code.
- **`spec_schema_sha`** — SHA-256 of the `WorkflowSpec` v1 JSON
  Schema the artefact was generated against.
- **`ork_min_version`** — semver of the ork catalog the manifest
  came from (read from `AppManifest.ork_version`).

When an SDK builder serialises a `WorkflowSpec` for upload, it
includes a `provenance` block:

```json
{
  "v": "V1",
  "id": "ticket-triage",
  ...
  "provenance": {
    "manifest_sha": "sha256:...",
    "spec_schema_sha": "sha256:...",
    "sdk": "ts/0.1.4"
  }
}
```

`POST /api/workflows` validates the spec twice: first against the
`WorkflowSpec` v1 schema (the spec-schema check), then against the
*current tenant catalog*. If the spec's `provenance.manifest_sha`
does not match the current catalog's manifest sha, the response is
either:

- **409 Conflict** if the spec validates against the current
  catalog anyway (the SDK is stale but the spec is still valid;
  the response body suggests `ork sdk generate` to refresh);
- **422 Unprocessable Entity** if the spec references agents /
  tools that no longer exist (the SDK is stale *and* the spec is
  no longer valid).

The provenance field is **advisory** — clients can omit it and the
spec still validates against the runtime catalog. Its only purpose
is producing better error messages and audit trails; it is **not**
load-bearing for security.

### Generated artefact layout

#### Target `ts` (TypeScript)

```
out/
├── package.json                  # name = "@<scope>/ork-catalog-<...>", version pinned to manifest sha
├── manifest.json                 # the source manifest, embedded verbatim
├── src/
│   ├── index.ts                  # re-exports
│   ├── catalog.ts                # PROVENANCE constants + manifest sha
│   ├── agents.ts                 # const agents = { classifier: { ... }, ... }
│   ├── tools.ts                  # const tools = { jira: { create_issue: { ... } }, ... }
│   ├── schemas.ts                # zod schemas generated from agent/tool JSON Schemas
│   └── workflow-spec.schema.ts   # the WorkflowSpec v1 JSON Schema as a const
└── README.md                     # autogenerated; explains how to consume
```

A consumer authors workflows like this:

```ts
// user code, after `npm install @ork/workflow-sdk @<scope>/ork-catalog-acme`
import { workflow, step } from "@ork/workflow-sdk";
import { agents, tools } from "@<scope>/ork-catalog-acme";

export default workflow("ticket-triage")
  .input(z.object({ summary: z.string(), id: z.string() }))
  .output(z.object({ status: z.string(), assigned_to: z.string() }))
  .step("classify", step.agent(agents.classifier, ({ input }) => ({
    text: input.summary,
  })))
  .branch(({ classify }) => classify.severity === "high",
    s => s.step("page",  step.tool(tools.pagerduty.fire,
      ({ input }) => ({ summary: input.summary }))),
    s => s.step("queue", step.tool(tools.jira.create_issue,
      ({ input }) => ({ title: input.summary }))));
```

The `step.agent(agents.classifier, fn)` form gives the consumer
*type-safe input shaping*: `fn`'s argument type is derived from
the upstream step's output, and its return type is checked against
the agent's input schema (codegen'd from
`agents.classifier.input_schema`). The `${...}` interpolation form
in [`0059`](0059-declarative-workflow-wire-format.md)'s
`input_expr` is generated at serialise time from the function body
via a tiny CEL emitter (see "Compilation: TS function → CEL"
below).

`@ork/workflow-sdk` runtime API:

```ts
export function workflow<I, O>(id: string): WorkflowBuilder<I, O>;
export const step: {
  agent<A, In>(agent: AgentRef<A>, input: (ctx: StepCtx<In>) => InputOf<A>):
    AgentStep<A>;
  tool<T, In>(tool: ToolRef<T>, input: (ctx: StepCtx<In>) => InputOf<T>):
    ToolStep<T>;
  transform<X, In>(fn: (ctx: StepCtx<In>) => X): TransformStep<X>;
  suspend<R>(opts: { payload: (ctx: StepCtx) => unknown; resume: z.ZodType<R> }):
    SuspendStep<R>;
};
export interface WorkflowBuilder<I, O> {
  input <X>(schema: z.ZodType<X>): WorkflowBuilder<X, O>;
  output<X>(schema: z.ZodType<X>): WorkflowBuilder<I, X>;
  retry(p: RetryPolicy): this;
  timeout(ms: number): this;
  trigger(t: Trigger): this;
  step  <S>(id: string, body: S): WorkflowBuilder<I, OutputOf<S>>;
  branch(...): WorkflowBuilder<I, O>;
  parallel(...): WorkflowBuilder<I, O>;
  doUntil(...): WorkflowBuilder<I, O>;
  doWhile(...): WorkflowBuilder<I, O>;
  forEach(...): WorkflowBuilder<I, O>;
  /// Serialise to WorkflowSpec v1 JSON.
  toSpec(): WorkflowSpec;
}
```

#### Target `go`

```
out/
├── go.mod                        # module github.com/<scope>/ork-catalog-<...>
├── manifest.json
├── catalog/
│   ├── provenance.go             # ManifestSHA, SpecSchemaSHA, OrkMinVersion
│   ├── agents.go                 # var Classifier = AgentRef[ClassifierIn, ClassifierOut]{ ... }
│   ├── tools.go
│   └── schemas.go                # generated structs + JSON tags
└── README.md
```

Go authoring:

```go
package main

import (
    "github.com/ork-dev/workflow-sdk-go/orkw"
    catalog "github.com/<scope>/ork-catalog-acme/catalog"
)

func TicketTriage() *orkw.Workflow[TicketIn, TicketOut] {
    return orkw.New[TicketIn, TicketOut]("ticket-triage").
        Step("classify", orkw.Agent(catalog.Classifier,
            func(c orkw.StepCtx[TicketIn]) catalog.ClassifierIn {
                return catalog.ClassifierIn{Text: c.Input().Summary}
            })).
        Branch(
            orkw.When(`step.classify.output.severity == "high"`).Do(
                orkw.Step("page", orkw.Tool(catalog.Tools.Pagerduty.Fire, ...))),
            orkw.Otherwise().Do(
                orkw.Step("queue", orkw.Tool(catalog.Tools.Jira.CreateIssue, ...)))).
        Build()
}
```

Go's lack of expression-body lambdas means CEL predicates are
written as raw strings (validated at `Build()` time against the
binding shape). Input-shaping closures still work because the SDK
runs them once at `Build()` time and emits the resulting CEL.

Go target supports Go ≥ 1.22 (generics required).

#### Target `rust`

The Rust target generates a thin shim over
[`crates/ork-workflow/`](../../crates/ork-workflow/) that wires
the catalog's typed agent/tool ids into the existing builder. It
exists primarily for **federation** — when a Rust-shop ork
operator wants to author workflows for a *different* catalog (a
partner organisation's), `ork sdk generate --target rust --from
<partner manifest>` produces a no-std-friendly crate.

For the operator authoring against their *own* catalog,
`ork sdk generate --target rust` is unnecessary; the existing
[`0050`](0050-code-first-workflow-dsl.md) code-first DSL is
already typed against the local `OrkApp`.

### Compilation: SDK function bodies → CEL

The TS / Go SDKs accept user-supplied **input-shaping** functions
(e.g. `({ input }) => ({ text: input.summary })`). On
`.toSpec()` / `.Build()`, the SDK runs the function once with a
**proxy object** that records property accesses and the literal
shape of the returned object, then emits the equivalent CEL:

- `({ input }) => ({ text: input.summary })`
- becomes `"input_expr": "{ text: input.summary }"`.

This is the same trick Drizzle, Prisma, and other "typed query
builder" libraries use. It is **deliberately limited**:

- Only property access and literal object/array construction.
- No control flow inside input-shaping functions.
- No method calls (`.toString()`, `.includes(...)`).

If the proxy detects a forbidden operation (e.g. an `if` branch),
the SDK throws a `CelEmitError` at `.toSpec()` time with the
exact source location. Branch / `doUntil` predicates are written
as raw CEL strings — the proxy form is reserved for the simple
input-shaping case.

This keeps the wire format pure-CEL while letting authors write
idiomatic SDK code for the common case.

### Versioning and compatibility

- The wire format is `WorkflowSpec` v1, frozen by ADR
  [`0059`](0059-declarative-workflow-wire-format.md).
- The SDK packages use semver: `0.1.x` for v1-targeting
  pre-stable, `1.x` once production-stable. Their major version
  tracks the wire-format major.
- A v2 wire format spawns a v2 SDK release; v1 SDKs remain
  installable until ork-server drops v1 reading (no sooner than
  one major ork release after v2 ships).
- Generated artefact directories embed `spec_schema_sha`; SDK
  package versions ≥ a known-good range for that sha are
  required at compile time (TS via `peerDependencies`; Go via
  module-version pin in the generated `go.mod`).

### Distribution

Reference SDKs live in separate repositories under
`github.com/ork-dev/`:

- `ork-dev/workflow-sdk-ts` (TypeScript). Published to npm as
  `@ork/workflow-sdk`. CI on every PR; release tags publish.
- `ork-dev/workflow-sdk-go` (Go module
  `github.com/ork-dev/workflow-sdk-go/orkw`). Released via Go
  module version tags.

Generated catalog packages (the per-operator artefacts) are
**not** published by the ork project. Operators publish their own
catalogs to their own registries (private npm, Go private module
repo, or vendored). The `ork sdk generate` output is consumable
as a local package without publication.

### Catalog discovery for SDK consumers

To produce a generated SDK, a developer needs a manifest. Three
ways to obtain one:

1. **`GET /api/manifest`** with a token carrying the new
   `catalog:read` scope (see RBAC below). Response is the
   tenant's view per ADR
   [`0058`](0058-per-tenant-orkapp.md).
2. **`GET /api/admin/tenants/:id/manifest`** for operators (the
   admin endpoint from
   [`0058`](0058-per-tenant-orkapp.md)).
3. **`ork inspect --tenant <id>`** locally for dev.

A new RBAC scope is coined here: **`catalog:read`** — read the
catalog manifest. End-user tokens get this by default
(`config/default.toml` `[security.scopes].end_user`); SDK
generation against a partner catalog requires the partner-issued
token to carry it.

### What the ADR locks in

This ADR makes the **contract** binding:

- The CLI surface (`ork sdk generate ...`).
- The generated artefact layout per target.
- The manifest-pinning + provenance-block contract.
- The public surface of `@ork/workflow-sdk` and
  `workflow-sdk-go` (`workflow` / `step` / `WorkflowBuilder` /
  the four `step.*` constructors).
- The `catalog:read` scope.

It does **not** lock in:

- Internal codegen tooling (we may use `quicktype`,
  `oapi-codegen`, hand-rolled emitter — implementer's choice).
- npm package layout beyond the public exports.
- Go module internal package structure beyond what's quoted
  above.
- The proxy-to-CEL emitter implementation; only the *behaviour*
  is normative.

## Acceptance criteria

- [ ] `ork sdk generate` subcommand defined at
      [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs)
      with the `Sdk { cmd: SdkCmd }` shape from `Decision`.
      Help text matches the surface above.
- [ ] `--from <URL>` and `--from <path>` both work; the URL form
      uses the `Authorization: Bearer` token from `--token` /
      `ORK_SDK_TOKEN`. Test
      `crates/ork-cli/tests/sdk_generate.rs::manifest_from_url`
      uses `wiremock` to assert the scope-checked HTTP path.
- [ ] `--target ts` writes the layout in "Generated artefact
      layout" above; assertion test
      `crates/ork-cli/tests/sdk_generate.rs::ts_layout`
      validates each emitted file's header constants
      (`manifest_sha`, `spec_schema_sha`, `ork_min_version`).
- [ ] `--target go` writes the layout above; equivalent
      assertion test
      `crates/ork-cli/tests/sdk_generate.rs::go_layout`.
- [ ] `--target rust` writes a Cargo crate that depends on
      `ork-workflow` and exposes `agents` / `tools` modules.
      Smoke test: `cargo check` on the emitted crate succeeds in
      a tempdir.
- [ ] Manifest canonicalisation uses RFC 8785; the canonicaliser
      lives at `crates/ork-cli/src/sdk/canonical.rs` (or a
      shared location under `crates/ork-common/`) and has a
      golden-file test against the [RFC 8785 test vectors](https://datatracker.ietf.org/doc/html/rfc8785#section-3.2.4).
- [ ] `provenance.manifest_sha` mismatch produces 409 from
      `POST /api/workflows` when the spec still validates and
      422 when it does not. Test
      `crates/ork-api/tests/workflow_upload_provenance.rs`
      covers both branches.
- [ ] `catalog:read` scope registered in
      [`crates/ork-security/src/scopes.rs`](../../crates/ork-security/),
      added to default end-user scope set in
      [`config/default.toml`](../../config/default.toml), and
      enforced on `GET /api/manifest`. Test
      `crates/ork-api/tests/manifest_scope.rs::catalog_read_required`.
- [ ] `WorkflowSpec` v1 JSON Schema, manifest schema, and an
      example manifest checked into the repo at
      `crates/ork-cli/sdk-fixtures/` for SDK-side CI to consume.
      A `cargo xtask publish-sdk-fixtures` task copies them to a
      release artefact.
- [ ] Reference TS package skeleton committed to
      `github.com/ork-dev/workflow-sdk-ts` (separate repo) with
      the public exports listed in `Decision`. The package
      ingests `crates/ork-cli/sdk-fixtures/manifest.example.json`
      via the codegen and asserts the resulting bindings type-check
      under `tsc --strict`. CI on that repo runs against this
      fixture on every push.
- [ ] Reference Go package skeleton at
      `github.com/ork-dev/workflow-sdk-go` with the surface in
      `Decision`. CI runs `go test ./...` and a fixture-driven
      "generate then build" round-trip.
- [ ] `Workflow::to_spec()` from
      [`0059`](0059-declarative-workflow-wire-format.md) emits
      a spec that, fed back through any reference SDK's
      validator (TS via `zod` over the schema, Go via
      `validateSpec`), passes. Test
      `crates/ork-workflow/tests/sdk_roundtrip_fixture.rs`
      asserts this for the bundled fixture workflows.
- [ ] CLI `ork lint` extends to "validate any local
      `*.workflow.json` against the manifest at
      `--catalog`". Test
      `crates/ork-cli/tests/sdk_generate.rs::lint_local_spec`.
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- **Idiomatic authoring per language.** TS authors get zod-typed
  inputs, IntelliSense across every agent/tool in the catalog,
  and tsc-checked refactors. Go authors get generics-driven
  type safety. Rust authors keep
  [`0050`](0050-code-first-workflow-dsl.md)'s code-first DSL.
- **One wire format, three SDKs.** No language-side drift —
  every SDK serialises to the same `WorkflowSpec` v1 the Rust
  loader consumes. Bug-fix-once.
- **Catalog-typed SDKs are a real-DX upgrade.** Most "code-first"
  agent platforms type against a generic API; the catalog-pinned
  generation step gives the SDK consumer types specific to *this
  operator's catalog*, including agent input/output schemas.
- **Manifest pinning closes the staleness foot-gun.** A spec
  generated against an old catalog gets a 409/422 with
  actionable error messages, not a silent runtime failure.
- **External repos for SDKs keep the main workspace lean.** No
  npm/Go tooling in the Rust workspace; SDK CI lives where it
  belongs.

### Negative / costs

- **Three SDKs to maintain.** Even with one wire format,
  language-idiom updates (TS strict mode upgrades, Go major
  version bumps) cost engineering time. Mitigation: scope the
  SDK to the wire-format's surface; do not chase per-language
  feature parity beyond what the spec expresses.
- **Codegen for catalogs needs to be fast.** A 200-agent catalog
  could produce a large emitted SDK. Default budget: < 5 s for
  a 100-agent catalog; failures profiled and tracked.
- **Proxy-to-CEL emitter has sharp edges.** Authors will write
  function bodies the emitter cannot parse and get a runtime
  `CelEmitError`. The error must be precise (line / column,
  source token); imprecise errors are an SDK-quality bug.
  Mitigation: a property test that fuzzes function shapes
  asserting either "compiles to CEL" or "emits a precise error"
  (no third option).
- **The "external repos" choice means SDK releases lag ork
  releases.** A wire-format change requires three coordinated
  releases. Mitigation: release notes pin the matching SDK
  versions; CI in each SDK repo runs against the main ork
  branch's fixtures so drift is caught early.
- **Per-operator catalog SDK packages multiply.** A medium-sized
  customer with 5 partner catalogs ends up with 5 generated
  packages, each pinned to a hash. Mitigation: regen on a
  schedule via `ork sdk generate` in the customer's CI;
  document the pattern.
- **`catalog:read` is a small surface widening.** Today
  `GET /api/manifest` is implicitly tenant-scoped via
  [`0058`](0058-per-tenant-orkapp.md); this ADR makes it
  explicitly scope-gated. Default-grant on end-user tokens
  preserves UX; minor breaking change for tokens without it
  (mitigated by config-file default).

### Neutral / follow-ups

- **More SDK targets.** Python (likely next), Java, Kotlin. The
  contract supports any language with a JSON Schema validator
  and basic codegen. Each is a follow-up ADR + repo.
- **Hosted catalog publishing** (a `catalog publish` verb that
  pushes the manifest to a private registry, à la
  Backstage software catalog). Out of scope for v1; the file +
  HTTP `--from` cases cover the common path.
- **Visual builder** that emits `WorkflowSpec` directly. The
  wire format is designed to be the bridge for that future
  product; out of scope here.
- **CEL playground in SDK error messages.** When the
  proxy-to-CEL emitter rejects a function, link to a hosted
  CEL playground with the partial state pre-loaded. Polish, not
  v1.
- **DevPortal integration** ([`0005`](0005-agent-card-and-devportal-discovery.md))
  — DevPortal can host `ork sdk generate`-friendly manifests
  for partner catalogs. Future ADR.

## Alternatives considered

- **Hand-write the TS / Go SDKs against the Rust types.**
  Rejected. Three serialisers to maintain; each one drifts
  from the wire format; bugs surface at upload time.
- **Generate SDKs at server-side as part of `POST
  /api/sdk/generate`.** Rejected. The codegen runtime would
  need a Rust HTTP endpoint shipping `tsc` / `gofmt` /
  `quicktype`, polluting the server image. CLI-side keeps the
  server image clean and lets generation run in CI.
- **Skip codegen entirely; ship generic SDKs that accept agent
  ids as strings.** Rejected. Loses the catalog-typed DX that's
  this ADR's main pitch. Stringly-typed SDKs already exist
  (literally write the JSON by hand); we want to do better.
- **Use OpenAPI / AsyncAPI as the wire format and generate
  SDKs from it.** Rejected. The workflow shape is graph-shaped,
  not request-response; OpenAPI's tooling produces awkward
  bindings for graph types. The bespoke `WorkflowSpec` schema
  matches the domain.
- **Pin SDK versions to ork versions instead of wire-format
  versions.** Rejected. Wire format major bumps less often
  than ork itself; pinning to the wire format means SDKs need
  fewer releases.
- **One SDK repo with target-specific subpackages.** Rejected.
  Each ecosystem expects its own repo conventions
  (`package.json` at root, `go.mod` at root). A monorepo with
  `ts/` and `go/` subdirs surprises every consumer.
- **Embed CEL emission as a host-side step at upload (the
  client sends the source function body as a string, the
  server parses).** Rejected. Pushing parser surface area into
  ork-api is a bad ergonomic / security trade; SDKs do the
  emission, server validates the resulting CEL only.

## Affected ork modules

- [`crates/ork-cli/src/main.rs`](../../crates/ork-cli/src/main.rs) —
  new `Sdk` subcommand and codegen orchestration.
- New: `crates/ork-cli/src/sdk/` — `manifest.rs` (fetch +
  canonicalise), `ts.rs`, `go.rs`, `rust.rs`, `proxy.rs` (the
  function-to-CEL emitter for downstream SDKs to mirror).
- [`crates/ork-cli/sdk-fixtures/`](../../crates/ork-cli/) —
  golden manifests + spec-schema fixtures consumed by SDK CI.
- [`crates/ork-security/src/scopes.rs`](../../crates/ork-security/src/scopes.rs) —
  `catalog:read` scope.
- [`crates/ork-api/src/routes/manifest.rs`](../../crates/ork-api/src/routes/) —
  `require_scope!("catalog:read")` on `GET /api/manifest`.
- [`config/default.toml`](../../config/default.toml) —
  `catalog:read` added to default end-user scope set.
- New external repos (out of this workspace, but contracted by
  this ADR): `github.com/ork-dev/workflow-sdk-ts`,
  `github.com/ork-dev/workflow-sdk-go`.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Drizzle ORM | proxy-based query builder emitting SQL | proxy-based input-shaping emitting CEL |
| Prisma | typed client generated from a schema | typed catalog SDK generated from a manifest |
| Mastra Cloud | TypeScript-only authoring | this ADR is the language-agnostic generalisation |
| OpenAPI Generator | one schema, N language clients | exact pattern, applied to the workflow domain |
| `tsoa` / `nestia` | TS-side codegen from typed sources | adjacent — they go schema-from-code; we go code-from-schema |
| Conductor SDKs | per-language clients calling a JSON DSL | similar shape; we add catalog-typed bindings |

## Open questions

- **Python SDK timing.** Likely first follow-up. Python's
  type-checking story is weaker than TS / Go; we'd lean on
  `pydantic` + `mypy`. Defer until we see customer demand.
- **CEL extensions exposed via SDK helpers.** When ADR
  [`0059`](0059-declarative-workflow-wire-format.md) adds CEL
  extensions (regex, JSON Pointer, time math), each SDK needs
  proxy-side helpers. We will not pre-emptively add them.
- **Catalog SDK package naming.** `@<scope>/ork-catalog-<id>`
  for npm; `github.com/<scope>/ork-catalog-<id>` for Go. The
  `<scope>` is the operator's namespace. Customers may want to
  override per-team. Default: configurable via
  `ork sdk generate --pkg-name <name>`.
- **SDK-side spec validation.** Each SDK can validate its emitted
  spec locally before upload (against the embedded schema).
  Default: yes. Disabled with `--unsafe` for codegen testing.
- **Error-message UX for proxy-to-CEL failures.** Rough today;
  budget for "shows the offending source line and a one-line
  reason" before stable release.

## References

- ADR [`0059`](0059-declarative-workflow-wire-format.md) —
  the wire format this ADR ships SDKs for.
- ADR [`0050`](0050-code-first-workflow-dsl.md) — Rust
  code-first DSL; structurally the third SDK.
- ADR [`0049`](0049-orkapp-central-registry.md) —
  `OrkApp::manifest()` is the codegen input.
- ADR [`0058`](0058-per-tenant-orkapp.md) — per-tenant
  manifests.
- ADR [`0057`](0057-ork-cli-dev-build-start.md) — CLI surface
  this extends.
- ADR [`0021`](0021-rbac-scopes.md) — `catalog:read` scope
  vocabulary.
- RFC 8785 (JSON Canonicalisation):
  <https://datatracker.ietf.org/doc/html/rfc8785>
- CEL spec: <https://github.com/google/cel-spec>
- OpenAPI Generator (parallel pattern):
  <https://openapi-generator.tech/>
- Drizzle ORM (proxy-based emission pattern):
  <https://orm.drizzle.team/docs/overview>
