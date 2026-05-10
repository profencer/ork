//! ADR-0054 acceptance criterion `Built-in scorers` —
//! `answer_relevancy` integration test.
//!
//! Uses the in-crate `ScriptedJudge` (defined locally to avoid
//! pulling `cfg(test)` modules into the integration crate) to drive
//! the scorer with a known [`JudgeOutput`] and asserts the resulting
//! `ScoreCard` mirrors the ADR's `(score, rationale)` contract.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::scorer::{RunId, RunKind, ScoreInput};
use ork_eval::scorers::answer_relevancy;
use ork_eval::scorers::judge::{Judge, JudgeOutput, JudgeResponse, JudgeUsage};

use super::common;

struct ScriptedJudge {
    model: String,
    outputs: Mutex<Vec<JudgeOutput>>,
    usage: JudgeUsage,
}

#[async_trait]
impl Judge for ScriptedJudge {
    fn judge_model(&self) -> &str {
        &self.model
    }
    async fn judge(&self, _prompt: &str) -> Result<JudgeResponse, OrkError> {
        let mut g = self.outputs.lock().expect("scripted judge poisoned");
        Ok(JudgeResponse {
            output: g.remove(0),
            usage: self.usage.clone(),
        })
    }
}

#[tokio::test]
async fn surfaces_judge_score_and_rationale_on_score_card() {
    let judge: Arc<dyn Judge> = Arc::new(ScriptedJudge {
        model: "openai/gpt-4o-mini".into(),
        outputs: Mutex::new(vec![JudgeOutput {
            score: 0.9,
            rationale: "directly answers the question".into(),
        }]),
        usage: JudgeUsage {
            prompt_tokens: Some(120),
            completion_tokens: Some(35),
        },
    });
    let scorer = answer_relevancy().judge(judge).build();

    let ctx = common::ctx();
    let trace = common::trace(vec![]);
    let card = scorer
        .score(&ScoreInput {
            run_id: RunId::new(),
            run_kind: RunKind::Agent,
            agent_id: Some("weather"),
            workflow_id: None,
            user_message: "is it warm in SF?",
            final_response: "yes, around 70F",
            trace: &trace,
            expected: None,
            context: &ctx,
        })
        .await
        .unwrap();

    assert!((card.score - 0.9).abs() < 1e-5);
    assert!(card.rationale.as_deref().unwrap().contains("directly"));
    // Judge metadata must surface for `scorer_results.judge_*` columns.
    assert_eq!(card.details["judge_model"], "openai/gpt-4o-mini");
    assert_eq!(card.details["judge_input_tokens"], 120);
    assert_eq!(card.details["judge_output_tokens"], 35);
}

#[tokio::test]
async fn judge_model_override_wins_over_judges_native_label() {
    let judge: Arc<dyn Judge> = Arc::new(ScriptedJudge {
        model: "underlying/judge".into(),
        outputs: Mutex::new(vec![JudgeOutput {
            score: 1.0,
            rationale: "ok".into(),
        }]),
        usage: JudgeUsage::default(),
    });
    let scorer = answer_relevancy()
        .judge(judge)
        .judge_model("override/model")
        .build();

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

    assert_eq!(card.details["judge_model"], "override/model");
}

#[tokio::test]
async fn try_build_without_judge_returns_typed_configuration_error() {
    let res = answer_relevancy().try_build();
    let err = match res {
        Ok(_) => panic!("expected Configuration error when judge is missing"),
        Err(e) => e,
    };
    assert!(matches!(err, OrkError::Configuration { .. }));
}
