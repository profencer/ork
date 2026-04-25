//! Late-phase `«…»` resolution on the agent event stream (ADR-0015).

use std::sync::Arc;

use async_stream::stream;
use futures::StreamExt;
use ork_a2a::FileRef;
use ork_a2a::Message;
use tracing::warn;

use crate::a2a::{AgentEvent, Part};
use crate::embeds::{
    EmbedContext, EmbedLimits, EmbedOutput, EmbedPhase, EmbedRegistry, LATE_EMBED_OUTPUT_TRUNCATED,
    parser, resolve_early_counted,
};
use crate::ports::agent::AgentEventStream;

/// Wraps a [`AgentEventStream`] and expands `«type:…»` in text parts (phase [`EmbedPhase::Late`]
/// or [`EmbedPhase::Both`]) before events reach the client.
pub struct LateEmbedResolver {
    registry: Arc<EmbedRegistry>,
    base_ctx: Arc<EmbedContext>,
    limits: EmbedLimits,
}

impl LateEmbedResolver {
    #[must_use]
    pub fn new(
        registry: Arc<EmbedRegistry>,
        base_ctx: Arc<EmbedContext>,
        limits: EmbedLimits,
    ) -> Self {
        Self {
            registry,
            base_ctx,
            limits,
        }
    }

    /// Wraps the stream, buffering `Part::Text` across events so embeds split across chunk
    /// boundaries can still be detected.
    #[must_use]
    pub fn wrap(self, stream: AgentEventStream) -> AgentEventStream {
        let mut stream = stream;
        let registry = self.registry;
        let base_ctx = self.base_ctx;
        let limits = self.limits;

        let mut text_carry: String = String::new();
        let mut embeds_used: usize = 0;
        let mut output_bytes: usize = 0;

        let out = stream! {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(AgentEvent::Message(m)) => {
                        let (msg, carry, eu, ob) = process_message(
                            m,
                            text_carry,
                            embeds_used,
                            output_bytes,
                            &registry,
                            &base_ctx,
                            &limits,
                        ).await;
                        text_carry = carry;
                        embeds_used = eu;
                        output_bytes = ob;
                        yield Ok(AgentEvent::Message(msg));
                    }
                    other => yield other,
                }
            }
        };
        Box::pin(out)
    }
}

async fn process_message(
    mut msg: Message,
    mut text_carry: String,
    mut embeds_used: usize,
    mut output_bytes: usize,
    registry: &EmbedRegistry,
    base_ctx: &EmbedContext,
    limits: &EmbedLimits,
) -> (Message, String, usize, usize) {
    let mut new_parts: Vec<Part> = Vec::new();
    for p in std::mem::take(&mut msg.parts) {
        match p {
            Part::Text { text, metadata } => {
                text_carry.push_str(&text);
                let drained = drain_late_buffer(
                    &mut text_carry,
                    &mut embeds_used,
                    &mut output_bytes,
                    registry,
                    base_ctx,
                    limits,
                )
                .await;
                for part in drained {
                    new_parts.push(part_with_metadata(part, &metadata));
                }
            }
            other => {
                if !text_carry.is_empty() {
                    new_parts.push(Part::Text {
                        text: std::mem::take(&mut text_carry),
                        metadata: None,
                    });
                }
                new_parts.push(other);
            }
        }
    }
    msg.parts = new_parts;
    (msg, text_carry, embeds_used, output_bytes)
}

fn part_with_metadata(part: Part, parent: &Option<ork_a2a::JsonObject>) -> Part {
    match part {
        Part::Text { text, .. } => Part::Text {
            text,
            metadata: parent.clone(),
        },
        o => o,
    }
}

fn add_bytes(n: usize, output_bytes: &mut usize, limits: &EmbedLimits, out: &mut Vec<Part>) {
    *output_bytes = (*output_bytes).saturating_add(n);
    if *output_bytes > limits.max_late_embed_output_bytes {
        warn!("late embed: max_late_embed_output_bytes exceeded; truncating");
        *output_bytes = limits.max_late_embed_output_bytes;
        out.push(Part::text(LATE_EMBED_OUTPUT_TRUNCATED));
    }
}

/// Caps nested-early (or plain expression) size so it cannot blow past the late output budget
/// before the late handler runs.
fn cap_expr_to_late_room(expr: String, room: usize) -> String {
    if expr.len() <= room {
        return expr;
    }
    warn!("late embed: nested expression output capped to remaining late byte budget");
    let mut end = room;
    while end > 0 && !expr.is_char_boundary(end) {
        end -= 1;
    }
    expr[..end].to_string()
}

