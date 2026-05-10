use ork_core::ports::scorer::{RunId, RunKind, ScoreInput, ToolCallRecord};
use ork_eval::scorers::cost_under;
use serde_json::json;

use super::common;

fn cost_call(cost_usd: f64) -> ToolCallRecord {
    ToolCallRecord {
        name: "tool".into(),
        args: serde_json::Value::Null,
        result: json!({"cost_usd": cost_usd}),
        duration_ms: 1,
        error: None,
    }
}

#[tokio::test]
async fn returns_unknown_when_no_cost_reported() {
    let scorer = cost_under(0.10).build();
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
    assert_eq!(card.label.as_deref(), Some("unknown"));
    assert_eq!(card.score, 0.0);
}

#[tokio::test]
async fn passes_when_total_under_budget() {
    let scorer = cost_under(0.10).build();
    let ctx = common::ctx();
    let trace = common::trace(vec![cost_call(0.04), cost_call(0.05)]);
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
    assert_eq!(card.label.as_deref(), Some("under"));
    assert_eq!(card.score, 1.0);
}

#[tokio::test]
async fn fails_when_total_exceeds_budget() {
    let scorer = cost_under(0.05).build();
    let ctx = common::ctx();
    let trace = common::trace(vec![cost_call(0.04), cost_call(0.05)]);
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
    assert_eq!(card.label.as_deref(), Some("over"));
    assert_eq!(card.score, 0.0);
}
