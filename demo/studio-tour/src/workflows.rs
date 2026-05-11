//! Demo workflow: `feedback-triage`.
//!
//! Three steps and a branch:
//!
//! ```text
//!   TriageInput { message }
//!     │
//!     ▼  classify  (analyst agent, real LLM call returning JSON)
//!   ClassifyOutput { message, sentiment, reason }
//!     │
//!     ▼  branch on `sentiment`
//!     ├── negative → draft-apology  (concierge agent)
//!     ├── positive → draft-thanks   (concierge agent)
//!     └── neutral  → acknowledge    (deterministic)
//!     │
//!     ▼  finalize  (re-types the branch's serde_json::Value accumulator)
//!   TriageOutput { sentiment, reason, response_text, tone }
//! ```
//!
//! Why this shape: the workflow exercises the three things
//! ork-workflow's `StepContext` makes available — tool calls, agent
//! calls, and conditional branching driven by an LLM classifier. The
//! two terminal branches each issue their own LLM call against the
//! `concierge` agent, so a single workflow run lights up two real
//! model invocations.

use ork_common::error::OrkError;
use ork_workflow::{AnyStep, Step, StepOutcome, step, workflow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Sentiment {
    Positive,
    Negative,
    Neutral,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct TriageInput {
    /// Customer message to triage.
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct ClassifyOutput {
    /// Echoed so the branch arms see the original message.
    pub message: String,
    pub sentiment: Sentiment,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct TriageOutput {
    pub message: String,
    pub sentiment: Sentiment,
    pub reason: String,
    pub tone: String,
    pub response_text: String,
}

/// Step 1 — ask the `analyst` agent to classify and explain.
///
/// The prompt is fully self-contained so the analyst's existing system
/// instructions don't have to know about the triage task; any tightly-
/// scoped reply-with-JSON instruction wins regardless. Best-effort
/// parse: if the model wraps the JSON in code fences or omits a field,
/// we fall back to Neutral and surface the raw text as the `reason`.
fn classify_step() -> Step<TriageInput, ClassifyOutput> {
    step("classify")
        .description("Classify the incoming message as positive / negative / neutral via the analyst agent.")
        .input::<TriageInput>()
        .output::<ClassifyOutput>()
        .uses_agent("analyst")
        .execute(|ctx, input| async move {
            let prompt = format!(
                "Classify the sentiment of the customer message below. \
                 Reply with EXACTLY one JSON object on a single line, no commentary, no code fences:\n\
                 {{\"sentiment\":\"positive|negative|neutral\",\"reason\":\"<1 short sentence>\"}}\n\
                 ---\n\
                 Message: {message}",
                message = input.message,
            );
            let raw = ctx
                .agents
                .run(ctx.agent_context.clone(), "analyst", prompt)
                .await?;
            let (sentiment, reason) = parse_classification(&raw);
            tracing::info!(
                workflow_id = "feedback-triage",
                step = "classify",
                sentiment = ?sentiment,
                reason = %reason,
                "feedback-triage: classified"
            );
            Ok::<_, OrkError>(StepOutcome::Done(ClassifyOutput {
                message: input.message,
                sentiment,
                reason,
            }))
        })
}

/// Best-effort extraction. Order:
/// 1. Find a `{ ... }` JSON object and parse it.
/// 2. Fall back to keyword matching (`negative` / `positive`) over the
///    raw text.
/// 3. Default to Neutral and surface the trimmed model text as
///    the rationale.
fn parse_classification(raw: &str) -> (Sentiment, String) {
    if let Some(start) = raw.find('{')
        && let Some(end) = raw.rfind('}')
        && end > start
    {
        let slice = &raw[start..=end];
        if let Ok(v) = serde_json::from_str::<Value>(slice) {
            let sentiment = v
                .get("sentiment")
                .and_then(|s| s.as_str())
                .map(parse_sentiment_str)
                .unwrap_or(Sentiment::Neutral);
            let reason = v
                .get("reason")
                .and_then(|s| s.as_str())
                .unwrap_or("(no reason given)")
                .to_string();
            return (sentiment, reason);
        }
    }
    let lower = raw.to_ascii_lowercase();
    let sentiment = if lower.contains("negative") {
        Sentiment::Negative
    } else if lower.contains("positive") {
        Sentiment::Positive
    } else {
        Sentiment::Neutral
    };
    let reason = format!("model returned non-JSON: {}", raw.trim());
    (sentiment, reason)
}

fn parse_sentiment_str(s: &str) -> Sentiment {
    match s.trim().to_ascii_lowercase().as_str() {
        "positive" => Sentiment::Positive,
        "negative" => Sentiment::Negative,
        _ => Sentiment::Neutral,
    }
}

/// Branch helper — build a step that invokes `agent_id` with the
/// branch's templated prompt and wraps the response in the converged
/// `TriageOutput` shape. All branch arms must emit the same type so the
/// post-branch `finalize` step can type the accumulator back.
fn draft_step(
    id: &'static str,
    description: &'static str,
    agent_id: &'static str,
    tone: &'static str,
    prompt_template: &'static str,
) -> Step<ClassifyOutput, TriageOutput> {
    step(id)
        .description(description)
        .input::<ClassifyOutput>()
        .output::<TriageOutput>()
        .uses_agent(agent_id)
        .execute(move |ctx, input| async move {
            let prompt = prompt_template
                .replace("{message}", &input.message)
                .replace("{reason}", &input.reason);
            let response = ctx
                .agents
                .run(ctx.agent_context.clone(), agent_id, prompt)
                .await?;
            let out = TriageOutput {
                message: input.message,
                sentiment: input.sentiment,
                reason: input.reason,
                tone: tone.to_string(),
                response_text: response.trim().to_string(),
            };
            tracing::info!(
                workflow_id = "feedback-triage",
                step = id,
                tone = tone,
                response = %out.response_text,
                "feedback-triage: drafted response"
            );
            Ok::<_, OrkError>(StepOutcome::Done(out))
        })
}

fn apology_step() -> Step<ClassifyOutput, TriageOutput> {
    draft_step(
        "draft-apology",
        "Negative-sentiment branch: draft a one-paragraph apology via the concierge agent.",
        "concierge",
        "apology",
        "You are responding to a customer complaint. \
         Reason for classification: {reason}. \
         Customer message: \"{message}\". \
         Reply with a short, empathetic apology (2 sentences max). \
         Acknowledge the issue, do not promise specific remedies. \
         Do not call any tools.",
    )
}

fn thanks_step() -> Step<ClassifyOutput, TriageOutput> {
    draft_step(
        "draft-thanks",
        "Positive-sentiment branch: draft a brief thank-you via the concierge agent.",
        "concierge",
        "thanks",
        "You are responding to positive customer feedback. \
         Reason for classification: {reason}. \
         Customer message: \"{message}\". \
         Reply with a warm, brief thank-you (1-2 sentences). \
         Do not call any tools.",
    )
}

/// Neutral arm: deterministic, no LLM. Records the message and moves on.
fn acknowledge_step() -> Step<ClassifyOutput, TriageOutput> {
    step("acknowledge")
        .description("Neutral-sentiment branch: log + no LLM response.")
        .input::<ClassifyOutput>()
        .output::<TriageOutput>()
        .execute(|_ctx, input| async move {
            Ok::<_, OrkError>(StepOutcome::Done(TriageOutput {
                message: input.message,
                sentiment: input.sentiment,
                reason: input.reason,
                tone: "noted".into(),
                response_text: "Noted. Routed to the inbox for human review.".into(),
            }))
        })
}

/// After `.branch(...)` the workflow accumulator is `serde_json::Value`
/// (the branch DSL erases the typed output to allow heterogeneous arms).
/// This step re-types it so the workflow's declared output is honoured.
fn finalize_step() -> Step<Value, TriageOutput> {
    step("finalize")
        .description("Re-type the branch accumulator into the workflow's TriageOutput.")
        .input::<Value>()
        .output::<TriageOutput>()
        .execute(|_ctx, value| async move {
            let out: TriageOutput = serde_json::from_value(value).map_err(|e| {
                OrkError::Internal(format!(
                    "feedback-triage finalize: branch accumulator did not match TriageOutput: {e}"
                ))
            })?;
            tracing::info!(
                workflow_id = "feedback-triage",
                step = "finalize",
                sentiment = ?out.sentiment,
                tone = %out.tone,
                response = %out.response_text,
                "feedback-triage workflow completed"
            );
            Ok::<_, OrkError>(StepOutcome::Done(out))
        })
}

pub fn feedback_triage_workflow() -> ork_workflow::Workflow {
    use ork_workflow::types::BranchPredicate;

    let branch_arms = vec![
        (
            BranchPredicate::new(|_ctx, v| sentiment_is(v, Sentiment::Negative)),
            AnyStep::from_step(apology_step()),
        ),
        (
            BranchPredicate::new(|_ctx, v| sentiment_is(v, Sentiment::Positive)),
            AnyStep::from_step(thanks_step()),
        ),
        (
            BranchPredicate::new(|_ctx, v| sentiment_is(v, Sentiment::Neutral)),
            AnyStep::from_step(acknowledge_step()),
        ),
    ];

    workflow("feedback-triage")
        .description(
            "LLM-classified customer message triage: classify sentiment, branch into apology / \
             thanks / acknowledge, return the drafted response. Two real LLM calls per run \
             (one classification, one drafted reply).",
        )
        .input::<TriageInput>()
        .output::<TriageOutput>()
        .then(classify_step())
        .branch(branch_arms)
        .then(finalize_step())
        .commit()
}

fn sentiment_is(v: &Value, expected: Sentiment) -> bool {
    v.get("sentiment")
        .and_then(|s| s.as_str())
        .map(parse_sentiment_str)
        == Some(expected)
}
