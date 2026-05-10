use ork_core::ports::scorer::{RunId, RunKind, ScoreInput, ToolCallRecord};
use ork_eval::scorers::ToolCallExpectation;
use ork_eval::scorers::tool_calls_recall;
use serde_json::json;

use super::common;

fn call(name: &str, args: serde_json::Value) -> ToolCallRecord {
    ToolCallRecord {
        name: name.into(),
        args,
        result: serde_json::Value::Null,
        duration_ms: 1,
        error: None,
    }
}

#[tokio::test]
async fn vacuous_when_no_expectations_configured() {
    let scorer = tool_calls_recall(vec![]).build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a"),
            workflow_id: None,
            user_message: "u",
            final_response: "r",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
    assert_eq!(card.label.as_deref(), Some("vacuous"));
}

#[tokio::test]
async fn perfect_recall_when_all_expectations_match() {
    let scorer = tool_calls_recall(vec![
        ToolCallExpectation::new("get_weather"),
        ToolCallExpectation::new("get_uv_index"),
    ])
    .build();
    let ctx = common::ctx();
    let trace = common::trace(vec![
        call("get_weather", json!({"city": "SF"})),
        call("get_uv_index", json!({"lat": 37, "lon": -122})),
    ]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "r",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
    assert_eq!(card.details["matched"], 2);
    assert_eq!(card.details["misses"], json!([]));
}

#[tokio::test]
async fn partial_recall_records_misses() {
    let scorer = tool_calls_recall(vec![
        ToolCallExpectation::new("get_weather"),
        ToolCallExpectation::new("get_uv_index"),
    ])
    .build();
    let ctx = common::ctx();
    let trace = common::trace(vec![call("get_weather", json!({"city": "SF"}))]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "r",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert!((card.score - 0.5).abs() < f32::EPSILON);
    assert_eq!(card.details["misses"], json!(["get_uv_index"]));
}

#[tokio::test]
async fn args_predicate_filters_matches() {
    let scorer = tool_calls_recall(vec![
        ToolCallExpectation::new("get_weather").with_args(json!({"city": "SF"})),
    ])
    .build();
    let ctx = common::ctx();
    // The agent called the tool with a different city — recall should miss.
    let trace = common::trace(vec![call("get_weather", json!({"city": "NYC"}))]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "r",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 0.0);
}
