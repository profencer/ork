# 0046 — Evaluation harness and regression corpus

- **Status:** Superseded by 0054
- **Date:** 2026-04-29
- **Phase:** 4
- **Relates to:** 0011, 0017, 0018, 0022, 0025, 0034

## Context

ork has no end-to-end regression coverage of agent behaviour. Crate
unit tests in [`crates/ork-core/tests/`](../../crates/ork-core/tests/),
[`crates/ork-api/tests/`](../../crates/ork-api/tests/) and friends
exercise individual ports, but nothing checks "feed a workflow input
through [`WorkflowEngine`](../../crates/ork-core/src/workflow/engine.rs)
end-to-end and assert the output is the right shape." The only safety
net is hand-running the scripts under [`demo/`](../../demo/), which is
slow, ad-hoc, and easily skipped.

The ADR work loop has accelerated: the index in
[`docs/adrs/README.md`](README.md) has 45+ entries, and every recent
ADR — DAG executor enhancements
([`0018`](0018-dag-executor-enhancements.md)), validation gate
([`0025`](0025-typed-output-validation-and-verifier-agent.md)),
per-model capability profiles ([`0034`](0034-per-model-capability-profiles.md)) —
reshapes engine-internal contracts. Without an automated regression
sweep, "the engine can no longer produce a parseable extraction
output" is the kind of bug that only surfaces when a human happens to
run the demo.

There is also no CI yet: the repo has no `.github/workflows/`
directory. The verification gate in [`AGENTS.md`](../../AGENTS.md) §5
is enforced locally, on the agent's word. A CI job is the load-bearing
piece that makes the gate observable across PRs.

This ADR keeps the scope deliberately narrow: a corpus, a replay
binary, a CI job. Semantic grading, scoring, and live-LLM evaluation
are explicitly deferred.

## Decision

ork **adds a corpus-driven evaluation harness** comprising:

1. A YAML corpus under [`eval/corpus/`](../../) — one file per case,
   each carrying an `input`, a scripted `llm_script`, and an
   `expected_shape` (JSON Schema Draft 2020-12).
2. A new crate `crates/ork-eval` with one example binary, runnable as
   `cargo run --example replay -p ork-eval`. It walks the corpus, runs
   each case through `WorkflowEngine` against a scripted LLM provider,
   and validates the terminal output against `expected_shape`.
3. A CI workflow [`.github/workflows/eval.yml`](../../.github/workflows/eval.yml)
   that runs the harness on every pull request and on merges to `main`.

The corpus is the contract: a case passes iff its terminal output
parses as JSON and matches its declared schema. No exact-match, no
semantic grading, no live LLM — those are follow-ups (see
`Neutral / follow-ups`).

### Corpus case shape (YAML)

One file per case under `eval/corpus/<group>/<slug>.yaml`:

```yaml
id: invoice-extract-happy-path
description: Single agent extracts invoice fields from a one-line input.
workflow: workflow-templates/invoice-extraction.yaml   # path or inline
input:
  parts:
    - kind: text
      text: "Acme Inc, $123.45 USD"
llm_script:
  # Ordered list. Each entry is the response that the next chat() /
  # chat_stream() call returns. Mismatched length fails the case loud.
  - kind: tool_call
    tool: submit_extraction
    arguments:
      invoice_number: "ACME-001"
      total_cents: 12345
      currency: "USD"
expected_shape:
  # JSON Schema Draft 2020-12, applied to the terminal step output
  # parsed as JSON.
  type: object
  required: [invoice_number, total_cents, currency]
  properties:
    invoice_number: { type: string, minLength: 1 }
    total_cents:    { type: integer, minimum: 0 }
    currency:       { type: string, enum: [USD, EUR, GBP] }
```

`workflow:` either points to a YAML under
[`workflow-templates/`](../../workflow-templates/) or carries an inline
workflow definition (same struct, parsed by the existing workflow
compiler). Cases that need an artifact in
[`crates/ork-storage`](../../crates/ork-storage/) fixture-load it from
a sibling file referenced by relative path; the harness binds the
in-memory artifact adapter at startup.

### Rust surface

A single library crate plus the example binary:

