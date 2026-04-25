//! Application-level wiring on top of [`ork_eventing`].
//!
//! As of ADR-0005, the discovery publisher/subscriber live in `ork_eventing::discovery` and
//! are wired up directly from `main.rs` (one publisher per local agent + one subscriber per
//! process). Future submodules (SSE bridge, push outbox, delegation handler) will land here.
