use std::time::Duration;

use ork_core::ports::scorer::{RunId, RunKind, ScoreInput};
use ork_eval::scorers::latency_under;

use super::common;

#[tokio::test]
async fn passes_when_run_under_budget() {
    let scorer = latency_under(Duration::from_millis(200)).build();
    let ctx = common::ctx();
    let trace = common::trace_lasting(50, vec![]);
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
async fn fails_when_run_exceeds_budget() {
    let scorer = latency_under(Duration::from_millis(50)).build();
    let ctx = common::ctx();
    let trace = common::trace_lasting(150, vec![]);
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