async fn drain_late_buffer(
    buf: &mut String,
    embeds_used: &mut usize,
    output_bytes: &mut usize,
    registry: &EmbedRegistry,
    base_ctx: &EmbedContext,
    limits: &EmbedLimits,
) -> Vec<Part> {
    let mut out: Vec<Part> = Vec::new();
    let mut work = std::mem::take(buf);

    while let Some((s, e)) = parser::find_first_embed_span(&work) {
        if s > 0 {
            let prefix = work[..s].to_string();
            add_bytes(prefix.len(), output_bytes, limits, &mut out);
            if *output_bytes > limits.max_late_embed_output_bytes {
                *buf = work;
                return out;
            }
            out.push(Part::text(prefix));
        }
        let span = work[s..e].to_string();
        work = work[e..].to_string();

        let body = &span[parser::OPEN.len()..span.len() - parser::CLOSE.len()];
        let Some(parsed) = parser::parse_embed_body(body) else {
            add_bytes(span.len(), output_bytes, limits, &mut out);
            if *output_bytes > limits.max_late_embed_output_bytes {
                *buf = work;
                return out;
            }
            out.push(Part::text(span));
            continue;
        };

        let Some(h) = registry.get(&parsed.type_id) else {
            warn!(embed_type = %parsed.type_id, "late embed: unknown type; keeping literal");
            add_bytes(span.len(), output_bytes, limits, &mut out);
            if *output_bytes > limits.max_late_embed_output_bytes {
                *buf = work;
                return out;
            }
            out.push(Part::text(span));
            continue;
        };
        if matches!(h.phase(), EmbedPhase::Early) {
            add_bytes(span.len(), output_bytes, limits, &mut out);
            if *output_bytes > limits.max_late_embed_output_bytes {
                *buf = work;
                return out;
            }
            out.push(Part::text(span));
            continue;
        }

        if *embeds_used >= limits.max_embeds_per_request {
            warn!("late embed: max_embeds_per_request exceeded; keeping literal");
            out.push(Part::text(span));
            continue;
        }

        let room = limits
            .max_late_embed_output_bytes
            .saturating_sub(*output_bytes);
        if room == 0 {
            warn!("late embed: no room left in late output budget; keeping literal");
            out.push(Part::text(span));
            continue;
        }

        let inner_ctx = base_ctx.with_depth(base_ctx.depth + 1);
        let expr_r = if parsed.expr.contains(parser::OPEN) {
            match resolve_early_counted(&parsed.expr, &inner_ctx, registry, limits, embeds_used)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "late embed: failed to expand nested early embeds; keeping span");
                    out.push(Part::text(span));
                    continue;
                }
            }
        } else {
            parsed.expr.clone()
        };
        if *embeds_used >= limits.max_embeds_per_request {
            warn!(
                "late embed: max_embeds_per_request exceeded after nested early; keeping literal"
            );
            out.push(Part::text(span));
            continue;
        }
        let expr_r = cap_expr_to_late_room(expr_r, room);
        *embeds_used += 1;

        let res = h.resolve(&expr_r, parsed.format.as_deref(), base_ctx).await;
        let res = match res {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "late embed: handler error; keeping span");
                out.push(Part::text(span));
                continue;
            }
        };

        match res {
            EmbedOutput::Text(t) => {
                add_bytes(t.len(), output_bytes, limits, &mut out);
                if *output_bytes > limits.max_late_embed_output_bytes {
                    *buf = work;
                    return out;
                }
                out.push(Part::text(t));
            }
            EmbedOutput::Parts(ps) => {
                for p in ps {
                    let n = part_byte_len(&p);
                    add_bytes(n, output_bytes, limits, &mut out);
                    if *output_bytes > limits.max_late_embed_output_bytes {
                        *buf = work;
                        return out;
                    }
                    out.push(p);
                }
            }
        }
    }

    // No complete `«…»` left in work
    if work.is_empty() {
        *buf = work;
        return out;
    }
    if work.contains(parser::OPEN) && parser::find_first_embed_span(&work).is_none() {
        if work.len() > limits.max_late_embed_buffer_bytes {
            add_bytes(work.len(), output_bytes, limits, &mut out);
            if *output_bytes > limits.max_late_embed_output_bytes {
                *buf = String::new();
                return out;
            }
            out.push(Part::text(work));
            *buf = String::new();
            return out;
        }
        *buf = work;
        return out;
    }
    add_bytes(work.len(), output_bytes, limits, &mut out);
    if *output_bytes > limits.max_late_embed_output_bytes {
        *buf = String::new();
        return out;
    }
    out.push(Part::text(work));
    *buf = String::new();
    out
}

fn part_byte_len(p: &Part) -> usize {
    match p {
        Part::Text { text, .. } => text.len(),
        Part::Data { data, .. } => data.to_string().len(),
        Part::File { file, .. } => match file {
            FileRef::Bytes { bytes, .. } => bytes.0.len(),
            FileRef::Uri { uri, .. } => uri.as_str().len(),
        },
    }
}
