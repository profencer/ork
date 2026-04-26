//! Web UI gateway (ADR-0017): protected `/webui/api/*` routes and `Gateway` discovery card.
//!
//! Registration: `GatewayRegistry::add_factory("webui", Arc::new(WebUiGatewayFactory))` from `ork-api`.

mod factory;
pub mod in_memory_store;
pub mod routes;
mod state;
mod static_assets;

pub use factory::WebUiGatewayFactory;
pub use state::WebUiState;
