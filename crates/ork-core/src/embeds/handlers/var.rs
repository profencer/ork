//! `«var:name»` — lookup in [`EmbedContext::variables`](../super::super::EmbedContext).

use async_trait::async_trait;
use tracing::warn;

use super::super::{EmbedContext, EmbedHandler, EmbedOutput, EmbedPhase};

/// Built-in `var` embed (ADR-0015).
pub struct VarHandler;

#[async_trait]
impl EmbedHandler for VarHandler {
    fn type_id(&self) -> &'static str {
        "var"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Early
    }

    async fn resolve(
        &self,
        expr: &str,
        _format: Option<&str>,
        ctx: &EmbedContext,
    ) -> Result<EmbedOutput, super::super::EmbedError> {
        let key = expr.trim();
        if key.is_empty() {
            return Ok(EmbedOutput::Text(String::new()));
        }
        if let Some(v) = ctx.variables.get(key) {
            return Ok(EmbedOutput::Text(v.clone()));
        }
        warn!(key = %key, "embed var: no such key; substituting empty string");
        Ok(EmbedOutput::Text(String::new()))
    }
}
