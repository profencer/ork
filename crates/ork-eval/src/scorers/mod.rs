//! Built-in scorers
//! ([ADR-0054](../../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Built-in scorers`).
//!
//! Concrete impls land in the per-scorer modules; a scorer-specific
//! todo step fills each one in. The trait itself lives in
//! [`ork_core::ports::scorer::Scorer`]; the builders here return
//! `Arc<dyn Scorer>` so registrations (`OrkAppBuilder::scorer`) accept
//! any of them uniformly.

pub mod cost_under;
pub mod exact_match;
pub mod json_schema_match;
pub mod latency_under;
pub mod tool_calls_recall;

pub mod answer_relevancy;
pub mod faithfulness;
pub mod judge;
pub mod llm_judge;
pub mod toxicity;

pub use judge::{Judge, JudgeOutput, JudgeResponse, JudgeUsage};
pub use llm_judge::LlmProviderJudge;

pub use answer_relevancy::{AnswerRelevancyBuilder, answer_relevancy};
pub use cost_under::{CostUnderBuilder, cost_under};
pub use exact_match::{ExactMatchBuilder, exact_match};
pub use faithfulness::{FaithfulnessBuilder, faithfulness};
pub use json_schema_match::{JsonSchemaMatchBuilder, json_schema_match};
pub use latency_under::{LatencyUnderBuilder, latency_under};
pub use tool_calls_recall::{ToolCallExpectation, ToolCallsRecallBuilder, tool_calls_recall};
pub use toxicity::{ToxicityBuilder, toxicity};
