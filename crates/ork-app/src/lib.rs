//! `OrkApp` central registry (ADR [`0049`](../../docs/adrs/0049-orkapp-central-registry.md)).

pub mod app;
pub mod builder;
pub mod id;
pub mod inner;
pub mod manifest;
pub mod ports;
pub mod types;

pub use app::{ChatMessage, OrkApp, WorkflowRunHandle};
pub use builder::OrkAppBuilder;
pub use inner::OrkAppInner;
pub use manifest::AppManifest;
pub use ports::{ServeHandle, Server};
pub use types::*;

impl crate::app::OrkApp {
    /// Builder entrypoint (Mastra `new Mastra({ … })` parity).
    #[must_use]
    pub fn builder() -> crate::builder::OrkAppBuilder {
        crate::builder::OrkAppBuilder::default()
    }
}
