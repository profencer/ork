use ork_core::ports::scorer::{RunId, RunKind, ScoreInput};
use ork_eval::scorers::json_schema_match;
use serde_json::json;

use super::common;

const PERSON_SCHEMA: fn() -> serde_json::Value = || {
    json!({
        "type": "object",
        "required": ["name", "age"],
        "properties": {
            "name": { "type": "string" },
            "age":  { "type": "integer", "minimum": 0 }
        }
    })
};

#[tokio::test]
async fn valid_json_against_schema_scores_one() {
    let scorer = json_schema_match(PERSON_SCHEMA()).build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a"),
            workflow_id: None,
            user_message: "u",
            final_response: r#"{"name":"Ada","age":36}"#,
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 1.0);
    assert_eq!(card.label.as_deref(), Some("valid"));
}

#[tokio::test]
async fn invalid_json_scores_zero_with_parse_stage_detail() {
    let scorer = json_schema_match(PERSON_SCHEMA()).build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a"),
            workflow_id: None,
            user_message: "u",
            final_response: "not json",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 0.0);
    assert_eq!(card.details["stage"], "parse");
}

#[tokio::test]
async fn schema_violation_scores_zero() {
    let scorer = json_schema_match(PERSON_SCHEMA()).build();
    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("a"),
            workflow_id: None,
            user_message: "u",
            final_response: r#"{"name":"Ada"}"#,
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();
    assert_eq!(card.score, 0.0);
    assert_eq!(card.label.as_deref(), Some("invalid"));
}
