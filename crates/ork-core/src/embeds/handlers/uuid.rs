//! `«uuid:...»` — random UUID (v4).

use async_trait::async_trait;
use uuid::Uuid;

use super::super::{EmbedContext, EmbedHandler, EmbedOutput, EmbedPhase};

/// Built-in `uuid` embed (ADR-0015).
pub struct UuidHandler;

#[async_trait]
impl EmbedHandler for UuidHandler {
    fn type_id(&self) -> &'static str {
        "uuid"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Both
    }

    async fn resolve(
        &self,
        _expr: &str,
        _format: Option<&str>,
        _ctx: &EmbedContext,
    ) -> Result<EmbedOutput, super::super::EmbedError> {
        Ok(EmbedOutput::Text(Uuid::new_v4().to_string()))
    }
}
