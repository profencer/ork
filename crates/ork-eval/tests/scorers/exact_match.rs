use ork_core::ports::scorer::{RunId, RunKind, ScoreInput};
use ork_eval::scorers::exact_match;

use super::common;

#[tokio::test]
async fn passes_on_exact_string_match() {
    let scorer = exact_match().expected_string("sunny").build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "sunny",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
    assert_eq!(card.label.as_deref(), Some("match"));
}

#[tokio::test]
async fn fails_on_mismatch() {
    let scorer = exact_match().expected_string("sunny").build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "rainy",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 0.0);
    assert_eq!(card.label.as_deref(), Some("miss"));
}

#[tokio::test]
async fn case_insensitive_mode_matches_across_cases() {
    let scorer = exact_match()
        .expected_string("Sunny")
        .case_sensitive(false)
        .build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "u",
            final_response: "sunny",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
}

#[tokio::test]
async fn pulls_expected_from_dataset_field() {
    let scorer = exact_match().expected_field("answer").build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let expected = serde_json::json!({"answer": "42"});
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("answers"),
            workflow_id: None,
            user_message: "what is the answer?",
            final_response: "42",
            trace: &trace,
            expected: Some(&expected),
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
}
