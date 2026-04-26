//! Dynamic `«type:expr | format»` embeds (ADR-0015).

pub mod early;
pub mod handlers;
pub mod parser;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::{ContextId, Part, TaskId};

use crate::ports::artifact_store::ArtifactStore;
use ork_common::types::TenantId;
use thiserror::Error;

use crate::ports::a2a_task_repo::A2aTaskRepository;

/// When the handler runs relative to the LLM call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbedPhase {
    Early,
    Late,
    Both,
}

/// Result of resolving a single embed.
#[derive(Debug)]
pub enum EmbedOutput {
    Text(String),
    /// Late-phase: replace with structured parts (e.g. text + file).
    Parts(Vec<Part>),
}

/// Embed resolution failure (mapped to [`ork_common::error::OrkError`] at the workflow/API edge).
#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("unknown embed type: {0}")]
    Unknown(String),
    #[error("invalid expression: {0}")]
    InvalidExpression(String),
    #[error("invalid format hint: {0}")]
    InvalidFormat(String),
    #[error("limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("handler error: {0}")]
    Handler(#[from] anyhow::Error),
}

/// Per-resolve limits (ADR-0015 defaults).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbedLimits {
    /// Nesting bound for `«a:«b:…»»`.
    pub max_embed_depth: usize,
    /// Hard cap on successful handler invocations per `resolve_early` / late pass.
    pub max_embeds_per_request: usize,
    /// Cumulative cap on bytes expanded from late-phase embeds (best-effort).
    pub max_late_embed_output_bytes: usize,
    /// If buffered text (unfinished `«`) grows past this, flush as plain text.
    pub max_late_embed_buffer_bytes: usize,
}

impl Default for EmbedLimits {
    fn default() -> Self {
        Self {
            max_embed_depth: 4,
            max_embeds_per_request: 64,
            max_late_embed_output_bytes: 1_048_576,
            max_late_embed_buffer_bytes: 65_536,
        }
    }
}

/// Context available to all embed handlers.
#[derive(Clone)]
pub struct EmbedContext {
    pub tenant_id: TenantId,
    /// ADR-0016: optional conversation scope; `None` = tenant-scoped only.
    pub context_id: Option<ContextId>,
    pub task_id: Option<TaskId>,
    /// When missing, `status_update` and similar handlers fail.
    pub a2a_repo: Option<Arc<dyn A2aTaskRepository>>,
    /// ADR-0016: optional blob store for `artifact_content` / `artifact_meta` embeds.
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    /// When `presign_get` is unavailable, build `GET {base}/api/artifacts/…` URLs (no trailing path).
    pub artifact_public_base: Option<String>,
    /// Same value as [`EmbedLimits::max_late_embed_output_bytes`] for this resolve pass.
    pub max_late_embed_output_bytes: usize,
    pub now: DateTime<Utc>,
    /// Early `«var:…»` map (callers may extend later).
    pub variables: HashMap<String, String>,
    /// Recursion / nesting control.
    pub depth: usize,
}

impl EmbedContext {
    /// Seed a context with [`EmbedLimits`]-driven output caps. Artifact fields start unset.
    #[must_use]
    pub fn with_limits(
        tenant_id: TenantId,
        context_id: Option<ContextId>,
        task_id: Option<TaskId>,
        a2a_repo: Option<Arc<dyn A2aTaskRepository>>,
        now: DateTime<Utc>,
        variables: HashMap<String, String>,
        limits: &EmbedLimits,
    ) -> Self {
        Self {
            tenant_id,
            context_id,
            task_id,
            a2a_repo,
            artifact_store: None,
            artifact_public_base: None,
            max_late_embed_output_bytes: limits.max_late_embed_output_bytes,
            now,
            variables,
            depth: 0,
        }
    }

    #[must_use]
    pub fn with_depth(&self, depth: usize) -> Self {
        Self {
            depth,
            ..self.clone()
        }
    }
}

/// Registry of type id → handler (built-in + future plugin hooks).
#[derive(Clone, Default)]
pub struct EmbedRegistry {
    handlers: HashMap<String, Arc<dyn EmbedHandler>>,
}

impl EmbedRegistry {
    /// Built-in handler set: `math`, `datetime`, `uuid`, `var`, `status_update`,
    /// `artifact_content`, `artifact_meta` (ADR-0016).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut r = Self::default();
        handlers::register_builtins(&mut r);
        r
    }

    pub(crate) fn register<H: EmbedHandler + 'static>(&mut self, h: H) {
        self.handlers.insert(h.type_id().to_string(), Arc::new(h));
    }

    /// Look up a handler by `type` in `«type:…»`.
    #[must_use]
    pub fn get(&self, type_id: &str) -> Option<Arc<dyn EmbedHandler>> {
        self.handlers.get(type_id).cloned()
    }
}

#[async_trait]
pub trait EmbedHandler: Send + Sync {
    fn type_id(&self) -> &'static str;
    fn phase(&self) -> EmbedPhase;

    /// `expr` is the (possibly pre-resolved) body after `type:`; `format` is the part after ` | `, if any.
    async fn resolve(
        &self,
        expr: &str,
        format: Option<&str>,
        ctx: &EmbedContext,
    ) -> Result<EmbedOutput, EmbedError>;
}

pub use early::{resolve_early, resolve_early_counted};

/// When late-phase expansion exceeds size limits and artifact spill is unavailable, emit this marker.
pub const LATE_EMBED_OUTPUT_TRUNCATED: &str = "[ork:ref:late_output_truncated]";

/// Maps `input.embed_variables` (JSON object of strings) to `«var:…»` keys.
pub fn embed_variables_from_workflow_input(input: &serde_json::Value) -> HashMap<String, String> {
    let Some(obj) = input.get("embed_variables").and_then(|v| v.as_object()) else {
        return HashMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}
