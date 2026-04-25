//! Integration tests for embed delimiter parsing (ADR-0015).

use ork_core::embeds::parser::{
    CLOSE, OPEN, find_first_embed_span, find_matching_close, parse_embed_body,
};

#[test]
fn span_across_whitespace() {
    let s = "a «math:1+1 | int» b";
    let (a, b) = find_first_embed_span(s).expect("span");
    assert_eq!(&s[a..b], "«math:1+1 | int»");
}

#[test]
fn matching_close_nested() {
    let s = "«a:«b:1»»";
    let start = s.find(OPEN).unwrap();
    let close = find_matching_close(s, start).expect("close");
    assert_eq!(&s[start..=close + CLOSE.len() - 1], s);
}

#[test]
fn parse_body_split() {
    let p = parse_embed_body("type:expr | fmt").expect("parsed");
    assert_eq!(p.type_id, "type");
    assert_eq!(p.expr, "expr");
    assert_eq!(p.format.as_deref(), Some("fmt"));
}
