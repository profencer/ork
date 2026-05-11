//! `/studio/api/evals/run` — Studio Evals panel runner.
//!
//! ADR-0055 §`Decision`: pick a dataset + target, run, see the
//! [`EvalReport`](ork_eval::EvalReport).
//!
//! v1 is a synchronous long-poll: the panel POSTs and waits for the
//! report. A 3-example fixture typically completes in < 1s with the
//! echo runner; live-LLM datasets are owned by the offline `ork eval`
//! command (ADR-0054 §`Offline OrkEval runner`).
//!
//! v1 deliberately constrains the runner to `EchoRunner` and one
//! built-in scorer (`exact_match`). Live-LLM dispatch and arbitrary
//! scorer DSLs land in a follow-up; the SPA only needs the report
//! shape for now.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::scorer::{Scorer, Trace};
use ork_eval::runner::{EvalExample, EvalReport, EvalRunOutput, EvalRunner, FailOn, OrkEval};
use serde::{Deserialize, Serialize};

use crate::envelope::ok;

pub fn routes() -> Router {
    Router::new().route("/studio/api/evals/run", post(run_eval))
}

#[derive(Debug, Deserialize)]
struct RunRequest {
    /// Filesystem path to the JSONL dataset.
    dataset: PathBuf,
    /// Target agent id (mutually exclusive with `workflow`).
    #[serde(default)]
    agent: Option<String>,
    /// Target workflow id.
    #[serde(default)]
    workflow: Option<String>,
    /// JSON field on the example `input` to echo back as the response.
    /// Defaults to `"prompt"`.
    #[serde(default = "default_echo_field")]
    echo_from: String,
    /// Scorer specs, e.g. `["exact_match=answer"]`. Empty = no scorers.
    #[serde(default)]
    scorers: Vec<String>,
    /// Optional baseline report path for regression detection.
    #[serde(default)]
    baseline: Option<PathBuf>,
    /// Optional `--fail-on` policy; `None` = report only.
    #[serde(default)]
    fail_on: Option<FailOnKind>,
}

fn default_echo_field() -> String {
    "prompt".into()
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FailOnKind {
    Regression,
    ScoreBelow { threshold: f32 },
    FailuresAbove { count: usize },
}

impl FailOnKind {
    fn to_policy(self) -> FailOn {
        match self {
            Self::Regression => FailOn::Regression,
            Self::ScoreBelow { threshold } => FailOn::ScoreBelow(threshold),
            Self::FailuresAbove { count } => FailOn::FailuresAbove(count),
        }
    }
}

#[derive(Debug, Serialize)]
struct RunResponse {
    report: EvalReport,
    fail_on_hit: Option<i32>,
}

async fn run_eval(Json(req): Json<RunRequest>) -> impl IntoResponse {
    match build_and_run(req).await {
        Ok(resp) => ok(resp).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "studio_api_version": crate::envelope::STUDIO_API_VERSION,
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

async fn build_and_run(req: RunRequest) -> Result<RunResponse, OrkError> {
    if req.agent.is_none() && req.workflow.is_none() {
        return Err(OrkError::Validation(
            "POST /studio/api/evals/run: pass either `agent` or `workflow`".into(),
        ));
    }
    if req.agent.is_some() && req.workflow.is_some() {
        return Err(OrkError::Validation(
            "POST /studio/api/evals/run: `agent` and `workflow` are mutually exclusive".into(),
        ));
    }

    // ADR-0055 §`Evals`: per-request output path. A tempdir would
    // leak fixtures across runs; honour the dataset's parent.
    let report_path = req.dataset.with_file_name("studio-eval-report.json");

    let mut eval = OrkEval::new()
        .dataset(&req.dataset)
        .output(&report_path)
        .runner(Arc::new(EchoRunner {
            field: req.echo_from.clone(),
        }));

    if let Some(id) = &req.agent {
        eval = eval.target_agent(id);
    }
    if let Some(id) = &req.workflow {
        eval = eval.target_workflow(id);
    }

    for spec in &req.scorers {
        eval = eval.scorer(parse_scorer_spec(spec)?);
    }

    if let Some(p) = &req.baseline {
        eval = eval.baseline(p);
    }

    let policy: Option<FailOn> = req.fail_on.map(FailOnKind::to_policy);
    let report = eval.run().await?;
    let fail_on_hit = policy.as_ref().and_then(|p| p.evaluate(&report));
    Ok(RunResponse {
        report,
        fail_on_hit,
    })
}

/// Echo-mode runner mirroring `ork-cli/src/eval.rs`. Studio's panel
/// drives synthetic datasets at the moment; live-LLM dispatch is the
/// offline `ork eval` command's job.
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

fn parse_scorer_spec(spec: &str) -> Result<Arc<dyn Scorer>, OrkError> {
    use ork_eval::scorers::{exact_match, json_schema_match};

    let (name, arg) = spec.split_once('=').ok_or_else(|| {
        OrkError::Validation(format!(
            "scorer spec `{spec}` must be `<name>=<arg>` (e.g. `exact_match=value`)"
        ))
    })?;
    match name {
        "exact_match" => Ok(exact_match().expected_field(arg).build()),
        "json_schema_match" => {
            let raw = std::fs::read_to_string(arg)
                .map_err(|e| OrkError::Validation(format!("read schema file {arg}: {e}")))?;
            let schema = serde_json::from_str(&raw)
                .map_err(|e| OrkError::Validation(format!("parse JSON schema at {arg}: {e}")))?;
            Ok(json_schema_match(schema).build())
        }
        other => Err(OrkError::Validation(format!(
            "unknown scorer `{other}` (v1 ships `exact_match`, `json_schema_match`)"
        ))),
    }
}
