//! `«datetime:... | format»` — wall clock from [`EmbedContext::now`](super::super::EmbedContext).

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::super::{EmbedContext, EmbedError, EmbedHandler, EmbedOutput, EmbedPhase};

/// Built-in `datetime` embed (ADR-0015).
pub struct DateTimeHandler;

#[async_trait]
impl EmbedHandler for DateTimeHandler {
    fn type_id(&self) -> &'static str {
        "datetime"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Early
    }

    async fn resolve(
        &self,
        expr: &str,
        format: Option<&str>,
        ctx: &EmbedContext,
    ) -> Result<EmbedOutput, EmbedError> {
        let _ = expr;
        let now: DateTime<Utc> = ctx.now;
        let s = match format.map(str::trim).filter(|s| !s.is_empty()) {
            None => now.to_rfc3339(),
            Some(f) => {
                if f.contains('%') {
                    // strftime: preserve `%Y` / `%m` / etc.
                    return Ok(EmbedOutput::Text(now.format(f).to_string()));
                }
                match f.to_lowercase().as_str() {
                    "iso8601" => now.to_rfc3339(),
                    "unix" => now.timestamp().to_string(),
                    "human" => now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                    other => {
                        return Err(EmbedError::InvalidFormat(other.to_string()));
                    }
                }
            }
        };
        Ok(EmbedOutput::Text(s))
    }
}
