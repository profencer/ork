//! Built-in protocol gateways (ADR-0013).

pub mod auth;
pub mod bootstrap;
pub mod event_mesh;
pub mod hmac_util;
pub mod mcp_gw;
pub mod noop_gateway;
pub mod registry;
pub mod rest;
pub mod webhook;

pub use registry::{GatewayInstance, GatewayRegistry, GatewaysBuild};
