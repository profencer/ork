//! ADR-0054 acceptance criterion `OrkEval runner`
//! ([live-scorers ADR](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)).
//!
//! Drives the runner against a 3-example JSONL dataset using a
//! scripted `EvalRunner`. Asserts:
//! - the report has the expected aggregates,
//! - one regression row vs an embedded baseline,
//! - the per-example JSONL exists alongside `report.json`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::scorer::Trace;
use ork_eval::runner::{
    EvalExample, EvalReport, EvalRunOutput, EvalRunner, FailOn, OrkEval, RegressionRow,
    ScorerAggregate,
};
use ork_eval::scorers::exact_match;
use serde_json::json;
use tempfile::TempDir;
use tokio::fs;

struct ScriptedAgent;

#[async_trait]
impl EvalRunner for ScriptedAgent {
    async fn dispatch(
        &self,
        _ctx: &AgentContext,
        example: &EvalExample,
    ) -> Result<EvalRunOutput, OrkError> {
        // The dataset's `input.answer` is what the agent returns —
        // simulates a perfect agent for `ex-001` and `ex-002`, and
        // a buggy one for `ex-003`.
        let answer = example
            .input
            .get("answer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let trace = Trace {
            user_message: example.id.clone(),
            tool_calls: vec![],
            started_at: chrono::Utc::now(),
            completed_at: chrono::Utc::now(),
        };
        Ok(EvalRunOutput {
            final_response: answer,
            trace,
        })
    }
}

#[tokio::test]
async fn runs_three_examples_and_reports_one_regression() {
    let tmp = TempDir::new().unwrap();
    let dataset_path = tmp.path().join("data.jsonl");
    let baseline_path = tmp.path().join("baseline.json");
    let output_path = tmp.path().join("report.json");

    // Dataset: three rows, two pass exact_match against `expected.value`,
    // one fails (the agent's `input.answer` is wrong on purpose).
    let dataset = "\
{\"id\":\"ex-001\",\"input\":{\"answer\":\"42\"},\"expected\":{\"value\":\"42\"}}
{\"id\":\"ex-002\",\"input\":{\"answer\":\"hello\"},\"expected\":{\"value\":\"hello\"}}
{\"id\":\"ex-003\",\"input\":{\"answer\":\"WRONG\"},\"expected\":{\"value\":\"sunny\"}}
";
    fs::write(&dataset_path, dataset).await.unwrap();

    // Baseline: scorer mean was 1.0 last time. This run will be ~0.667
    // → delta < -0.05, regression detected.
    let mut baseline_by = HashMap::new();
    baseline_by.insert(
        "exact_match".to_string(),
        ScorerAggregate {
            examples: 3,
            mean: 1.0,
            min: 1.0,
            max: 1.0,
            passed: 3,
            failed: 0,
        },
    );
    let baseline = EvalReport {
        examples: 3,
        passed: 3,
        failed: 0,
        by_scorer: baseline_by,
        regressions: vec![],
        raw_path: tmp.path().join("baseline.jsonl"),
    };
    let baseline_json = serde_json::to_string_pretty(&baseline).unwrap();
    fs::write(&baseline_path, baseline_json).await.unwrap();

    let scorer = exact_match().expected_field("value").build();

    let report = OrkEval::new()
        .dataset(&dataset_path)
        .target_agent("scripted")
        .scorer(scorer)
        .baseline(&baseline_path)
        .output(&output_path)
        .runner(Arc::new(ScriptedAgent))
        .run()
        .await
        .expect("runner produced report");

    assert_eq!(report.examples, 3);
    assert_eq!(report.passed, 2);
    assert_eq!(report.failed, 1);
    let agg = report
        .by_scorer
        .get("exact_match")
        .expect("scorer aggregate present");
    assert_eq!(agg.examples, 3);
    assert!((agg.mean - 2.0 / 3.0).abs() < 1e-5);
    assert_eq!(report.regressions.len(), 1);
    let RegressionRow {
        scorer_id,
        baseline_mean,
        current_mean,
        delta,
    } = &report.regressions[0];
    assert_eq!(scorer_id, "exact_match");
    assert!((baseline_mean - 1.0).abs() < 1e-5);
    assert!((current_mean - 2.0 / 3.0).abs() < 1e-5);
    assert!(*delta < 0.0);

    // `--fail-on regression` would exit 2 here.
    assert_eq!(FailOn::Regression.evaluate(&report), Some(2));

    // Per-example JSONL must exist alongside the report.
    let jsonl = report.raw_path.clone();
    let raw = fs::read_to_string(&jsonl).await.unwrap();
    let lines: Vec<_> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["example_id"], json!("ex-001"));

    // report.json on disk should round-trip.
    let on_disk = fs::read_to_string(&output_path).await.unwrap();
    let parsed: EvalReport = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(parsed.examples, 3);
}

#[tokio::test]
async fn no_baseline_means_no_regressions() {
    let tmp = TempDir::new().unwrap();
    let dataset_path = tmp.path().join("d.jsonl");
    let output_path = tmp.path().join("r.json");
    fs::write(
        &dataset_path,
        "{\"id\":\"e1\",\"input\":{\"answer\":\"x\"},\"expected\":{\"value\":\"x\"}}\n",
    )
    .await
    .unwrap();
    let scorer = exact_match().expected_field("value").build();

    let report = OrkEval::new()
        .dataset(&dataset_path)
        .target_agent("scripted")
        .scorer(scorer)
        .output(&output_path)
        .runner(Arc::new(ScriptedAgent))
        .run()
        .await
        .unwrap();
    assert_eq!(report.regressions.len(), 0);
}
