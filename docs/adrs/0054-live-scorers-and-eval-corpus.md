# 0054 — Live scorers and offline eval corpus

- **Status:** Implemented
- **Date:** 2026-05-01
- **Deciders:** ork core
- **Phase:** 4
- **Relates to:** 0011, 0048, 0049, 0050, 0052, 0053
- **Supersedes:** 0046

## Context

The repository's [`todos.md`](../../todos.md) and the
[self-review](../../self-reviews/2026-04-29-adrs-0019-0045-and-pivot.md)
both flagged the same gap: there is no first-class way in ork to
ask "did this agent do its job?" ADR
[`0046`](0046-evaluation-harness-and-regression-corpus.md) (Proposed,
superseded) sketched an offline replay harness, which is half the
answer; the other half is *live* scoring (sample N% of production
traffic, run a judge model or a deterministic check, store the
score, alert on regressions).

Mastra ships both halves under one shape:
- [`createScorer`](https://mastra.ai/docs/evals/overview) +
  `agent.scorers = { relevancy: { scorer, sampling: { type: 'ratio',
  rate: 0.5 } } }` for **live** evaluation. Results land in
  `mastra_scorers` for analysis.
- A separate
  [evaluate / dataset](https://mastra.ai/docs/evals/overview) flow
  for **offline** runs — fixture in, prediction out, scorer
  computes per-example, aggregate report.

rig has no scorer abstraction (it stops at the agent). For
LLM-as-judge scorers, rig's
[`Extractor<M, T>`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html)
gives us a typed-output judge in one expression.

## Decision

ork **introduces a `Scorer` port and the two surfaces that consume
it**: live sampling on registered agents/workflows, and an
offline `OrkEval` runner against a JSONL corpus. Results land in a
single `scorer_results` Postgres table that Studio (ADR 0055)
visualises and CI reads.

```rust
use ork_eval::{scorer, Scorer, RunInput, RunOutput, ScoreCard};
use ork_eval::scorers::{answer_relevancy, faithfulness, exact_match};

pub fn relevancy() -> impl Into<ScorerSpec> {
    answer_relevancy()
        .judge_model("openai/gpt-4o-mini")
        .build()
}

let app = OrkApp::builder()
    .agent(weather_agent())
    .scorer(ScorerTarget::agent("weather"),
            ScorerSpec::live(relevancy(), Sampling::ratio(0.1)))
    .build()?;
```

### Scorer trait

```rust
#[async_trait]
pub trait Scorer: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ScoreSchema;   // expected score shape

    async fn score(
        &self,
        input: ScoreInput,
    ) -> Result<ScoreCard, OrkError>;
}

pub struct ScoreInput<'a> {
    pub run_id: RunId,
    pub run_kind: RunKind,            // Agent | Workflow
    pub user_message: &'a str,
    pub final_response: &'a str,
    pub trace: &'a Trace,             // tool calls, intermediate steps
    pub expected: Option<&'a serde_json::Value>,  // dataset reference
    pub context: &'a AgentContext,
}

pub struct ScoreCard {
    pub score: f32,                   // 0.0 – 1.0
    pub label: Option<String>,        // pass/fail/etc
    pub rationale: Option<String>,    // "why this score"
    pub details: serde_json::Value,   // scorer-specific
}
```

### Built-in scorers

```rust
// crates/ork-eval/src/scorers/
pub fn answer_relevancy() -> AnswerRelevancyBuilder;     // judge model
pub fn faithfulness() -> FaithfulnessBuilder;            // judge model + context
pub fn toxicity() -> ToxicityBuilder;                    // judge model
pub fn exact_match() -> ExactMatchBuilder;               // deterministic
pub fn json_schema_match(schema: JsonSchema) -> JsonSchemaMatchBuilder; // det
pub fn regex_match(re: Regex) -> RegexMatchBuilder;      // deterministic
pub fn tool_calls_recall(expected: Vec<ToolCallExpectation>)
    -> ToolCallsRecallBuilder;                           // deterministic
pub fn cost_under(usd: f32) -> CostUnderBuilder;         // deterministic
pub fn latency_under(d: Duration) -> LatencyUnderBuilder; // deterministic
```

LLM-as-judge scorers (relevancy, faithfulness, toxicity) wrap rig's
`Extractor<M, JudgeOutput>` where `JudgeOutput` is a typed
`(score, rationale)` shape. The judge model is configurable per
scorer.

### Live sampling

```rust
pub enum Sampling {
    /// Run every Nth eligible run. `rate` is in [0, 1].
    Ratio { rate: f32 },
    /// Run a fixed number per minute (rate-limited live evals).
    PerMinute { n: u32 },
    /// Run on every error.
    OnError,
    /// Never run live; only offline.
    Never,
}
```

The runtime hook (a `CompletionHook` per ADR 0052) fires after
every agent or workflow completion. If the scorer's `Sampling`
predicate is satisfied, the score job is dispatched **out of the
hot path** to a background worker so the user-visible response is
not delayed. Result rows land in `scorer_results`.

### `scorer_results` table

```
scorer_results(
    id UUID PRIMARY KEY,
    tenant_id, agent_id, workflow_id,
    run_id, run_kind,
    scorer_id,
    score REAL, label TEXT, rationale TEXT, details JSONB,
    created_at, scorer_duration_ms,
    judge_model TEXT, judge_input_tokens, judge_output_tokens,
    sampled_via TEXT  -- "live:ratio", "offline:dataset:foo"
)
```

Migration ships in
[`migrations/`](../../migrations/). Studio queries this table for
the scorer dashboard (ADR 0055). The CI gate reads it to gate
merges (an option, see `Open questions`).

### Offline `OrkEval` runner

```rust
// crates/ork-eval/src/runner.rs
pub struct OrkEval { /* ... */ }

impl OrkEval {
    pub fn new(app: &OrkApp) -> Self;

    pub fn dataset(self, path: impl AsRef<Path>) -> Self;  // JSONL
    pub fn target_agent(self, id: impl Into<String>) -> Self;
    pub fn target_workflow(self, id: impl Into<String>) -> Self;
    pub fn scorer<S: Into<ScorerSpec>>(self, s: S) -> Self;
    pub fn concurrency(self, n: usize) -> Self;
    pub fn output(self, path: impl AsRef<Path>) -> Self;   // report.json

    pub async fn run(self) -> Result<EvalReport, OrkError>;
}

pub struct EvalReport {
    pub examples: usize,
    pub passed: usize,
    pub failed: usize,
    pub by_scorer: HashMap<String, ScorerAggregate>,
    pub regressions: Vec<RegressionRow>,    // vs `--baseline path`
    pub raw_path: PathBuf,
}
```

Dataset format (one example per line):

```json
{"id":"ex-001","input":{"city":"SF"},"expected":{"high_f":[58,75]},"tags":["weather"]}
```

The runner produces a `report.json` next to a per-example `.jsonl`
of full traces. CI integration is one bash line:

```bash
ork eval --agent weather --dataset data/weather.jsonl \
  --baseline previous-report.json --fail-on regression
```

Replaces ADR 0046's bespoke harness with a strictly more general
one.

### Surface registration

`OrkAppBuilder::scorer(target, spec)` (ADR 0049 stub):

```rust
pub enum ScorerTarget {
    Agent(AgentId),
    Workflow(WorkflowId),
    AgentEverywhere,            // attach to every agent
    Wildcard(GlobPattern),      // "weather-*"
}

pub enum ScorerSpec {
    Live { scorer: Box<dyn Scorer>, sampling: Sampling },
    Offline { scorer: Box<dyn Scorer> },
    Both  { scorer: Box<dyn Scorer>, sampling: Sampling },
}
```

A scorer attached as `Both` runs live on sampled production traffic
**and** in `ork eval` runs against datasets, with the same
implementation. Mastra has separate types for live vs offline; we
unify because the Rust trait surface is identical.

## Acceptance criteria

- [ ] New crate `crates/ork-eval/` with `Cargo.toml` declaring
      `ork-core`, `ork-common`, `serde`, `schemars`, `tokio`,
      `futures`, `rig-core`. No `axum`/`reqwest`/`rmcp`/`rskafka`.
- [ ] `Scorer` port at `crates/ork-core/src/ports/scorer.rs` with
      the surface in `Decision`.
- [ ] `OrkAppBuilder::scorer(target, spec)` registers the scorer;
      `OrkApp` exposes `scorers()` that returns the registered
      bindings (consumed by the live hook + the offline runner).
- [ ] Live sampling: `CompletionHook` shipped that, when the
      target's sampling predicate fires, enqueues a score job to a
      background worker (channel-bounded to prevent backpressure
      affecting the user-facing path). Test
      `crates/ork-eval/tests/live_sampling.rs` asserts (a) score
      rows land for sampled requests, (b) user-facing response
      latency is unaffected within ±5 ms (measured against a
      no-scorer baseline), (c) dropped score jobs (queue full)
      surface as `scorer_dropped_total` Prometheus counter.
- [ ] Built-in scorers `answer_relevancy`, `exact_match`,
      `json_schema_match`, `tool_calls_recall`, `cost_under`,
      `latency_under` implemented with one integration test each
      under `crates/ork-eval/tests/scorers/`.
- [ ] LLM-as-judge scorers wrap `rig::Extractor<M, JudgeOutput>`
      where `JudgeOutput { score: f32, rationale: String }`;
      verified by `crates/ork-eval/tests/judge_model_smoke.rs`
      against a scripted LLM that returns the expected structured
      output.
- [ ] Migration `migrations/NNNN_scorer_results.sql` adds the
      `scorer_results` table with indices on `(tenant_id, agent_id,
      created_at DESC)` and `(tenant_id, scorer_id, score)`.
- [ ] `OrkEval` runner: ships as a library crate function and a
      CLI subcommand `ork eval` (ADR 0057). Test
      `crates/ork-eval/tests/runner_smoke.rs` runs a 3-example
      JSONL dataset against a scripted agent, asserts the report
      contains the expected aggregates and one regression row vs
      a baseline.
- [ ] `--fail-on` option supported: `regression`, `score_below
      <THRESHOLD>`, `failures_above <N>`. Exit code matrix
      documented in the runner's docs.
- [ ] CI grep: no file under `crates/ork-eval/` imports `axum`,
      `reqwest`, `rmcp`, or `rskafka`.
- [ ] [`README.md`](README.md) ADR index row added; ADR 0046
      status flipped to `Superseded by 0054`.
- [ ] [`metrics.csv`](metrics.csv) row appended.

## Consequences

### Positive

- One scorer trait powers both live sampling and offline replay.
  Authoring an in-house scorer is one trait impl; running it on
  prod traffic + on a dataset is two registrations.
- LLM-as-judge scorers reuse rig's `Extractor`, so the typed
  output shape is enforced and the judge prompts are normal
  `instructions` strings.
- The `scorer_results` table is the answer to "is the agent
  regressing?" Studio's dashboard, the CI gate, and the
  director's "is this real for our org?" question all read the
  same data.
- The CLI surface (`ork eval --fail-on regression`) drops into
  any CI without bespoke ork integration. Customers get a
  pre-merge gate by default.

### Negative / costs

- Live scoring multiplies token spend. A 10 % live sample on a
  high-traffic agent with two judge scorers can double the
  monthly LLM bill. Mitigation: `Sampling::Ratio` defaults to a
  conservative 1 %; `cost_under` and `latency_under`
  (deterministic) are free; `PerMinute` cap is the production
  default for paying customers.
- Offline datasets are user-supplied. Crafting a meaningful
  dataset is the genuine work; the harness can't make that easier.
  Documented as the bar a customer has to meet.
- Background score worker is a new failure mode: if the worker
  panics, scores are silently dropped. The
  `scorer_dropped_total` counter and a Studio "scorer health"
  panel cover this; an alert ADR (downstream) hooks Prometheus.
- `tool_calls_recall` requires *expected tool calls* to be
  declared per dataset example — a real authoring cost. Worth
  it; this is the highest-signal scorer for tool-using agents.

### Neutral / follow-ups

- A "human in the loop scorer" (Studio panel that paints failed
  scores and lets a human override) is a future ADR; the schema
  carries `label` precisely for this.
- A regression-detection ADR can layer on top: weekly cron that
  runs the offline corpus, posts a delta to a #channel.
  Triggered via ADR 0050's `Trigger::cron(...)`.
- A dataset-from-production tool ("freeze the last 100 prod
  requests as a dataset") is a Studio feature; out of scope here
  but trivial on top of `scorer_results` + the trace store.
- A "scorer-as-tool" mode (LLMs can score themselves and store
  the result) is a future ADR but trivial on top of ADR 0051.

## Alternatives considered

- **Adopt LangSmith / Phoenix / Langfuse externally instead of
  building this.** Rejected. ADR 0048's positioning is on-prem
  Rust with no required SaaS dependency. We can offer Langfuse
  as an *exporter* later (a `LangfuseExporter` consuming
  `scorer_results`) but the primary store is ours.
- **Ship only offline (ADR 0046's shape).** Rejected.
  "Offline-only" means the customer never sees regressions until
  the next CI run; live scoring is the early-warning signal that
  matters in production.
- **One generic `evaluate(input, output) -> f32` function, no
  trait.** Rejected. The trait surface lets scorer impls own
  state (judge model handles, regex caches) and lets us
  parameterise through `ScorerSpec::live`/`offline`/`both`. The
  function shape forces every scorer into a stateless closure.
- **Use rig's `Extractor` directly in user code, no judge
  scorer abstraction.** Rejected. Centralising the judge-model
  surface lets us standardise the `(score, rationale)`
  output shape, the input prompt template, and the cost
  accounting (every live judge call has a `judge_model`,
  `judge_input_tokens`, `judge_output_tokens` recorded).
- **Combine scorer with workflow steps (a "scorer step" type).**
  Rejected. Scorers run *after* a run completes, against the
  trace; they are not workflow nodes. Mastra keeps them
  separate; we follow.

## Affected ork modules

- New: [`crates/ork-eval/`](../../crates/) — scorers, runner, CLI
  glue.
- [`crates/ork-core/src/ports/scorer.rs`](../../crates/ork-core/src/) —
  `Scorer` trait.
- [`crates/ork-app/`](../../crates/) — registration surface +
  background worker spawn.
- [`crates/ork-cli/`](../../crates/ork-cli/) — `ork eval`
  subcommand.
- [`crates/ork-persistence/`](../../crates/ork-persistence/) —
  `scorer_results` table.
- [`migrations/`](../../migrations/) — migration.

## Reviewer findings

| Severity | Finding | Resolution |
| -------- | ------- | ---------- |
| Critical | C1 — `OrkApp::build()` did not spawn the live worker or attach `LiveAgentScoringHook` to matching agents; `OrkApp::scorers()` was a holding cell only. | **Fixed in-session.** `OrkAppBuilder::build()` now spins up `spawn_worker(...)` once when at least one `Live`/`Both` binding exists and calls `agent.inject_run_complete_hook(...)` on every agent whose id matches a binding's target. New `Agent::inject_run_complete_hook` default no-op port in `ork-core`; `CodeAgent` overrides via `Mutex<Vec<...>>`. |
| Major | M1 — No Postgres-backed `ScorerResultSink`; `scorer_results` table would stay empty at runtime. | **Acknowledged, deferred.** v1 ships `InMemoryScorerResultSink` as the default sink so the worker is observable end-to-end (Studio panels and the live test gate read through it). The Postgres-backed sink lands as a follow-up driven by ADR-0058 (per-tenant overlay) which already requires the sqlx pool plumbing this would share. `OrkAppBuilder::scorer_sink(Arc<dyn ScorerResultSink>)` accepts a custom sink today. |
| Major | M2 — ADR text and `Reviewer findings` did not record the `rig::Extractor<M, JudgeOutput>` → `LlmProviderJudge` substitution; `rig-core` dep in `ork-eval/Cargo.toml` was unused. | **Fixed in-session.** Substitution recorded in this row; `LlmProviderJudge` (`crates/ork-eval/src/scorers/llm_judge.rs`) preserves the typed `(score, rationale)` contract while routing through `LlmProvider` so judge calls inherit `LlmRouter` tenant overrides + cost accounting. `rig-core` dep dropped from `ork-eval/Cargo.toml`. |
| Major | M3 — Fatal-error paths in `rig_engine` (`max_tool_iterations == 0`, `MaxTurnsError`, `Tool`/`Completion`/`Prompt` stream errors) skipped `RunCompleteHook` so `Sampling::OnError` under-fired. | **Fixed in-session.** `RigEngine::run` early-out path and `handle_stream_err` now both fire `RunCompleteHook` before the consumer emits the `Err`. `handle_stream_err` was refactored to return the `OrkError` it would emit so the caller can fire hooks first. |
| Major | M4 — Required `tests/scorers/answer_relevancy.rs` integration test was missing; the judge scorer was only covered by `tests/judge_model_smoke.rs`. | **Fixed in-session.** Added `crates/ork-eval/tests/scorers/answer_relevancy.rs` with three tests: judge-output passthrough, `judge_model(...)` override, and `try_build` error contract. |
| Major | M5 — `ScoredRow` lacked `judge_model` / `judge_input_tokens` / `judge_output_tokens`; columns existed in the migration but were never written. | **Fixed in-session.** Added explicit fields to `ScoredRow`; live worker extracts them from `ScoreCard.details` via `extract_judge_metadata`. `Judge` trait return type evolved to `JudgeResponse { output, usage: JudgeUsage { prompt_tokens, completion_tokens } }`; `LlmProviderJudge` populates from `ChatResponse.usage`; the three judge scorers (relevancy/faithfulness/toxicity) now stamp `judge_input_tokens`/`judge_output_tokens` into `details`. |
| Minor | m1 — `Scorer::score` takes `&ScoreInput<'_>` (borrow), ADR shows owned `ScoreInput`. | **Acknowledged, kept borrow.** Borrowing avoids cloning `Trace` per scorer when several attach to one run; documented here. |
| Minor | m2 — `cost_under` returns `score = 0.0` with `label = "unknown"` when no cost is reported, which the runner counts as a failure. | **Acknowledged, deferred.** Module doc comment flags the limitation; production cost telemetry lands with ADR-0058. The label is distinct so a downstream regression detector can ignore `unknown` rows. |
| Minor | m3 — `LiveSamplerHandle::try_enqueue` collapsed `Closed` and `Full` into the same counter. | **Fixed in-session.** Added `scorer_worker_closed_total` Prometheus counter; closed-channel drops increment it instead of `scorer_dropped_total`. |
| Minor | m4 — `user_facing_latency_unchanged_within_5ms` test compares means over 50 iterations, prone to scheduler-jitter flake. | **Acknowledged, deferred.** Test left as-is for v1; a 1000-iter / p99 variant lands with the perf-CI follow-up. |
| Minor | m5 — `Sampling::Ratio` wasted an RNG draw at `rate = 0.0` / `1.0`. | **Fixed in-session.** Short-circuit before the draw. |
| Minor | m6 — Judge builders panicked when `.judge(...)` was missing. | **Fixed in-session.** Added `try_build()` returning `OrkError::Configuration`; `build()` retained as the panic-on-misuse alias used by tests + the ADR's example. |
| Minor | m7 — `.judge_model("openai/gpt-4o-mini")` was a no-op stub on the three judge builders. | **Fixed in-session.** Override stored on the scorer struct and surfaced as `details.judge_model` (and `ScoredRow.judge_model`); falls through to the injected `Judge`'s `judge_model()` when unset. |
| Minor | m8 — `RunCompleteHook` doc comment said it fired *after* `CompletionHook` on every run-end path, but `CompletionHook` only fires on the success path. | **Fixed in-session.** Doc comment in `crates/ork-agents/src/hooks.rs` now spells out: success → `CompletionHook` then `RunCompleteHook`; cancel/fatal/`tool_loop_exceeded` → `RunCompleteHook` only. |
| Nit | n1 — `Sampling` / `ScorerSpec` are not `Serialize`. | **Acknowledged, deferred** to ADR-0055 (Studio) where the manifest projection lives. |
| Nit | n2 — `glob` workspace dep declared in two crates. | **Acknowledged, kept** — both `ork-app` and `ork-eval` legitimately use the dep. |
| Nit | n3 — Variant `ScorerSpec::Live` shares the lower-case identifier `live(...)` with the constructor. | **Acknowledged, kept** — Rust convention; `ScorerSpec::live(...)` is the documented constructor used in the ADR example. |
| Nit | n4 — README ADR-0061 row added in this diff, unrelated to ADR-0054. | **Acknowledged, kept** as housekeeping; the 0061 ADR file already existed in the repo, only the index row was missing. |
| Nit | n5 — `OrkEval::concurrency(n)` setter stored, never read. | **Acknowledged, deferred.** Sequential dispatch is sufficient for v1 datasets; parallel dispatch (`futures::stream::buffer_unordered`) lands with ADR-0057's CLI work. |

## Prior art / parity references

| Source | Where | ork equivalent in this ADR |
| ------ | ----- | -------------------------- |
| Mastra | [Evals overview](https://mastra.ai/docs/evals/overview) | `Scorer` trait + `ScorerSpec` |
| Mastra | `agent.scorers = { relevancy: { scorer, sampling } }` | `OrkAppBuilder::scorer(...)` + `Sampling` enum |
| Mastra | `evaluate(...)` offline runner | `OrkEval` runner |
| LangSmith | dataset / experiment / scorer triad | informative — ours is on-prem |
| rig | [`Extractor`](https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html) | judge-model output shape |

## Open questions

- **CI fail-on policy.** Should `--fail-on regression` exit 1 on
  any score drop, or only on tagged "must-not-regress" scorers?
  Default v1: any score drop > 5 %; tagged scorers configurable.
- **Sampling fairness.** A naive `Sampling::Ratio` may
  over-sample fast endpoints. Whether to weight by request
  duration is open; default is unweighted.
- **Replay determinism.** `OrkEval` replays do not get
  deterministic LLM outputs by default (LLMs drift). For
  scorer-stable testing, the harness can record-and-replay LLM
  responses; this is a v2 feature.
- **Scorer composition.** Mastra has no aggregation primitive
  ("score = avg(s1, s2)"); we ship scorers as leaf evaluators.
  Aggregation can be done in the report or as another scorer.
- **Dataset versioning.** Where do datasets live (in-repo,
  Postgres, object storage)? Default v1: filesystem path; ADR
  follow-up if Studio gains a dataset editor.

## References

- ADR [`0048`](0048-pivot-to-code-first-rig-platform.md) — pivot.
- ADR [`0049`](0049-orkapp-central-registry.md) — registry.
- ADR [`0052`](0052-code-first-agent-dsl.md) — agent DSL,
  `CompletionHook` the live sampler attaches to.
- ADR [`0046`](0046-evaluation-harness-and-regression-corpus.md)
  — superseded; offline runner shape carries forward.
- Mastra evals: <https://mastra.ai/docs/evals/overview>
- rig extractor:
  <https://docs.rs/rig-core/latest/rig/extractor/struct.Extractor.html>
