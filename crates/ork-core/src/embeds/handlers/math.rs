//! `«math:expr | format»` — numeric evaluation with `evalexpr`.

use async_trait::async_trait;

use super::super::{EmbedContext, EmbedError, EmbedHandler, EmbedOutput, EmbedPhase};

/// Built-in `math` embed (ADR-0015).
pub struct MathHandler;

#[async_trait]
impl EmbedHandler for MathHandler {
    fn type_id(&self) -> &'static str {
        "math"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Early
    }

    async fn resolve(
        &self,
        expr: &str,
        format: Option<&str>,
        _ctx: &EmbedContext,
    ) -> Result<EmbedOutput, EmbedError> {
        let v = evalexpr::eval(expr.trim())
            .map_err(|e| EmbedError::InvalidExpression(format!("{e} (in {expr:?})")))?;
        let n = v
            .as_number()
            .map_err(|e| EmbedError::InvalidExpression(format!("{e}")))?;
        let s = match format.map(str::to_lowercase).as_deref() {
            None | Some("") | Some("float") => format!("{n}"),
            Some("int") => format_int(n),
            Some("percent") => format!("{}%", n * 100.0),
            Some("usd") => format!("${n:.2}"),
            Some("bytes") => format_bytes(n),
            Some(other) => {
                return Err(EmbedError::InvalidFormat(other.to_string()));
            }
        };
        Ok(EmbedOutput::Text(s))
    }
}

fn format_int(n: f64) -> String {
    format!("{}", n.round() as i64)
}

fn format_bytes(n: f64) -> String {
    let mut v = n.abs();
    if v < 0.0 {
        return "0 B".to_string();
    }
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}
