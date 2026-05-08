//! ADR-0020 §`Tenant id propagation across delegation`: when minting a
//! mesh token for an outbound A2A call, ork narrows the caller's scopes to
//! the subset the destination [`AgentCard`] declares accepting.
//!
//! ADR-0021 §`Vocabulary` adds the runtime checker [`ScopeChecker`] that
//! every authorisation point in ork-api / ork-agents / ork-integrations /
//! ork-storage / ork-llm / ork-gateways calls. The matcher is shared with
//! [`intersect_scopes`] so a scope shape that's accepted at mint-time is
//! also accepted at check-time.
//!
//! Wildcard rules:
//!  - `*` alone is the root-admin wildcard reserved for the `tenant:root`
//!    operator scope. ADR-0021 forbids any *granted* scope that is `*:*:*`
//!    (would silently grant everything); see [`ScopeChecker::validate_format`].
//!  - A single `*` segment in a colon-separated scope (e.g. `agent:*:invoke`)
//!    matches any value in that segment (so the card-accepted / granted entry
//!    `agent:*:invoke` matches `agent:planner:invoke` and
//!    `agent:reviewer:invoke`, but not `agent:planner:delegate`).
//!  - Trailing `:*` matches any single deeper segment (so `agent:planner:*`
//!    matches `agent:planner:invoke` and `agent:planner:delegate`).
//!
//! The intersect is order-preserving on the caller side and de-dup'd by
//! lexical equality on the way out.

use ork_common::error::OrkError;

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

