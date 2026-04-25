use thiserror::Error;

/// Errors returned by [`crate::Producer`] and [`crate::Consumer`] implementations.
///
/// The [`Backend`](EventingError::Backend) variant carries arbitrary backend strings (e.g. an
/// `rskafka` error) so callers do not need to depend on the underlying client crate to surface
/// failures.
#[derive(Debug, Error)]
pub enum EventingError {
    #[error("eventing backend error: {0}")]
    Backend(String),

    #[error("eventing client is not connected to any backend")]
    NotConnected,

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("serde_json: {0}")]
    SerdeJson(#[from] serde_json::Error),
}