```rust
// crates/ork-eval/src/lib.rs

#[derive(Debug, Deserialize)]
pub struct Case {
    pub id: String,
    pub description: Option<String>,
    pub workflow: WorkflowRef,                   // path | inline
    pub input: A2aInputFixture,                  // parts to feed the run
    pub llm_script: Vec<ScriptedResponse>,
    pub expected_shape: serde_json::Value,       // raw JSON Schema
    #[serde(default)]
    pub artifacts: Vec<ArtifactFixture>,
}

#[derive(Debug)]
pub enum Outcome {
    Pass { case_id: String, duration_ms: u64 },
    Fail { case_id: String, stage: FailStage, detail: String },
}

pub enum FailStage {
    LoadCase,
    BuildEngine,
    DispatchRun,
    ParseOutput,
    SchemaCheck,
    ScriptExhausted,                             // ran out of scripted responses
    ScriptUnused,                                // case ended with responses left
}

pub async fn run_case(case: &Case) -> Outcome;

pub fn discover_corpus(root: &Path) -> Result<Vec<PathBuf>, std::io::Error>;
```

The scripted provider lives next to the loader:

```rust
// crates/ork-eval/src/scripted_llm.rs

/// Returns scripted responses in declaration order. Each chat() /
/// chat_stream() call pops one entry; mismatched arity fails the case.
pub struct ScriptedLlmProvider {
    script: Mutex<VecDeque<ScriptedResponse>>,
    name: &'static str,
}

#[async_trait]
impl LlmProvider for ScriptedLlmProvider { /* … */ }
```

`ScriptedResponse` is one of `text`, `tool_call`, `tool_call_then_text`,
or `error` — enough to drive the existing
[`LocalAgent`](../../crates/ork-agents/src/) loop deterministically.

### Example binary

`crates/ork-eval/examples/replay.rs`:

- Discovers `eval/corpus/**/*.yaml` from the workspace root.
- Spawns a tokio runtime.
- Runs cases sequentially (no parallelism in the seed; the harness is
  IO-bound on schema parsing, not CPU-bound).
- Emits one line per case: `PASS  invoice-extract-happy-path  37ms`
  or `FAIL  invoice-extract-happy-path  ScriptExhausted: …`.
- Exits `0` on all-pass, `1` on any failure.
- Optional flag `--filter <substr>` to run a subset locally.

### CI workflow

`.github/workflows/eval.yml` (the first CI workflow in the repo):

```yaml
name: eval
on:
  pull_request:
  push:
    branches: [main]
jobs:
  replay:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { toolchain: "1.85" }
      - uses: Swatinem/rust-cache@v2
      - run: cargo run --example replay -p ork-eval --release
```

The job is added as a required status check on `main`. It does not
also run `cargo test --workspace`; that lives under a future
`ci.yml`. This ADR scopes only to the eval gate.

## Acceptance criteria

- [ ] New crate `crates/ork-eval` added to
      [`Cargo.toml`](../../Cargo.toml) `[workspace] members` and to
      `[workspace.dependencies]` with a `path =` entry.
- [ ] Public types `Case`, `WorkflowRef`, `A2aInputFixture`,
      `ScriptedResponse`, `ArtifactFixture`, `Outcome`, `FailStage`
      defined in `crates/ork-eval/src/lib.rs` with the shape shown in
      `Decision`.
- [ ] `ScriptedLlmProvider` in `crates/ork-eval/src/scripted_llm.rs`
      implements [`LlmProvider`](../../crates/ork-core/src/ports/llm.rs)
      from `ork-core`.
- [ ] `pub async fn run_case(&Case) -> Outcome` and
      `pub fn discover_corpus(&Path) -> Result<Vec<PathBuf>, _>`
      exported from `crates/ork-eval/src/lib.rs`.
- [ ] Example binary at `crates/ork-eval/examples/replay.rs` runnable
      via `cargo run --example replay -p ork-eval`; exits non-zero on
      any case failure; supports `--filter <substr>`.
- [ ] At least three corpus cases under `eval/corpus/`, each backed by
      an existing template in
      [`workflow-templates/`](../../workflow-templates/) (e.g.
      `release-notes.yaml`, `change-plan.yaml`, `standup-brief.yaml`).
- [ ] Schema check uses the `jsonschema` crate (Draft 2020-12); the
      same dependency added by ADR
      [`0025`](0025-typed-output-validation-and-verifier-agent.md) is
      reused — no second JSON-Schema engine.
- [ ] `.github/workflows/eval.yml` runs the harness on `pull_request`
      and `push` to `main`; uses pinned action versions.
- [ ] Integration test
      `crates/ork-eval/tests/replay_smoke.rs` covers: pass case,
      schema-mismatch fail, `ScriptExhausted` fail, `ScriptUnused`
      fail. (This validates the harness itself, separately from the
      corpus it runs.)