/// `pattern` is a card-accepted / granted scope (may contain `*` segments);
/// `value` is a concrete caller / required scope. Returns true when the
/// pattern matches the value.
///
/// Public-in-crate so [`ScopeChecker::allows`] reuses the same rules as the
/// outbound `intersect_scopes` mint-time path. ADR-0021 §`Wildcards and
/// hierarchy`.
pub fn scope_matches(pattern: &str, value: &str) -> bool {
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

/// Runtime authorisation checker. ADR-0021 §`ScopeChecker`.
///
/// `ScopeChecker` is a stateless namespace: it carries no policy of its own
/// — every authorisation decision is a pure function of the caller's
/// scopes and the required scope. The token issuer (DevPortal / mesh-token
/// signer) owns the role → scope mapping; ork is the runtime, not the IAM.
pub struct ScopeChecker;

impl ScopeChecker {
    /// `true` if any granted scope matches `required`. Granted scopes may
    /// contain `*` segments; the required scope is a concrete string. (A
    /// caller is not expected to *request* a wildcard — wildcards only
    /// appear in the granted set as a way to express "any value".)
    #[must_use]
    pub fn allows(scopes: &[String], required: &str) -> bool {
        scopes
            .iter()
            .any(|granted| scope_matches(granted, required))
    }

    /// `Ok` if the granted set covers the required scope, else
    /// [`OrkError::Forbidden`] with a structured message that callers map
    /// to a 403 response. The error string starts with `missing scope ` so
    /// existing tests (and the audit-log query convention) can pivot on it.
    ///
    /// ADR-0021 §`Audit`: every deny emits a `tracing::info!` event with
    /// `event = "audit.scope_denied"`. Callers that need richer fields
    /// (principal id, tenant id, request id) should layer their own log
    /// on top — the [`crate::audit::SCOPE_DENIED_EVENT`] constant is the
    /// canonical event name to pivot SIEM queries on.
    pub fn require(scopes: &[String], required: &str) -> Result<(), OrkError> {
        if Self::allows(scopes, required) {
            Ok(())
        } else {
            tracing::info!(
                scope = %required,
                event = crate::audit::SCOPE_DENIED_EVENT,
                "ADR-0021 audit"
            );
            Err(OrkError::Forbidden(format!("missing scope {required}")))
        }
    }

    /// Pre-validate a scope string at config / mint time. Catches the
    /// shapes ADR-0021 forbids before they reach a granted-set:
    ///
    /// - empty string,
    /// - empty segments (e.g. `agent::invoke`),
    /// - a granted scope of `*:*:*` (would silently grant everything;
    ///   the only legal "everything" scope is the `tenant:root` operator
    ///   sentinel — itself plain text, not a wildcard).
    ///
    /// Used by the config loader and the per-tenant allowlist setter.
    /// Returns `Err(reason)` so the caller can surface the offending
    /// string in operator-facing logs.
    pub fn validate_format(scope: &str) -> Result<(), String> {
        if scope.is_empty() {
            return Err("scope is empty".into());
        }
        if scope == "*" {
            return Err("`*` alone is reserved for the `tenant:root` operator sentinel — declare the literal `tenant:root` instead".into());
        }
        let parts: Vec<&str> = scope.split(':').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(format!(
                "scope `{scope}` has empty segments; declare each segment explicitly"
            ));
        }
        // `*:*:*` (and any all-wildcard shape) is the silent-grant trap
        // ADR-0021 §`Wildcards and hierarchy` forbids. Pinned at format
        // validation so a misconfigured token can never be minted with it.
        if parts.iter().all(|p| *p == "*") {
            return Err(format!(
                "scope `{scope}` would grant everything; only `tenant:root` may stand in for the operator wildcard"
            ));
        }
        Ok(())
    }
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

    // ---------- ADR-0021 §`ScopeChecker` ----------

    #[test]
    fn checker_allows_exact_match() {
        let granted = vec!["agent:planner:invoke".into()];
        assert!(ScopeChecker::allows(&granted, "agent:planner:invoke"));
    }

    #[test]
    fn checker_allows_via_segment_wildcard() {
        let granted = vec!["agent:*:invoke".into()];
        assert!(ScopeChecker::allows(&granted, "agent:planner:invoke"));
        assert!(ScopeChecker::allows(
            &granted,
            "agent:vendor.scanner:invoke"
        ));
        assert!(!ScopeChecker::allows(&granted, "agent:planner:delegate"));
    }

    #[test]
    fn checker_allows_via_trailing_wildcard() {
        let granted = vec!["agent:planner:*".into()];
        assert!(ScopeChecker::allows(&granted, "agent:planner:invoke"));
        assert!(ScopeChecker::allows(&granted, "agent:planner:cancel"));
        assert!(!ScopeChecker::allows(&granted, "agent:reviewer:invoke"));
    }

    #[test]
    fn checker_allows_root_wildcard() {
        let granted = vec!["*".into()];
        assert!(ScopeChecker::allows(&granted, "agent:planner:invoke"));
        assert!(ScopeChecker::allows(&granted, "tenant:admin"));
    }

    #[test]
    fn checker_denies_when_segment_count_differs() {
        let granted = vec!["agent:*".into()];
        assert!(!ScopeChecker::allows(&granted, "agent:planner:invoke"));
    }

    #[test]
    fn checker_require_returns_forbidden_with_required_scope_in_message() {
        let granted = vec!["tenant:self".into()];
        let err = ScopeChecker::require(&granted, "agent:planner:invoke").unwrap_err();
        match err {
            OrkError::Forbidden(msg) => {
                assert!(
                    msg.contains("agent:planner:invoke"),
                    "error message must surface the required scope, got `{msg}`"
                );
                assert!(
                    msg.starts_with("missing scope "),
                    "audit-log convention: message starts with `missing scope `, got `{msg}`"
                );
            }
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[test]
    fn checker_require_ok_when_granted_covers_required() {
        let granted = vec!["tool:*:invoke".into()];
        assert!(ScopeChecker::require(&granted, "tool:agent_call:invoke").is_ok());
    }

    #[test]
    fn validate_format_rejects_empty() {
        assert!(ScopeChecker::validate_format("").is_err());
    }

    #[test]
    fn validate_format_rejects_empty_segments() {
        assert!(ScopeChecker::validate_format("agent::invoke").is_err());
        assert!(ScopeChecker::validate_format(":agent:invoke").is_err());
        assert!(ScopeChecker::validate_format("agent:invoke:").is_err());
    }

    #[test]
    fn validate_format_rejects_all_wildcard_grant() {
        // ADR-0021 §`Wildcards and hierarchy`: `*:*:*` is forbidden — it
        // would silently grant everything. Only `tenant:root` is allowed
        // to stand in for the operator wildcard.
        assert!(ScopeChecker::validate_format("*:*:*").is_err());
        assert!(ScopeChecker::validate_format("*:*").is_err());
    }

    #[test]
    fn validate_format_rejects_bare_star() {
        // The single `*` (root admin sentinel) must be expressed as the
        // explicit `tenant:root` literal so audit logs can flag it.
        assert!(ScopeChecker::validate_format("*").is_err());
    }

    #[test]
    fn validate_format_accepts_canonical_shapes() {
        for s in [
            "tenant:admin",
            "tenant:self",
            "tenant:root",
            "agent:planner:invoke",
            "agent:*:invoke",
            "agent:planner:*",
            "tool:agent_call:invoke",
            "tool:mcp:atlassian.search_jira:invoke",
            "tool:mcp:atlassian.*:invoke",
            "artifact:tenant:read",
            "artifact:context-abc:write",
            "model:openai:gpt-4o:invoke",
            "gateway:slack-acme:invoke",
            "schedule:read",
            "schedule:write",
            "webui:access",
            "ops:read",
        ] {
            assert!(
                ScopeChecker::validate_format(s).is_ok(),
                "canonical scope `{s}` must validate"
            );
        }
    }
}
