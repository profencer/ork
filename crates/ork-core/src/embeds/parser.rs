//! `«type:expr | format»` parsing (U+00AB / U+00BB) — ADR-0015.

/// Opening guillemet «
pub const OPEN: &str = "\u{00AB}";
/// Closing guillemet »
pub const CLOSE: &str = "\u{00BB}";

/// Returns byte index of the `»` that matches `open_idx` (the byte index of `«`).
#[must_use]
pub fn find_matching_close(s: &str, open_idx: usize) -> Option<usize> {
    if !s[open_idx..].starts_with(OPEN) {
        return None;
    }
    let mut depth = 1usize;
    let mut i = open_idx + OPEN.len();
    while i < s.len() {
        if s[i..].starts_with(OPEN) {
            depth += 1;
            i += OPEN.len();
        } else if s[i..].starts_with(CLOSE) {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(i);
            }
            i += CLOSE.len();
        } else {
            let c = s.get(i..)?.chars().next()?;
            i += c.len_utf8();
        }
    }
    None
}

/// First complete embed `[start, end)` where `end` is exclusive, covering `«` through the byte after `»`.
#[must_use]
pub fn find_first_embed_span(s: &str) -> Option<(usize, usize)> {
    let start = s.find(OPEN)?;
    let close_start = find_matching_close(s, start)?;
    let end = close_start + CLOSE.len();
    Some((start, end))
}

/// Parsed `«body»` (without the guillemets). `type_id` and `format` are trimmed; `expr` is not trimmed
/// so that whitespace is preserved for `math` / etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEmbed {
    pub type_id: String,
    pub expr: String,
    pub format: Option<String>,
}

/// `body` is the text between `«` and `»` (excluding delimiters).
#[must_use]
pub fn parse_embed_body(body: &str) -> Option<ParsedEmbed> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if let Some((type_id, after_colon)) = body.split_once(':') {
        let type_id = type_id.trim();
        if type_id.is_empty() {
            return None;
        }
        let (expr, format) = match after_colon.split_once(" | ") {
            Some((e, f)) => (e.to_string(), Some(f.trim().to_string())),
            None => (after_colon.to_string(), None),
        };
        return Some(ParsedEmbed {
            type_id: type_id.to_string(),
            expr,
            format,
        });
    }
    // e.g. `«uuid»` or `«datetime»` — no `:expr` (SAM allows zero-arg embeds)
    Some(ParsedEmbed {
        type_id: body.to_string(),
        expr: String::new(),
        format: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_span_simple() {
        let s = "x «math:1+1 | int» y";
        let (a, b) = find_first_embed_span(s).unwrap();
        assert_eq!(&s[a..b], "«math:1+1 | int»");
    }

    #[test]
    fn find_nested() {
        let s = "«outer:«inner:1»»";
        let (a, b) = find_first_embed_span(s).unwrap();
        assert_eq!(&s[a..b], s);
        let body = &s[a + OPEN.len()..b - CLOSE.len()];
        assert_eq!(body, "outer:«inner:1»");
    }

    #[test]
    fn unbalanced_no_match() {
        let s = "no « guillemet or «only open";
        assert!(find_first_embed_span(s).is_none());
    }

    #[test]
    fn utf8_in_body() {
        let s = "«var:thünberg»";
        let (a, b) = find_first_embed_span(s).unwrap();
        let body = &s[a + OPEN.len()..b - CLOSE.len()];
        let p = parse_embed_body(body).unwrap();
        assert_eq!(p.type_id, "var");
        assert_eq!(p.expr, "thünberg");
    }

    #[test]
    fn parse_with_format() {
        let p = parse_embed_body("math:2*3 | int").unwrap();
        assert_eq!(p.type_id, "math");
        assert_eq!(p.expr, "2*3");
        assert_eq!(p.format.as_deref(), Some("int"));
    }

    #[test]
    fn parse_zero_arg_type() {
        let p = parse_embed_body("uuid").unwrap();
        assert_eq!(p.type_id, "uuid");
        assert!(p.expr.is_empty());
    }
}