- [ ] [`README.md`](README.md) ADR index row added.
- [ ] [`metrics.csv`](metrics.csv) row appended (see
      [`METRICS.md`](METRICS.md)).

## Consequences

### Positive

- Engine regressions caught between ADRs, not when a human runs the
  demo. Every PR that breaks the seed corpus fails CI.
- The corpus is data, not code: contributors add YAML files without
  learning the harness internals.
- The scripted-LLM substrate is reusable. Future tests that need a
  deterministic `LlmProvider` (e.g. agent-loop tests beyond ADR
  [`0011`](0011-native-llm-tool-calling.md) coverage) can depend on
  `ork-eval` and use `ScriptedLlmProvider` directly.
- Lines up cleanly with ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md): the
  JSON-Schema engine is shared, and the corpus's `expected_shape`
  blocks become natural seed material for `validate.schema` on real
  workflow steps.
- First CI workflow in the repo. Future workflows
  (`cargo test --workspace`, `cargo clippy`,
  [`AGENTS.md`](../../AGENTS.md) §5 verification gate as CI) follow
  the same pattern.

### Negative / costs

- Scripted LLM responses go stale when an agent's prompt or tool
  surface changes. The case fails loudly with `ScriptExhausted` /
  `ScriptUnused`; updating `llm_script:` becomes part of any prompt
  change. Acknowledged cost; documented in the harness README.
- Schema-only assertions miss semantic regressions ("the extracted
  total is now off by a factor of 100" passes if the type is still
  `integer`). Out of scope here; ADR
  [`0025`](0025-typed-output-validation-and-verifier-agent.md)'s
  verifier agent is the right home for semantic grading and a future
  ADR will lift it into the harness.
- Adding a CI gate slows merges by the harness's runtime. Seed corpus
  is three cases; expected wall-clock < 10s. Budget is "the gate must
  fit inside a 60s CI step"; the harness fails fast on any single case
  exceeding 30s.
- One more workspace crate to compile. Marginal; `ork-eval` is small
  and only the example brings in the engine graph.
- Engine construction in the harness must avoid Postgres / Redis /
  Kafka. If the current `WorkflowEngine` constructor demands them, an
  in-memory adapter ships in `ork-eval`. If a port is *not* yet
  abstracted enough to fake, the case is dropped from the seed corpus
  rather than hacked around — that absence is itself useful signal.

### Neutral / follow-ups

- A future ADR may add **cassette recording**: run the harness against
  a real provider, capture `ChatStreamEvent` traces, write them back
  into the case's `llm_script:` block. Strictly additive.
- A future ADR may add **scoring**: replace boolean shape pass/fail
  with rubric-graded scores via the ADR-0025 verifier port. Same
  corpus shape, additional `rubric:` field per case.
