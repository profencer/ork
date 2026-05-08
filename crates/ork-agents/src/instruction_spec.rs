//! [`InstructionSpec`] — system-prompt source for [`CodeAgent`](crate::code_agent::CodeAgent)
//! (ADR [`0052`](../../docs/adrs/0052-code-first-agent-dsl.md)).
//!
//! Phase 1 carries only the static variant; the dynamic resolver lands in Phase 3
//! alongside `dynamic_model` / `dynamic_tools`.

#[derive(Clone, Debug)]
pub enum InstructionSpec {
    Static(String),
}

impl InstructionSpec {
    #[must_use]
    pub fn as_static(&self) -> Option<&str> {
        match self {
            Self::Static(s) => Some(s.as_str()),
        }
    }
}

impl From<&str> for InstructionSpec {
    fn from(s: &str) -> Self {
        Self::Static(s.to_string())
    }
}

impl From<String> for InstructionSpec {
    fn from(s: String) -> Self {
        Self::Static(s)
    }
}

impl From<&String> for InstructionSpec {
    fn from(s: &String) -> Self {
        Self::Static(s.clone())
    }
}
