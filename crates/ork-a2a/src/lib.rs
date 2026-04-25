//! Agent2Agent (A2A) **1.0** wire types, JSON-RPC envelopes, and per-method `params`/`result` structs.
//!
//! This crate is the Rust mirror of the Google [A2A spec](https://github.com/google/a2a) and of
//! Solace Agent Mesh’s [`common/a2a/`](https://github.com/SolaceLabs/solace-agent-mesh/tree/main/src/solace_agent_mesh/common/a2a)
//! (`a2a-sdk` types in Python). See ADR `docs/adrs/0003-a2a-protocol-model.md`.
//!
//! ## Layout
//!
//! - **Value types** — [`AgentCard`], [`Part`], [`Message`], [`Task`], [`TaskEvent`], etc.
//! - **JSON-RPC** — `JsonRpcRequest` / `JsonRpcResponse`, [`A2aMethod`], [`JsonRpcError`] codes.
//! - **Per-method params** — [`MessageSendParams`], [`SendMessageResult`], [`TaskQueryParams`], [`PushNotificationConfig`], …
//! - **IDs** — [`TaskId`], [`ContextId`], [`MessageId`] (v7 UUID newtypes).
//!
//! ## Forward compatibility
//!
//! Minor A2A revisions should add **optional** fields with `#[serde(default)]` on new structs, or
//! `Option<T>` / `serde_json::Value` where the spec leaves room, so older clients keep deserializing.

pub mod agent_call;
pub mod extensions;
pub mod headers;
mod ids;
mod jsonrpc;
mod methods;
pub mod topics;
mod types;

pub use agent_call::{AgentCallInput, AgentCallInputError};
pub use ids::*;
pub use jsonrpc::*;
pub use methods::*;
pub use types::*;
