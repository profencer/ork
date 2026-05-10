//! Per-scorer integration tests
//! ([ADR-0054](../../docs/adrs/0054-live-scorers-and-eval-corpus.md)).
//!
//! Cargo only treats top-level `tests/*.rs` files as test crates; the
//! per-scorer modules live under `tests/scorers/` and are pulled in
//! here as one logical test binary.

mod scorers {
    pub mod common;

    pub mod answer_relevancy;
    pub mod cost_under;
    pub mod exact_match;
    pub mod json_schema_match;
    pub mod latency_under;
    pub mod tool_calls_recall;
}
