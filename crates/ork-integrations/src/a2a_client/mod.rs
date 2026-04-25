//! A2A 1.0 remote-agent client (ADR-0007).
//!
//! Layers, in dependency order:
//!
//! - [`config`] — static client knobs (timeouts, retry policy, refresh interval).
//! - [`auth`] — five auth variants + OAuth2 `client_credentials` token cache.
//! - [`sse`] — SSE frame parser turning a `reqwest::Response` body stream into
//!   `ork_a2a::TaskEvent`s.
//! - [`card_fetch`] — Redis-backed `/.well-known/agent-card.json` fetcher.
//! - [`agent`] — [`A2aRemoteAgent`] (the [`Agent`] impl).
//! - [`builder`] — [`A2aRemoteAgentBuilder`] implementing the
//!   [`RemoteAgentBuilder`] port shared by static config, discovery, and inline
//!   workflow cards.

pub mod agent;
pub mod auth;
pub mod builder;
pub mod card_fetch;
pub mod config;
pub mod sse;

pub use agent::A2aRemoteAgent;
pub use auth::{
    A2aAuth, CcTokenCache, DEFAULT_API_KEY_HEADER, TokenProvider, apply_auth,
    fetch_client_credentials_token,
};
pub use builder::A2aRemoteAgentBuilder;
pub use card_fetch::CardFetcher;
pub use config::{A2aClientConfig, RetryPolicy};
pub use sse::parse_a2a_sse;
