use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrkError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("LLM provider error: {0}")]
    LlmProvider(String),

    #[error("integration error: {0}")]
    Integration(String),

    #[error("workflow error: {0}")]
    Workflow(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Remote A2A agent returned a hard error. Carries the application code (HTTP status
    /// for transport-level failures, or the JSON-RPC error code from the response body)
    /// and the operator-facing message. ADR 0007 §`Failure model`.
    #[error("A2A client error ({0}): {1}")]
    A2aClient(i32, String),

    /// SSE stream from a remote A2A agent disconnected mid-task. The caller has already
    /// received any events emitted before the disconnect; engine MAY recover via
    /// `tasks/get`. ADR 0007 §`Failure model`.
    #[error("A2A SSE stream lost: {0}")]
    A2aStreamLost(String),
}

impl OrkError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Unauthorized(_) => 401,
            Self::Forbidden(_) => 403,
            Self::Validation(_) => 422,
            Self::Conflict(_) => 409,
            Self::LlmProvider(_)
            | Self::Integration(_)
            | Self::Workflow(_)
            | Self::A2aClient(..)
            | Self::A2aStreamLost(_) => 502,
            Self::Database(_) | Self::Internal(_) => 500,
            Self::Unsupported(_) => 501,
        }
    }
}
