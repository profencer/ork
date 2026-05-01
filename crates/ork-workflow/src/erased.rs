use async_trait::async_trait;
use ork_common::error::OrkError;
use serde_json::Value;

use crate::types::StepContext;

/// Outcome of executing a single step — complete or suspend for HITL / async wait.
#[derive(Debug, Clone)]
pub enum StepOutcome<O> {
    Done(O),
    Suspend {
        payload: Value,
        resume_schema: Value,
    },
}

#[async_trait]
pub trait ErasedStep: Send + Sync {
    fn id(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn output_schema(&self) -> Value;
    fn tool_refs(&self) -> &[String];
    fn agent_refs(&self) -> &[String];
    fn max_attempts(&self) -> u32;
    fn timeout(&self) -> Option<std::time::Duration>;

    async fn run(&self, ctx: StepContext, input: Value) -> Result<StepOutcome<Value>, OrkError>;
}
