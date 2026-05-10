//! ADR-0056 §`Decision`: the auto-generated REST + SSE surface mounted
//! alongside the A2A endpoints.
//!
//! Sub-modules each own one resource family:
//!
//! - [`manifest`] — `GET /api/manifest`, `GET /api/openapi.json`,
//!   `GET /healthz` & `/readyz`.
//! - [`agents`] — `GET /api/agents`, `GET /api/agents/:id`,
//!   `POST /api/agents/:id/generate|stream`.
//! - [`workflows`] — `GET/POST /api/workflows[/:id[/run|runs[/:run_id[/...]]]]`.
//! - [`tools`] — `GET /api/tools`, `GET /api/tools/:id`,
//!   `POST /api/tools/:id/invoke`.
//! - [`memory`] — `/api/memory/threads/...` & `/api/memory/working`.
//! - [`scorers`] — `GET /api/scorers`, `GET /api/scorer-results`.
//! - [`swagger`] — `/swagger-ui` static UI, optional via
//!   [`ServerConfig::swagger_ui`](ork_app::types::ServerConfig::swagger_ui).
//! - [`tenant`] — middleware enforcing `X-Ork-Tenant` (ADR-0020).
//!
//! Composed by [`crate::router_for::router_for`] into one
//! [`axum::Router`] that also merges the existing A2A endpoints.

pub mod agents;
pub mod manifest;
pub mod memory;
pub mod scorers;
pub mod swagger;
pub mod tenant;
pub mod tools;
pub mod workflows;
