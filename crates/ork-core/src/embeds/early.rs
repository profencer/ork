//! Early-phase embed resolution (before LLM or after template `{{...}}`).

use ork_common::error::OrkError;
use tracing::warn;

use super::{
    EmbedContext, EmbedHandler, EmbedLimits, EmbedOutput, EmbedPhase, EmbedRegistry, parser,
};

/// Resolve `«type:...»` in `input` for early-phase and `Both` handlers; leave unknown/late-only
/// literals in place. Recurses on nested `«»` in expressions up to `limits.max_embed_depth`.
pub async fn resolve_early(
    input: &str,
    ctx: &EmbedContext,
    registry: &EmbedRegistry,
    limits: &EmbedLimits,
) -> Result<String, OrkError> {
    let mut count = 0usize;
    resolve_early_counted(input, ctx, registry, limits, &mut count).await
}

/// Same as [`resolve_early`], but `count` is shared so late-phase + nested early share one
/// `max_embeds_per_request` budget.
pub async fn resolve_early_counted(
    input: &str,
    ctx: &EmbedContext,
    registry: &EmbedRegistry,
    limits: &EmbedLimits,
    count: &mut usize,
) -> Result<String, OrkError> {
    resolve_in_string(input, ctx, registry, limits, count).await
}

async fn resolve_in_string(
    input: &str,
    ctx: &EmbedContext,
    registry: &EmbedRegistry,
    limits: &EmbedLimits,
    count: &mut usize,
) -> Result<String, OrkError> {
    if ctx.depth > limits.max_embed_depth {
        return Err(OrkError::Workflow(format!(
            "embed depth exceeded (max depth {})",
            limits.max_embed_depth
        )));
    }

    let mut out = String::new();
    let mut rest = input;

    while let Some((s, e)) = parser::find_first_embed_span(rest) {
        out.push_str(&rest[..s]);
        let span = &rest[s..e];
        let body = &span[parser::OPEN.len()..span.len() - parser::CLOSE.len()];

        if let Some(parsed) = parser::parse_embed_body(body) {
            if let Some(h) = registry.get(&parsed.type_id) {
                if matches!(h.phase(), EmbedPhase::Early | EmbedPhase::Both) {
                    if *count >= limits.max_embeds_per_request {
                        return Err(OrkError::Workflow(format!(
                            "max embeds per request ({}) exceeded",
                            limits.max_embeds_per_request
                        )));
                    }
                    let inner_ctx = ctx.with_depth(ctx.depth + 1);
                    let expr_r = if parsed.expr.contains(parser::OPEN) {
                        Box::pin(resolve_in_string(
                            &parsed.expr,
                            &inner_ctx,
                            registry,
                            limits,
                            count,
                        ))
                        .await?
                    } else {
                        parsed.expr.clone()
                    };
                    *count += 1;
                    let fmt = parsed.format.as_deref();
                    let text = run_handler(h.as_ref(), &expr_r, fmt, ctx, &parsed.type_id).await?;
                    // If handler output contains embeds (rare), expand one more pass (same counter).
                    let after = if text.contains(parser::OPEN) {
                        Box::pin(resolve_in_string(&text, ctx, registry, limits, count)).await?
                    } else {
                        text
                    };
                    out.push_str(&after);
                    rest = &rest[e..];
                    continue;
                }
            } else {
                warn!(embed_type = %parsed.type_id, "unknown embed type; keeping literal");
            }
        } else {
            warn!("malformed embed body; keeping literal");
        }

        // Unknown, malformed, or late-only handler: preserve literal
        out.push_str(span);
        rest = &rest[e..];
    }
    out.push_str(rest);
    Ok(out)
}

async fn run_handler(
    h: &dyn EmbedHandler,
    expr: &str,
    format: Option<&str>,
    ctx: &EmbedContext,
    type_id: &str,
) -> Result<String, OrkError> {
    let out = h
        .resolve(expr, format, ctx)
        .await
        .map_err(|e| OrkError::Workflow(format!("embed {type_id}: {e}")))?;
    match out {
        EmbedOutput::Text(s) => Ok(s),
        EmbedOutput::Parts(_) => Err(OrkError::Workflow(format!(
            "embed {type_id}: Parts output in early resolution is not supported"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::EmbedContext;
    use super::resolve_early;
    use crate::embeds::{EmbedLimits, EmbedRegistry};

    use chrono::Utc;
    use ork_common::types::TenantId;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[tokio::test]
    async fn unknown_type_preserved() {
        let reg = EmbedRegistry::with_builtins();
        let ctx = EmbedContext {
            tenant_id: TenantId(Uuid::new_v4()),
            task_id: None,
            a2a_repo: None,
            now: Utc::now(),
            variables: HashMap::new(),
            depth: 0,
        };
        let s = resolve_early("a «nope:xx» b", &ctx, &reg, &EmbedLimits::default())
            .await
            .unwrap();
        assert_eq!(s, "a «nope:xx» b");
    }
}
