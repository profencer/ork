//! ADR-0020 §`Tenant id propagation across delegation`: when minting a
//! mesh token for an outbound A2A call, ork narrows the caller's scopes to
//! the subset the destination [`AgentCard`] declares accepting.
//!
//! Wildcard rules:
//!  - `*` matches anything.
//!  - A single `*` segment in a colon-separated scope (e.g. `agent:*:invoke`)
//!    matches any value in that segment (so the card-accepted entry
//!    `agent:*:invoke` matches `agent:planner:invoke` and
//!    `agent:reviewer:invoke`, but not `agent:planner:delegate`).
//!  - Trailing `:*` matches any single deeper segment (so `agent:planner:*`
//!    matches `agent:planner:invoke` and `agent:planner:delegate`).
//!
//! The intersect is order-preserving on the caller side and de-dup'd by
//! lexical equality on the way out.

/// Intersect `caller`'s scopes with the set of scopes the destination
/// agent card declares it accepts. Order follows `caller`; duplicates are
/// removed.
pub fn intersect_scopes(caller: &[String], card_accepted: &[String]) -> Vec<String> {
    if card_accepted.is_empty() {
        // An empty accept-list is the "wide open" default — intersect is
        // identity. (ADR-0020 ties this to the v1 behaviour for cards that
        // predate the field; v2 cards SHOULD declare an explicit list.)
        return dedup(caller);
    }

    let mut out: Vec<String> = Vec::with_capacity(caller.len());
    for c in caller {
        if card_accepted.iter().any(|a| scope_matches(a, c))
            && !out.iter().any(|existing| existing == c)
        {
            out.push(c.clone());
        }
    }
    out
}

fn dedup(scopes: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(scopes.len());
    for s in scopes {
        if !out.iter().any(|existing: &String| existing == s) {
            out.push(s.clone());
        }
    }
    out
}

/// `pattern` is a card-accepted scope (may contain `*` segments); `value`
/// is a concrete caller scope. Returns true when the card accepts the value.
fn scope_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let p_parts: Vec<&str> = pattern.split(':').collect();
    let v_parts: Vec<&str> = value.split(':').collect();
    if p_parts.len() != v_parts.len() {
        return false;
    }
    p_parts
        .iter()
        .zip(v_parts.iter())
        .all(|(p, v)| p == &"*" || p == v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_card_accept_is_identity() {
        let caller = vec!["a".into(), "b".into()];
        assert_eq!(intersect_scopes(&caller, &[]), caller);
    }

    #[test]
    fn exact_match_keeps_only_listed() {
        let caller = vec![
            "agent:planner:invoke".into(),
            "agent:reviewer:invoke".into(),
            "tool:agent_call:invoke".into(),
        ];
        let card = vec![
            "agent:planner:invoke".into(),
            "tool:agent_call:invoke".into(),
        ];
        let got = intersect_scopes(&caller, &card);
        assert_eq!(
            got,
            vec![
                "agent:planner:invoke".to_string(),
                "tool:agent_call:invoke".to_string(),
            ]
        );
    }

    #[test]
    fn wildcard_segment_in_card_matches_anything() {
        let caller = vec![
            "agent:planner:invoke".into(),
            "agent:reviewer:invoke".into(),
            "agent:planner:delegate".into(),
        ];
        let card = vec!["agent:*:invoke".into()];
        let got = intersect_scopes(&caller, &card);
        assert_eq!(
            got,
            vec![
                "agent:planner:invoke".to_string(),
                "agent:reviewer:invoke".to_string(),
            ]
        );
    }

    #[test]
    fn star_alone_matches_all() {
        let caller = vec!["a".into(), "b".into()];
        let got = intersect_scopes(&caller, &["*".into()]);
        assert_eq!(got, caller);
    }

    #[test]
    fn caller_dedup_is_preserved() {
        let caller = vec!["a:b:c".into(), "a:b:c".into(), "x:y:z".into()];
        let card = vec!["*:*:*".into()];
        let got = intersect_scopes(&caller, &card);
        assert_eq!(got, vec!["a:b:c".to_string(), "x:y:z".to_string()]);
    }

    #[test]
    fn no_match_returns_empty() {
        let caller = vec!["agent:planner:invoke".into()];
        let card = vec!["tool:*:invoke".into()];
        assert!(intersect_scopes(&caller, &card).is_empty());
    }

    #[test]
    fn segment_count_mismatch_does_not_match() {
        // `agent:*` only has 2 segments; `agent:planner:invoke` has 3.
        let caller = vec!["agent:planner:invoke".into()];
        let card = vec!["agent:*".into()];
        assert!(intersect_scopes(&caller, &card).is_empty());
    }
}
