//! Library surface of the `ork-api` binary, exposed so integration tests in
//! `crates/ork-api/tests/` can exercise wiring without spinning up a full server.
//!
//! `main.rs` continues to drive process lifecycle; everything else lives behind these
//! `pub mod` re-exports.

pub mod eventing;
pub mod middleware;
pub mod remote_agents;
pub mod routes;
pub mod sse_buffer;
pub mod state;
