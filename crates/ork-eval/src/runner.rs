//! Offline `OrkEval` runner
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Offline OrkEval runner`).
//!
//! Reads a JSONL dataset, drives a target agent or workflow per
//! example via an injectable [`EvalRunner`], runs every attached
//! scorer against `(input, response, trace, expected)`, and writes a
//! `report.json` plus a per-example trace JSONL.
//!
//! ## Exit-code matrix (`ork eval`)
//!
//! | `--fail-on` flag           | exit on hit | meaning                                     |
//! | -------------------------- | ----------- | ------------------------------------------- |
//! | _unset_                    | `0`         | always succeed; report-only                 |
//! | `regression`               | `2`         | any scorer mean dropped > 5% vs baseline    |
//! | `score-below <T>`          | `3`         | any scorer mean below `T`                   |
//! | `failures-above <N>`       | `4`         | total failed examples exceed `N`            |
//!
//! Any other failure (I/O error, JSONL parse error, dispatch error,
//! scorer error) returns exit code `1`. The CLI maps the
//! [`FailOn`]/[`EvalReport`] result through [`FailOn::exit_code`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::scorer::{RunId, RunKind, ScoreCard, ScoreInput, Scorer, Trace};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;

/// Aggregate over all scored examples for a single scorer id.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ScorerAggregate {
    pub examples: usize,
    pub mean: f32,
    pub min: f32,
    pub max: f32,
    pub passed: usize,
    pub failed: usize,
}

/// One regression row: a scorer whose mean dropped below the
/// baseline by more than the configured threshold (default 5%).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RegressionRow {
    pub scorer_id: String,
    pub baseline_mean: f32,
    pub current_mean: f32,
    pub delta: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct EvalReport {
    pub examples: usize,
    pub passed: usize,
    pub failed: usize,
    pub by_scorer: HashMap<String, ScorerAggregate>,
    pub regressions: Vec<RegressionRow>,
    pub raw_path: PathBuf,
}

/// CI gating policy. See module-level table for the exit-code matrix.
#[derive(Clone, Debug, PartialEq)]
pub enum FailOn {
    Regression,
    ScoreBelow(f32),
    FailuresAbove(usize),
}

impl FailOn {
    /// Map a [`FailOn`] hit to its documented exit code.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Regression => 2,
            Self::ScoreBelow(_) => 3,
            Self::FailuresAbove(_) => 4,
        }
    }

    /// Return `Some(exit_code)` if the report violates this policy.
    #[must_use]
    pub fn evaluate(&self, report: &EvalReport) -> Option<i32> {
        match self {
            Self::Regression => (!report.regressions.is_empty()).then(|| self.exit_code()),
            Self::ScoreBelow(threshold) => report
                .by_scorer
                .values()
                .any(|a| a.mean < *threshold)
                .then(|| self.exit_code()),
            Self::FailuresAbove(n) => (report.failed > *n).then(|| self.exit_code()),
        }
    }
}

/// One example as stored in the JSONL dataset.
#[derive(Clone, Debug, Deserialize)]
pub struct EvalExample {
    pub id: String,
    pub input: Value,
    #[serde(default)]
    pub expected: Option<Value>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// What the runner returns from a single example dispatch.
pub struct EvalRunOutput {
    pub final_response: String,
    pub trace: Trace,
}

/// Indirection over "how to run one example." Production wires this
/// to [`OrkApp::run_agent`](../../ork_app/struct.OrkApp.html#method.run_agent)
/// or the workflow engine; tests stub a synthetic dispatch.
#[async_trait]
pub trait EvalRunner: Send + Sync {
    async fn dispatch(
        &self,
        ctx: &AgentContext,
        example: &EvalExample,
    ) -> Result<EvalRunOutput, OrkError>;
}

/// Per-example record written to the per-run JSONL alongside the
/// summary `report.json`.
#[derive(Clone, Debug, Serialize)]
pub struct EvalExampleResult {
    pub example_id: String,
    pub run_id: RunId,
    pub final_response: String,
    pub trace: Trace,
    pub scores: HashMap<String, ScoreCard>,
}

#[derive(Default)]
pub struct OrkEval {
    dataset_path: Option<PathBuf>,
    target_agent: Option<String>,
    target_workflow: Option<String>,
    scorers: Vec<Arc<dyn Scorer>>,
    concurrency: usize,
    output_path: Option<PathBuf>,
    baseline_path: Option<PathBuf>,
    fail_on: Option<FailOn>,
    runner: Option<Arc<dyn EvalRunner>>,
    /// Threshold (fraction) for the regression detector. Default 0.05.
    regression_threshold: Option<f32>,
}

impl OrkEval {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dataset(mut self, path: impl AsRef<Path>) -> Self {
        self.dataset_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn target_agent(mut self, id: impl Into<String>) -> Self {
        self.target_agent = Some(id.into());
        self
    }

    pub fn target_workflow(mut self, id: impl Into<String>) -> Self {
        self.target_workflow = Some(id.into());
        self
    }

    pub fn scorer(mut self, scorer: Arc<dyn Scorer>) -> Self {
        self.scorers.push(scorer);
        self
    }

    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n;
        self
    }

    pub fn output(mut self, path: impl AsRef<Path>) -> Self {
        self.output_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn baseline(mut self, path: impl AsRef<Path>) -> Self {
        self.baseline_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn fail_on(mut self, policy: FailOn) -> Self {
        self.fail_on = Some(policy);
        self
    }

    pub fn runner(mut self, runner: Arc<dyn EvalRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    pub fn regression_threshold(mut self, threshold: f32) -> Self {
        self.regression_threshold = Some(threshold);
        self
    }

    #[must_use]
    pub fn fail_on_policy(&self) -> Option<&FailOn> {
        self.fail_on.as_ref()
    }

    /// Drive the eval. Loads the dataset, dispatches each example
    /// through [`EvalRunner`], runs all scorers, writes the per-example
    /// JSONL and `report.json`, and returns the assembled
    /// [`EvalReport`].
    pub async fn run(self) -> Result<EvalReport, OrkError> {
        let dataset_path = self
            .dataset_path
            .ok_or_else(|| OrkError::Validation("OrkEval: dataset(...) is required".into()))?;
        let output_path = self
            .output_path
            .unwrap_or_else(|| PathBuf::from("report.json"));
        let runner = self
            .runner
            .ok_or_else(|| OrkError::Validation("OrkEval: runner(...) is required".into()))?;
        if self.target_agent.is_none() && self.target_workflow.is_none() {
            return Err(OrkError::Validation(
                "OrkEval: either target_agent(...) or target_workflow(...) is required".into(),
            ));
        }
        let run_kind = if self.target_workflow.is_some() {
            RunKind::Workflow
        } else {
            RunKind::Agent
        };
        let agent_id = self.target_agent.clone();
        let workflow_id = self.target_workflow.clone();
        let regression_threshold = self.regression_threshold.unwrap_or(0.05);

        let dataset = read_jsonl_dataset(&dataset_path).await?;
        let raw_path = output_path.with_extension("jsonl");
        // Reset the per-example file before starting so re-runs do
        // not append to a stale tail.
        if fs::metadata(&raw_path).await.is_ok() {
            fs::remove_file(&raw_path).await.ok();
        }

        let ctx = test_only_eval_context();

        let mut per_example_results: Vec<EvalExampleResult> = Vec::with_capacity(dataset.len());
        let mut by_scorer: HashMap<String, Vec<f32>> = HashMap::new();
        let mut passed = 0usize;
        let mut failed = 0usize;

        for example in &dataset {
            let output = runner.dispatch(&ctx, example).await?;
            let mut scores = HashMap::new();
            let mut example_passed = true;
            for scorer in &self.scorers {
                let input = ScoreInput {
                    run_id: RunId::new(),
                    run_kind,
                    agent_id: agent_id.as_deref(),
                    workflow_id: workflow_id.as_deref(),
                    user_message: &chat_input_text(&example.input),
                    final_response: &output.final_response,
                    trace: &output.trace,
                    expected: example.expected.as_ref(),
                    context: &ctx,
                };
                let card = scorer.score(&input).await?;
                let scorer_id = scorer.id().to_string();
                by_scorer
                    .entry(scorer_id.clone())
                    .or_default()
                    .push(card.score);
                if card.score < 1.0 {
                    example_passed = false;
                }
                scores.insert(scorer_id, card);
            }
            if example_passed {
                passed += 1;
            } else {
                failed += 1;
            }
            per_example_results.push(EvalExampleResult {
                example_id: example.id.clone(),
                run_id: RunId::new(),
                final_response: output.final_response,
                trace: output.trace,
                scores,
            });
        }

        // Write per-example JSONL.
        write_jsonl(&raw_path, &per_example_results).await?;

        // Aggregate.
        let by_scorer_agg: HashMap<String, ScorerAggregate> = by_scorer
            .into_iter()
            .map(|(id, scores)| (id, aggregate(&scores)))
            .collect();

        // Detect regressions vs baseline.
        let regressions = match self.baseline_path {
            Some(path) => detect_regressions(&path, &by_scorer_agg, regression_threshold).await?,
            None => Vec::new(),
        };

        let report = EvalReport {
            examples: dataset.len(),
            passed,
            failed,
            by_scorer: by_scorer_agg,
            regressions,
            raw_path: raw_path.clone(),
        };

        let report_json = serde_json::to_string_pretty(&report)
            .map_err(|e| OrkError::Internal(format!("failed to serialize EvalReport: {e}")))?;
        fs::write(&output_path, report_json).await.map_err(|e| {
            OrkError::Internal(format!(
                "failed to write report.json at {}: {e}",
                output_path.display()
            ))
        })?;

        Ok(report)
    }
}

fn aggregate(scores: &[f32]) -> ScorerAggregate {
    if scores.is_empty() {
        return ScorerAggregate::default();
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f32;
    let mut passed = 0usize;
    let mut failed = 0usize;
    for &s in scores {
        if s < min {
            min = s;
        }
        if s > max {
            max = s;
        }
        sum += s;
        if s >= 1.0 {
            passed += 1;
        } else {
            failed += 1;
        }
    }
    ScorerAggregate {
        examples: scores.len(),
        mean: sum / scores.len() as f32,
        min,
        max,
        passed,
        failed,
    }
}

async fn detect_regressions(
    baseline_path: &Path,
    current: &HashMap<String, ScorerAggregate>,
    threshold: f32,
) -> Result<Vec<RegressionRow>, OrkError> {
    let raw = fs::read_to_string(baseline_path).await.map_err(|e| {
        OrkError::Validation(format!(
            "failed to read baseline {}: {e}",
            baseline_path.display()
        ))
    })?;
    let baseline: EvalReport = serde_json::from_str(&raw).map_err(|e| {
        OrkError::Validation(format!(
            "baseline {} is not a valid report.json: {e}",
            baseline_path.display()
        ))
    })?;
    let mut regressions = Vec::new();
    for (id, current_agg) in current {
        if let Some(baseline_agg) = baseline.by_scorer.get(id) {
            let delta = current_agg.mean - baseline_agg.mean;
            if delta < -threshold {
                regressions.push(RegressionRow {
                    scorer_id: id.clone(),
                    baseline_mean: baseline_agg.mean,
                    current_mean: current_agg.mean,
                    delta,
                });
            }
        }
    }
    Ok(regressions)
}

async fn read_jsonl_dataset(path: &Path) -> Result<Vec<EvalExample>, OrkError> {
    let raw = fs::read_to_string(path).await.map_err(|e| {
        OrkError::Validation(format!("failed to read dataset {}: {e}", path.display()))
    })?;
    let mut out = Vec::new();
    for (lineno, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let example: EvalExample = serde_json::from_str(trimmed).map_err(|e| {
            OrkError::Validation(format!("dataset line {}: {e} (raw: {trimmed})", lineno + 1))
        })?;
        out.push(example);
    }
    Ok(out)
}

async fn write_jsonl(path: &Path, results: &[EvalExampleResult]) -> Result<(), OrkError> {
    let mut buf = String::new();
    for r in results {
        let line = serde_json::to_string(r)
            .map_err(|e| OrkError::Internal(format!("failed to serialize per-example row: {e}")))?;
        buf.push_str(&line);
        buf.push('\n');
    }
    fs::write(path, buf).await.map_err(|e| {
        OrkError::Internal(format!(
            "failed to write per-example JSONL at {}: {e}",
            path.display()
        ))
    })
}

/// Best-effort string view of a dataset example's `input`. JSON
/// objects are stringified (so e.g. `{"city":"SF"}` flows verbatim
/// into the user_message field); already-string inputs pass through.
fn chat_input_text(input: &Value) -> String {
    match input {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Per-run [`AgentContext`] used by the offline runner. Tenant id is
/// `Uuid::nil()` because offline runs are not user-bound; the eval
/// process reads from local datasets and writes to local files.
fn test_only_eval_context() -> AgentContext {
    use ork_common::auth::{TrustClass, TrustTier};
    use ork_common::types::TenantId;
    use ork_core::a2a::{CallerIdentity, TaskId};
    AgentContext {
        tenant_id: TenantId(uuid::Uuid::nil()),
        task_id: TaskId::new(),
        parent_task_id: None,
        cancel: tokio_util::sync::CancellationToken::new(),
        caller: CallerIdentity {
            tenant_id: TenantId(uuid::Uuid::nil()),
            user_id: None,
            scopes: vec![],
            tenant_chain: vec![TenantId(uuid::Uuid::nil())],
            trust_tier: TrustTier::Internal,
            trust_class: TrustClass::User,
            agent_id: None,
        },
        push_notification_url: None,
        trace_ctx: None,
        context_id: None,
        workflow_input: Value::Null,
        iteration: None,
        delegation_depth: 0,
        delegation_chain: vec![],
        step_llm_overrides: None,
        artifact_store: None,
        artifact_public_base: None,
        resource_id: None,
        thread_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_on_regression_returns_exit_2() {
        let report = EvalReport {
            regressions: vec![RegressionRow {
                scorer_id: "x".into(),
                baseline_mean: 1.0,
                current_mean: 0.5,
                delta: -0.5,
            }],
            ..Default::default()
        };
        assert_eq!(FailOn::Regression.evaluate(&report), Some(2));
    }

    #[test]
    fn fail_on_score_below_returns_exit_3() {
        let mut by_scorer = HashMap::new();
        by_scorer.insert(
            "x".into(),
            ScorerAggregate {
                examples: 5,
                mean: 0.4,
                min: 0.0,
                max: 1.0,
                passed: 2,
                failed: 3,
            },
        );
        let report = EvalReport {
            by_scorer,
            ..Default::default()
        };
        assert_eq!(FailOn::ScoreBelow(0.6).evaluate(&report), Some(3));
        assert_eq!(FailOn::ScoreBelow(0.3).evaluate(&report), None);
    }

    #[test]
    fn fail_on_failures_above_returns_exit_4() {
        let report = EvalReport {
            failed: 5,
            ..Default::default()
        };
        assert_eq!(FailOn::FailuresAbove(3).evaluate(&report), Some(4));
        assert_eq!(FailOn::FailuresAbove(10).evaluate(&report), None);
    }

    #[test]
    fn aggregate_basic_stats() {
        let agg = aggregate(&[1.0, 0.5, 0.0, 1.0]);
        assert_eq!(agg.examples, 4);
        assert!((agg.mean - 0.625).abs() < f32::EPSILON);
        assert_eq!(agg.min, 0.0);
        assert_eq!(agg.max, 1.0);
        assert_eq!(agg.passed, 2);
        assert_eq!(agg.failed, 2);
    }
}
