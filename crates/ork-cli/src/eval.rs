//! `ork eval` CLI subcommand
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Offline OrkEval runner`).
//!
//! v1 ships an explicit, agent-free dispatch path: the runner reads
//! the example response from `input.<echo_from>` and runs the
//! configured scorers against it. This makes the subcommand usable
//! end-to-end without a live LLM (golden-set / fixture testing) and
//! is the canonical mode that `runner_smoke.rs` exercises.
//!
//! Production wiring of `ork eval` to a live `OrkApp` (loading agents
//! from config, dispatching through `OrkApp::run_agent`) lands with
//! ADR-0057 (CLI surface for the code-first platform). Until then
//! the subcommand is intentionally constrained to deterministic
//! dispatch — see the `--echo-from` flag.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use clap::{Args, ValueEnum};
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::scorer::Trace;
use ork_eval::runner::{EvalExample, EvalRunOutput, EvalRunner, FailOn, OrkEval, ScorerAggregate};
use ork_eval::scorers::{exact_match, json_schema_match};

#[derive(Args)]
pub struct EvalArgs {
    /// Target agent id (recorded on the `scorer_results.agent_id`
    /// column for traceability). Required unless `--workflow` is set.
    #[arg(long)]
    pub agent: Option<String>,

    /// Target workflow id. Mutually exclusive with `--agent`.
    #[arg(long)]
    pub workflow: Option<String>,

    /// JSONL dataset. One example per line; each row must have an
    /// `id` field, an `input` JSON object, and may carry `expected`
    /// and `tags`.
    #[arg(long)]
    pub dataset: PathBuf,

    /// Optional baseline `report.json` for regression detection.
    /// Produces one row in `report.regressions` per scorer whose
    /// mean dropped by more than the regression threshold.
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Output path for the assembled `report.json`. The per-example
    /// trace JSONL is written next to it with the same stem and
    /// `.jsonl` extension.
    #[arg(long, default_value = "report.json")]
    pub output: PathBuf,

    /// CI gating policy. See exit-code matrix on `ork-eval`'s
    /// runner module.
    #[arg(long, value_enum)]
    pub fail_on: Option<FailOnArg>,

    /// Threshold (e.g. `0.6`) for `--fail-on score-below`. Ignored
    /// for other policies.
    #[arg(long, default_value_t = 0.6)]
    pub score_below: f32,

    /// Threshold (e.g. `5`) for `--fail-on failures-above`. Ignored
    /// for other policies.
    #[arg(long, default_value_t = 0)]
    pub failures_above: usize,

    /// v1 deterministic dispatch: pull the response text from
    /// `input.<echo_from>` for every example. Use `answer` for
    /// the canonical golden-set shape (matches the runner_smoke
    /// test fixture).
    #[arg(long, default_value = "answer")]
    pub echo_from: String,

    /// Built-in scorer to attach. Repeat for multiple scorers.
    /// Recognised values:
    /// - `exact_match=<expected_field>` — passes when the response
    ///   equals `expected.<field>` (string equality).
    /// - `json_schema_match=<schema_path>` — loads the JSON Schema
    ///   from disk and validates the response.
    #[arg(long = "scorer")]
    pub scorers: Vec<String>,

    /// Threshold for the regression detector (default 0.05 = 5%).
    #[arg(long, default_value_t = 0.05)]
    pub regression_threshold: f32,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum FailOnArg {
    /// Exit 2 if any scorer regressed vs the baseline.
    Regression,
    /// Exit 3 if any scorer mean is below `--score-below`.
    ScoreBelow,
    /// Exit 4 if total failed examples exceed `--failures-above`.
    FailuresAbove,
}

/// Echo-mode runner: response is `input.<echo_from>` (string).
struct EchoRunner {
    field: String,
}

#[async_trait]
impl EvalRunner for EchoRunner {
    async fn dispatch(
        &self,
        _ctx: &AgentContext,
        example: &EvalExample,
    ) -> Result<EvalRunOutput, OrkError> {
        let response = example
            .input
            .get(&self.field)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default();
        let started = chrono::Utc::now();
        Ok(EvalRunOutput {
            final_response: response,
            trace: Trace {
                user_message: example.id.clone(),
                tool_calls: vec![],
                started_at: started,
                completed_at: chrono::Utc::now(),
            },
        })
    }
}

pub async fn run(args: EvalArgs) -> Result<()> {
    if args.agent.is_none() && args.workflow.is_none() {
        bail!("ork eval: pass either --agent <id> or --workflow <id>");
    }
    if args.agent.is_some() && args.workflow.is_some() {
        bail!("ork eval: --agent and --workflow are mutually exclusive");
    }

    let mut eval = OrkEval::new()
        .dataset(&args.dataset)
        .output(&args.output)
        .runner(Arc::new(EchoRunner {
            field: args.echo_from.clone(),
        }))
        .regression_threshold(args.regression_threshold);

    if let Some(id) = &args.agent {
        eval = eval.target_agent(id);
    }
    if let Some(id) = &args.workflow {
        eval = eval.target_workflow(id);
    }

    for spec in &args.scorers {
        let parsed = parse_scorer_spec(spec).with_context(|| format!("parse --scorer {spec}"))?;
        eval = eval.scorer(parsed);
    }

    if let Some(path) = &args.baseline {
        eval = eval.baseline(path);
    }

    let policy = args.fail_on.map(|p| match p {
        FailOnArg::Regression => FailOn::Regression,
        FailOnArg::ScoreBelow => FailOn::ScoreBelow(args.score_below),
        FailOnArg::FailuresAbove => FailOn::FailuresAbove(args.failures_above),
    });

    let report = eval.run().await.context("ork eval: runner failed")?;

    print_report_summary(&report);

    if let Some(p) = policy
        && let Some(code) = p.evaluate(&report)
    {
        eprintln!("ork eval: --fail-on hit ({p:?}), exit {code}");
        std::process::exit(code);
    }
    Ok(())
}

fn parse_scorer_spec(spec: &str) -> Result<Arc<dyn ork_core::ports::scorer::Scorer>> {
    let (name, arg) = spec.split_once('=').ok_or_else(|| {
        anyhow::anyhow!("scorer spec `{spec}` must be `<name>=<arg>` (e.g. `exact_match=value`)")
    })?;
    match name {
        "exact_match" => Ok(exact_match().expected_field(arg).build()),
        "json_schema_match" => {
            let raw =
                std::fs::read_to_string(arg).with_context(|| format!("read schema file {arg}"))?;
            let schema = serde_json::from_str(&raw)
                .with_context(|| format!("parse JSON schema at {arg}"))?;
            Ok(json_schema_match(schema).build())
        }
        other => bail!(
            "unknown scorer `{other}`; supported: exact_match, json_schema_match. \
             Other built-ins (tool_calls_recall, latency_under, cost_under, \
             answer_relevancy) need richer wiring landing in ADR-0057."
        ),
    }
}

fn print_report_summary(report: &ork_eval::runner::EvalReport) {
    println!(
        "examples: {}  passed: {}  failed: {}",
        report.examples, report.passed, report.failed
    );
    let mut ids: Vec<&String> = report.by_scorer.keys().collect();
    ids.sort();
    for id in ids {
        let agg = &report.by_scorer[id];
        print_scorer_line(id, agg);
    }
    if !report.regressions.is_empty() {
        println!("regressions:");
        for r in &report.regressions {
            println!(
                "  - {}  baseline {:.3}  current {:.3}  delta {:.3}",
                r.scorer_id, r.baseline_mean, r.current_mean, r.delta
            );
        }
    }
    println!("report:    {}", report.raw_path.display());
}

fn print_scorer_line(id: &str, agg: &ScorerAggregate) {
    println!(
        "  {id}: mean={:.3} min={:.3} max={:.3} pass={}/{}",
        agg.mean, agg.min, agg.max, agg.passed, agg.examples
    );
}
