//! Build a callable [`Agent`] instance from a discovered or configured
//! [`AgentCard`]. Implemented by `ork-integrations::a2a_client::A2aRemoteAgentBuilder`
//! (ADR-0007). Lives in `ork-core` so the workflow engine and the discovery
//! subscriber (`ork-eventing`) can construct remote agents without depending
//! on the integrations crate.

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::config::A2aAuthToml;
use ork_common::error::OrkError;
use url::Url;

use crate::a2a::AgentCard;
use crate::ports::agent::Agent;

/// Factory that turns an [`AgentCard`] into a callable [`Agent`]. The default
/// implementation is `A2aRemoteAgentBuilder` (HTTP/SSE A2A 1.0); tests can
/// substitute an in-memory stub.
#[async_trait]
pub trait RemoteAgentBuilder: Send + Sync {
    async fn build(&self, card: AgentCard) -> Result<Arc<dyn Agent>, OrkError>;

    /// Build a transient agent straight from a card URL + optional auth — the
    /// inline-card path used by the workflow engine (ADR-0007 §`Workflow-time
    /// inline card`). Implementations are expected to fetch the card (cached),
    /// resolve secrets from env, and call [`Self::build`].
    ///
    /// Default implementation returns [`OrkError::Unsupported`] so test stubs that
    /// only care about the registered-id path don't have to provide it.
    async fn build_inline(
        &self,
        _card_url: Url,
        _auth: Option<A2aAuthToml>,
    ) -> Result<Arc<dyn Agent>, OrkError> {
        Err(OrkError::Unsupported(
            "this RemoteAgentBuilder does not support inline cards".into(),
        ))
    }
}
