//! Backend-resolution helpers for [`super::Memory`].
//!
//! [`MemoryOptions`], [`SemanticRecallConfig`], [`Scope`] and
//! [`WorkingMemoryShape`] live in `ork-core::ports::memory_store` so
//! `ork-agents` can consume them without depending on this crate's
//! storage feature flags. This module hosts only the builder-side
//! helpers ([`EmbedderSpec`]).

use serde::{Deserialize, Serialize};

pub use ork_core::ports::memory_store::{
    MemoryOptions, Scope, SemanticRecallConfig, WorkingMemoryShape,
};

/// Static identifier for an embedding model (`"<provider>/<model>"`).
/// v1 is informational — the actual [`Embedder`](ork_core::ports::memory_store::Embedder)
/// is wired via [`super::MemoryBuilder::embedder`]. Surfaces stay aligned
/// with ADR 0012 so a follow-up `LlmRouter` integration can drop in
/// without renaming the public API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedderSpec {
    pub provider: String,
    pub model: String,
}

impl EmbedderSpec {
    /// Parse a `"<provider>/<model>"` slug, e.g.
    /// `"openai/text-embedding-3-small"`.
    #[must_use]
    pub fn parse(slug: &str) -> Option<Self> {
        let (p, m) = slug.split_once('/')?;
        if p.is_empty() || m.is_empty() {
            return None;
        }
        Some(Self {
            provider: p.to_string(),
            model: m.to_string(),
        })
    }
}

impl<S: AsRef<str>> From<S> for EmbedderSpec {
    fn from(s: S) -> Self {
        Self::parse(s.as_ref()).unwrap_or_else(|| Self {
            provider: "openai".into(),
            model: s.as_ref().to_string(),
        })
    }
}
