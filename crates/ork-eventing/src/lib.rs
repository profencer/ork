//! Async event-mesh primitives for ork's hybrid Kong+Kafka transport (ADR
//! [`0004`](../../../docs/adrs/0004-hybrid-kong-kafka-transport.md)).
//!
//! This crate owns the Kafka I/O for the async plane. The wire-format topic names live in
//! [`ork_a2a::topics`] and the JSON-RPC envelope/header constants live in [`ork_a2a::headers`];
//! `ork-eventing` is the pump that pushes those bytes onto Kafka and pulls them off again.
//!
//! ## Layout
//!
//! - [`Producer`] / [`Consumer`] — backend-agnostic async traits.
//! - [`InMemoryBackend`] — `tokio::sync::broadcast`-backed in-process implementation used by
//!   tests and dev mode (when no brokers are configured).
//! - [`RsKafkaBackend`] — pure-Rust `rskafka` implementation for production (no `librdkafka`).
//! - [`build_client`] — factory that picks the right backend from a [`KafkaConfig`].
//!
//! ## Concrete consumers (out of scope for this crate)
//!
//! `DiscoveryPublisher`/`Subscriber` (ADR 0005), the SSE bridge (ADR 0008), the push outbox
//! worker (ADR 0009), and the fire-and-forget delegation handler (ADR 0006) build on these
//! primitives but live in their own modules.

mod client;
mod consumer;
pub mod delegation_publisher;
pub mod discovery;
mod error;
pub mod gateway_discovery;
mod in_memory;
mod producer;
mod rskafka_backend;

pub use client::{EventingClient, build_client};
pub use consumer::{ConsumedMessage, Consumer, MessageStream};
pub use delegation_publisher::KafkaDelegationPublisher;
pub use discovery::{CardProvider, DiscoveryPublisher, DiscoverySubscriber};
pub use error::EventingError;
pub use gateway_discovery::{GatewayCardProvider, GatewayDiscoveryPublisher};
pub use in_memory::InMemoryBackend;
pub use producer::Producer;
pub use rskafka_backend::RsKafkaBackend;
