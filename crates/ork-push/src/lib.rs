//! ADR-0009 push notifications and webhook signing.
//!
//! This crate implements the outbound side of A2A push notifications:
//!
//! - [`encryption`] — HKDF-SHA256 KEK derivation and an AES-256-GCM envelope
//!   helper used to seal private signing keys at rest.
//! - [`signing`] — ES256 keypair generation, [`signing::JwksProvider`] (the
//!   public face for `/.well-known/jwks.json`), the detached JWS signer used
//!   on every outbound request, and `rotate_if_due`.
//! - [`outbox`] — the small JSON envelope written to
//!   `ork.a2a.v1.push.outbox` whenever a task reaches a terminal state.
//! - [`worker`] — the in-process tokio task that consumes the outbox, signs
//!   each payload, POSTs it with the documented headers, retries on failure,
//!   and writes to `a2a_push_dead_letter` once the budget is exhausted.
//! - [`janitor`] — periodic GC of `a2a_push_configs` for terminal tasks past
//!   the configured lifetime.
//!
//! [`PushService`] is the small façade `ork-api` holds in `AppState` to
//! publish terminal-state envelopes from the JSON-RPC dispatcher.

pub mod encryption;
pub mod janitor;
pub mod outbox;
pub mod signing;
pub mod worker;

pub use outbox::{PushOutboxEnvelope, PushService};
pub use signing::{JwksProvider, SigningKeyMaterial};
