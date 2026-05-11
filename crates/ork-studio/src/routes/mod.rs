//! ADR-0055 §`Studio API (introspection-only)` route modules.
//!
//! Each sub-module exposes `pub fn routes() -> axum::Router` so
//! [`crate::router`] can `.merge(...)` them. Handlers always return
//! the [`crate::envelope::StudioEnvelope`] for JSON responses.

pub mod deferred;
pub mod evals;
pub mod manifest;
pub mod memory;
pub mod scorers;
