//! [`ModelSpec`] — `provider/model` selector for [`CodeAgent`](crate::code_agent::CodeAgent)
//! (ADR [`0052`](../../docs/adrs/0052-code-first-agent-dsl.md)).
//!
//! Resolution falls through ADR [`0012`](../../docs/adrs/0012-multi-llm-providers.md) §`Selection`:
//! per-step → per-agent (this value) → tenant default → operator default.

/// Provider / model pair fed into [`AgentConfig`](ork_core::models::agent::AgentConfig).
///
/// `"openai/gpt-4o-mini"` parses into `provider = Some("openai")`, `model = Some("gpt-4o-mini")`.
/// A bare model name (no slash) parses into `provider = None`, `model = Some(name)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl ModelSpec {
    #[must_use]
    pub fn new(provider: Option<String>, model: Option<String>) -> Self {
        Self { provider, model }
    }

    fn from_str(s: &str) -> Self {
        match s.split_once('/') {
            Some((p, m)) if !p.is_empty() && !m.is_empty() => Self {
                provider: Some(p.to_string()),
                model: Some(m.to_string()),
            },
            _ if !s.is_empty() => Self {
                provider: None,
                model: Some(s.to_string()),
            },
            _ => Self::default(),
        }
    }
}

impl From<&str> for ModelSpec {
    fn from(s: &str) -> Self {
        Self::from_str(s)
    }
}

impl From<String> for ModelSpec {
    fn from(s: String) -> Self {
        Self::from_str(&s)
    }
}

impl From<&String> for ModelSpec {
    fn from(s: &String) -> Self {
        Self::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_slash_model() {
        let s: ModelSpec = "openai/gpt-4o-mini".into();
        assert_eq!(s.provider.as_deref(), Some("openai"));
        assert_eq!(s.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn bare_name_is_model_only() {
        let s: ModelSpec = "gpt-4o-mini".into();
        assert_eq!(s.provider, None);
        assert_eq!(s.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn empty_provider_falls_back_to_bare() {
        let s: ModelSpec = "/gpt-4o-mini".into();
        assert_eq!(s.provider, None);
        assert_eq!(s.model, Some("/gpt-4o-mini".into()));
    }

    #[test]
    fn empty_string_yields_default() {
        let s: ModelSpec = "".into();
        assert_eq!(s, ModelSpec::default());
    }
}
