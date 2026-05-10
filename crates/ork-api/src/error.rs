//! ADR-0056 §`Error model`: single error envelope shared across the
//! auto-generated REST + SSE routes. Maps [`OrkError`] (and validation
//! failures) to a JSON body of the shape:
//!
//! ```json
//! { "error": { "kind": "validation"|"auth"|"not_found"|"internal"|...,
//!              "message": "...",
//!              "details": {},
//!              "trace_id": "..." } }
//! ```
//!
//! `trace_id` is best-effort: the current `tracing::Span` id is used
//! when present so the same id surfaces in logs and traces; ADR-0058
//! will swap this for the OTel context.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ork_common::error::OrkError;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

/// Outer envelope: always `{ "error": { ... } }`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorBody {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Validation,
    Auth,
    Forbidden,
    NotFound,
    Conflict,
    Unsupported,
    RateLimited,
    Internal,
    Upstream,
}

impl ErrorKind {
    fn http_status(self) -> StatusCode {
        match self {
            Self::Validation => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Auth => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Unsupported => StatusCode::NOT_IMPLEMENTED,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        }
    }
}

/// Single error type returned by every auto-generated handler.
/// Implements [`IntoResponse`] so handlers can `?` directly.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
    pub details: Option<Value>,
    pub status_override: Option<StatusCode>,
}

impl ApiError {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
            status_override: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status_override = Some(status);
        self
    }

    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, message)
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    #[must_use]
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Auth, message)
    }

    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Forbidden, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    #[must_use]
    pub fn missing_tenant_header() -> Self {
        Self::new(
            ErrorKind::Validation,
            "missing X-Ork-Tenant header (ADR-0020)",
        )
        .with_status(StatusCode::BAD_REQUEST)
    }
}

impl From<OrkError> for ApiError {
    fn from(e: OrkError) -> Self {
        match e {
            OrkError::NotFound(m) => ApiError::not_found(m),
            OrkError::Unauthorized(m) => ApiError::auth(m),
            OrkError::Forbidden(m) => ApiError::forbidden(m),
            OrkError::Validation(m) => ApiError::validation(m),
            OrkError::Conflict(m) => ApiError::new(ErrorKind::Conflict, m),
            OrkError::Unsupported(m) => ApiError::new(ErrorKind::Unsupported, m),
            OrkError::Configuration { message } => ApiError::new(ErrorKind::Internal, message),
            OrkError::Internal(m) => ApiError::internal(m),
            OrkError::LlmProvider(m) | OrkError::Integration(m) | OrkError::A2aStreamLost(m) => {
                ApiError::new(ErrorKind::Upstream, m)
            }
            OrkError::A2aClient(_, m) => ApiError::new(ErrorKind::Upstream, m),
            OrkError::Workflow(m) | OrkError::Database(m) => ApiError::internal(m),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self
            .status_override
            .unwrap_or_else(|| self.kind.http_status());
        let trace_id = current_trace_id();
        let body = ErrorEnvelope {
            error: ErrorBody {
                kind: self.kind,
                message: self.message,
                details: self.details,
                trace_id,
            },
        };
        (status, Json(body)).into_response()
    }
}

/// Span-derived trace id placeholder. ADR-0058 wires the canonical OTel
/// trace id; until then the current `tracing::Span` id (or `None` if no
/// span is active) is used so logs and the response share a key.
fn current_trace_id() -> Option<String> {
    let id = tracing::Span::current().id()?;
    Some(format!("{:016x}", id.into_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn validation_error_serialises_with_envelope() {
        let resp = ApiError::validation("bad payload").into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["kind"], "validation");
        assert_eq!(v["error"]["message"], "bad payload");
    }

    #[tokio::test]
    async fn missing_tenant_uses_400_not_422() {
        let resp = ApiError::missing_tenant_header().into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
