//! Code-first native tool DSL (ADR [`0051`](../../docs/adrs/0051-code-first-tool-dsl.md)).
//!
//! ```
//! use ork_tool::tool;
//! use schemars::JsonSchema;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize, JsonSchema)]
//! struct ReverseIn {
//!     input: String,
//! }
//!
//! #[derive(Serialize, JsonSchema)]
//! struct ReverseOut {
//!     output: String,
//! }
//!
//! let _t = tool("reverse")
//!     .description("Reverse the input string")
//!     .input::<ReverseIn>()
//!     .output::<ReverseOut>()
//!     .execute(|_ctx, ReverseIn { input }| async move {
//!         Ok(ReverseOut {
//!             output: input.chars().rev().collect(),
//!         })
//!     });
//! ```
//!
//! ## Typestate: `.execute` requires `.input` and `.output`
//!
//! ```compile_fail
//! use ork_tool::tool;
//! fn _demo() {
//!     let _ = tool("x")
//!         .description("d")
//!         .execute(|_, ()| async { Ok(()) });
//! }
//! ```

#![doc = include_str!("../README.md")]
// Closure-heavy builders; keeping types inline matches rig/orchestration patterns.
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

mod builder;
mod context;
mod into_tool_def;
mod retry;
mod tool;

pub use builder::{ToolBuilder, Underspec, tool};
pub use context::ToolContext;
pub use into_tool_def::IntoToolDef;
pub use retry::{ExponentialBackoff, RetryPolicy};
pub use tool::{DynToolInvoke, Tool};
