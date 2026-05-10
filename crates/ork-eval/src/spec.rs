//! Registration types
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)
//! §`Surface registration`).
//!
//! `OrkAppBuilder::scorer(target, spec)` consumes these. Live and
//! offline registrations share the same scorer instance — `Both` is
//! the common case for "score in prod and in CI."

use std::sync::Arc;

use ork_core::ports::scorer::Scorer;
use serde::{Deserialize, Serialize};

use crate::sampling::Sampling;

/// Where a scorer attaches.
///
/// Serialised in the app manifest (ADR-0049) and in `scorer_results`
/// rows for traceability; `Arc<dyn Scorer>` is excluded by sitting on
/// [`ScorerSpec`], which is *not* `Serialize`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScorerTarget {
    /// A specific registered agent by id.
    Agent { id: String },
    /// A specific registered workflow by id.
    Workflow { id: String },
    /// Every agent registered on the app.
    AgentEverywhere,
    /// Glob pattern matched against agent ids (e.g. `"weather-*"`).
    Wildcard { pattern: String },
}

impl ScorerTarget {
    /// Convenience constructor matching the ADR's example shape.
    #[must_use]
    pub fn agent(id: impl Into<String>) -> Self {
        Self::Agent { id: id.into() }
    }

    /// Convenience constructor matching the ADR's example shape.
    #[must_use]
    pub fn workflow(id: impl Into<String>) -> Self {
        Self::Workflow { id: id.into() }
    }

    #[must_use]
    pub fn wildcard(pattern: impl Into<String>) -> Self {
        Self::Wildcard {
            pattern: pattern.into(),
        }
    }
}

impl ScorerTarget {
    /// True when this target should fire on the agent run identified
    /// by `agent_id`.
    #[must_use]
    pub fn matches_agent(&self, agent_id: &str) -> bool {
        match self {
            Self::Agent { id } => id == agent_id,
            Self::AgentEverywhere => true,
            Self::Wildcard { pattern } => glob::Pattern::new(pattern)
                .map(|p| p.matches(agent_id))
                .unwrap_or(false),
            Self::Workflow { .. } => false,
        }
    }

    /// True when this target should fire on the workflow run
    /// identified by `workflow_id`.
    #[must_use]
    pub fn matches_workflow(&self, workflow_id: &str) -> bool {
        match self {
            Self::Workflow { id } => id == workflow_id,
            _ => false,
        }
    }
}

/// How a scorer is consumed: live (sampled production traffic),
/// offline (replay against a dataset), or both with a single
/// instance.
#[derive(Clone)]
pub enum ScorerSpec {
    Live {
        scorer: Arc<dyn Scorer>,
        sampling: Sampling,
    },
    Offline {
        scorer: Arc<dyn Scorer>,
    },
    Both {
        scorer: Arc<dyn Scorer>,
        sampling: Sampling,
    },
}

impl ScorerSpec {
    #[must_use]
    pub fn scorer(&self) -> &Arc<dyn Scorer> {
        match self {
            Self::Live { scorer, .. } | Self::Offline { scorer } | Self::Both { scorer, .. } => {
                scorer
            }
        }
    }

    #[must_use]
    pub fn sampling(&self) -> Option<&Sampling> {
        match self {
            Self::Live { sampling, .. } | Self::Both { sampling, .. } => Some(sampling),
            Self::Offline { .. } => None,
        }
    }

    #[must_use]
    pub fn fires_live(&self) -> bool {
        matches!(self, Self::Live { .. } | Self::Both { .. })
    }

    #[must_use]
    pub fn fires_offline(&self) -> bool {
        matches!(self, Self::Offline { .. } | Self::Both { .. })
    }

    #[must_use]
    pub fn live(scorer: Arc<dyn Scorer>, sampling: Sampling) -> Self {
        Self::Live { scorer, sampling }
    }

    #[must_use]
    pub fn offline(scorer: Arc<dyn Scorer>) -> Self {
        Self::Offline { scorer }
    }

    #[must_use]
    pub fn both(scorer: Arc<dyn Scorer>, sampling: Sampling) -> Self {
        Self::Both { scorer, sampling }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches_glob() {
        let t = ScorerTarget::wildcard("weather-*");
        assert!(t.matches_agent("weather-eu"));
        assert!(t.matches_agent("weather-us"));
        assert!(!t.matches_agent("billing"));
        assert!(!t.matches_workflow("weather-eu"));
    }

    #[test]
    fn agent_everywhere_matches_any_agent() {
        let t = ScorerTarget::AgentEverywhere;
        assert!(t.matches_agent("anything"));
        assert!(!t.matches_workflow("anything"));
    }

    #[test]
    fn workflow_target_only_matches_workflow_ids() {
        let t = ScorerTarget::workflow("ingest");
        assert!(t.matches_workflow("ingest"));
        assert!(!t.matches_workflow("other"));
        assert!(!t.matches_agent("ingest"));
    }

    #[test]
    fn target_round_trips_through_json() {
        let cases = [
            ScorerTarget::agent("weather"),
            ScorerTarget::workflow("ingest"),
            ScorerTarget::AgentEverywhere,
            ScorerTarget::wildcard("weather-*"),
        ];
        for t in cases {
            let json = serde_json::to_string(&t).expect("serialize");
            let back: ScorerTarget = serde_json::from_str(&json).expect("deserialize");
            // round-trip preserves agent matching
            match (&t, &back) {
                (ScorerTarget::Agent { id: a }, ScorerTarget::Agent { id: b }) => assert_eq!(a, b),
                (ScorerTarget::Workflow { id: a }, ScorerTarget::Workflow { id: b }) => {
                    assert_eq!(a, b)
                }
                (ScorerTarget::AgentEverywhere, ScorerTarget::AgentEverywhere) => {}
                (ScorerTarget::Wildcard { pattern: a }, ScorerTarget::Wildcard { pattern: b }) => {
                    assert_eq!(a, b)
                }
                _ => panic!("variant changed across round-trip: {t:?} -> {back:?}"),
            }
        }
    }
}
