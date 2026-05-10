//! Live scorers and offline eval corpus
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)).
//!
//! This crate hosts:
//! - The concrete `Scorer` value types and built-in scorers (the trait
//!   itself lives in [`ork_core::ports::scorer`]).
//! - The live-sampling background worker driven by the agent /
//!   workflow completion hooks introduced in this ADR.
//! - The offline `OrkEval` runner consumed by the `ork eval` CLI.
//!
//! Per ADR-0054, `ork-eval` does **not** depend on `axum`, `reqwest`,
//! `rmcp`, or `rskafka`. Enforced by
//! [`crates/ork-eval/tests/boundaries.rs`](../../crates/ork-eval/tests/boundaries.rs).

pub mod agent_hook;
pub mod metrics;
pub mod sampling;
pub mod spec;

pub mod live;
pub mod runner;
pub mod scorers;

pub use agent_hook::{LiveAgentScoringHook, LiveBinding};
pub use live::{
    DEFAULT_QUEUE_CAPACITY, InMemoryScorerResultSink, LiveSamplerHandle, ScoreJob, ScoredRow,
    ScorerResultSink, spawn_worker,
};

pub use metrics::ScorerMetrics;
pub use sampling::Sampling;
pub use spec::{ScorerSpec, ScorerTarget};

pub use ork_core::ports::scorer::{
    RunId, RunKind, ScoreCard, ScoreInput, ScoreSchema, Scorer, ToolCallRecord, Trace,
    TraceCapture, TraceCaptureHandle,
};