- A future ADR may **tier the corpus**: `eval/corpus/smoke/` runs on
  every PR (this ADR's scope), `eval/corpus/extended/` runs nightly
  with a real provider.
- A future ADR may run the harness in **parallel**; for the seed it
  is sequential to keep failure attribution simple.
- A future ADR may add **a `--record` mode** that diff-updates a
  case's `expected_shape` from the observed output, with manual
  review.

## Alternatives considered

- **Use `cargo test --workspace` integration tests instead of a
  corpus-driven binary.** Rejected: the corpus is data, not Rust
  source. Tying every case to a `#[tokio::test]` makes additions
  require Rust changes and review, raising the bar for the contributor
  workflow this ADR is designed to enable. The example binary keeps
  the entry point obvious (`cargo run --example replay -p ork-eval`)
  and the corpus directly addressable.
- **A new top-level binary crate (`ork-eval-cli`) instead of an
  example.** Rejected for the smallest scope: examples are the
  canonical Rust convention for "small runnable that uses crate APIs."
  Promoting to a binary crate is a follow-up if the harness grows a
  CLI surface (subcommands, `--record`, etc.).
- **Drive the corpus from `ork-cli`.** Rejected: `ork-cli` is the
  user-facing CLI. Mixing developer-only test machinery into it
  complicates both surfaces.
- **Run live LLMs in CI.** Rejected: non-deterministic, billed,
  externally gated. CI must stay deterministic and offline. Live mode
  is a follow-up under `extended/`.
- **Adopt an external eval framework (Inspect AI, promptfoo,
  LangSmith).** Rejected for the seed: each one would need an adapter
  to drive `WorkflowEngine` rather than a raw prompt, which is most of
  what a small in-tree harness does anyway. We can adopt an external
  scorer once the corpus surface stabilises and we want rubric-grading
  primitives we do not have.
- **Snapshot-test the terminal output verbatim
  (`insta`-style).** Rejected: snapshot tests are brittle against any
  legitimate prompt change. Shape-only is the deliberate trade — the
  harness is a contract test, not a golden-output test.
- **Skip CI; run the harness ad-hoc.** Rejected: the value of a
  regression suite is that it runs without anyone remembering. CI is
  what makes the gate load-bearing.

## Affected ork modules

- New crate: `crates/ork-eval/`
  - `Cargo.toml` — depends on `ork-core`, `ork-agents`, `ork-llm`,
    `serde`, `serde_yaml`, `serde_json`, `jsonschema`, `tokio`,
    `async-trait`.
  - `src/lib.rs`, `src/scripted_llm.rs`, `src/corpus.rs`.
  - `examples/replay.rs`.
  - `tests/replay_smoke.rs`.
- [`Cargo.toml`](../../Cargo.toml) — workspace members + dependencies
  rows.
- New directory `eval/corpus/` with seed cases and a brief
  `eval/README.md` explaining the case format.
- New `.github/workflows/eval.yml` (first workflow in the repo).
- No runtime changes to `ork-core`, `ork-agents`, `ork-llm`, or any
  other production crate. The harness consumes their public APIs as
  they stand. If `WorkflowEngine` construction today requires a real
  Postgres / Kafka / Redis, the in-memory adapter ships in `ork-eval`
  rather than touching those crates' boundaries.

## Reviewer findings

Filled in **after** the required `code-reviewer` subagent pass on the
implementation diff. Leave empty until the implementation lands.

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| | | |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| OpenAI Evals | per-case input + JSON-schema-graded output | `Case.input` + `expected_shape` |
| Inspect AI | YAML corpus + scorer model | YAML corpus; scorer deferred to ADR-0025 follow-up |
| promptfoo | `tests:` array with `assert: type: is-json-schema` | `expected_shape:` block |
| LangSmith Datasets | stored input / expected pairs, replayed via SDK | `eval/corpus/` walked by `replay` example |
| ADR [`0025`](0025-typed-output-validation-and-verifier-agent.md) | `SchemaCheck` over JSON Schema 2020-12 | Same `jsonschema` crate, reused as the assertion engine |

## Open questions

- **Scripted LLM dispatch order.** Match by call order, or by hash of
  `(messages, tools)`? Stance: order. Mismatched length already fails
  the case loud; hash-based dispatch is over-engineering at this
  scope. Confirm in implementation review.
- **Persistence-fake placement.** If the in-memory adapter has to
  reach further into `ork-persistence` or `ork-core` than expected, do
  we (a) carve a new port in those crates, or (b) shrink the seed
  corpus to cases that do not need it? Stance: (b) for the seed; (a)
  is its own ADR.
- **Schema authoring style.** Inline JSON Schema in YAML is verbose.
  Acceptable for the seed (3 cases). If the corpus grows past ~20,
  factor a `schemas/` directory and `$ref`. Defer.
- **Artifact fixtures.** Some workflows
  (e.g. [`artifact-tour.yaml`](../../workflow-templates/artifact-tour.yaml))
  consume a pre-loaded artifact. Initial stance: load from a sibling
  path under the case file. Confirm the in-memory artifact adapter
  exists or ship one in `ork-eval`.
- **Failure-mode cases.** Do we encode "case is expected to fail"
  (e.g. validation gate trips and returns `validation_exhausted`)?
  Stance: yes — `expected_shape` may be `{ "const": "validation_exhausted" }`
  applied to the terminal `StepResult` failure reason rather than the
  output. Detail deferred until the harness lands and we have a real
  case to encode.

## References

- JSON Schema Draft 2020-12: <https://json-schema.org/draft/2020-12>
- `jsonschema` crate: <https://crates.io/crates/jsonschema>
- OpenAI Evals: <https://github.com/openai/evals>
- Inspect AI: <https://inspect.aisi.org.uk/>
- promptfoo: <https://www.promptfoo.dev/>
- Related ADRs: [`0011`](0011-native-llm-tool-calling.md),
  [`0017`](0017-webui-chat-client.md),
  [`0018`](0018-dag-executor-enhancements.md),
  [`0022`](0022-observability.md),
  [`0025`](0025-typed-output-validation-and-verifier-agent.md),
  [`0034`](0034-per-model-capability-profiles.md)
