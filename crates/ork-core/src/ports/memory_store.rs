//! Working / semantic memory port (ADR
//! [`0053`](../../../docs/adrs/0053-memory-working-and-semantic.md)).
//!
//! [`MemoryStore`] is the durable user-memory surface every `CodeAgent`
//! gets when [`OrkAppBuilder::memory`](../../../../ork-app/src/builder.rs)
//! is wired. It exposes three independent slices:
//!
//! - **Chat history** — append a [`ChatMessage`] for a `(tenant, resource,
//!   thread)` and read back the last N messages.
//! - **Working memory** — durable, structured per-resource state
//!   (`name`, `preferences`, `goals`). Optionally schema-validated via
//!   [`WorkingMemoryShape`](crate::ports::memory_store::WorkingMemoryShape)
//!   on the [backends].
//! - **Semantic recall** — vector-indexed past message bodies retrieved
//!   by similarity. The embedder lives behind [`Embedder`] and is
//!   passed to the backend at construction; ADR 0053 deferred routing
//!   embedders through `LlmRouter` to a follow-up.
//!
//! [`MemoryContext`] is the ambient identity of a memory call (tenant,
//! resource, thread, agent). [`AgentContext::memory_context`] in
//! `ork-core/src/a2a/context.rs` derives one from the running request.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::{MessageId, ResourceId, ThreadId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use serde::{Deserialize, Serialize};

use crate::a2a::AgentId;
use crate::ports::llm::ChatMessage;

/// Ambient identity passed to every [`MemoryStore`] method.
///
/// `agent_id` is part of the working-memory key so each agent gets its
/// own slot under the same `(tenant, resource)`. Mastra's
/// "shared across all agents owned by `(tenant, resource)`" mode is
/// deferred to a follow-up — see ADR 0053 §`Open questions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryContext {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
}

/// One semantic-recall result row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecallHit {
    pub message_id: MessageId,
    pub thread_id: ThreadId,
    pub content: String,
    /// Cosine similarity in `[-1.0, 1.0]`; backends MAY normalize to
    /// `[0.0, 1.0]` for ergonomics. Higher is more similar.
    pub score: f32,
}

/// One row returned by [`MemoryStore::list_threads`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: ThreadId,
    pub last_message_at: DateTime<Utc>,
    pub message_count: u64,
}

/// Recall scope. ADR 0053 §`Semantic-recall scope`. Defaults to
/// [`Scope::Resource`] in `MemoryOptions`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Hits limited to the current `(tenant, resource, thread)`.
    Thread,
    /// Hits span all threads owned by the current `(tenant, resource)`.
    /// Mastra's default.
    #[default]
    Resource,
    /// Hits span all resources within the current tenant. ADR 0053 ships
    /// this for the multi-team-shared-memory case ADR 0043 was after;
    /// documented as the leakiest shape.
    Tenant,
}

/// Working memory shape. ADR 0053 §`Working memory shapes`.
#[derive(Clone, Debug, Default)]
pub enum WorkingMemoryShape {
    /// Free-form JSON. The agent reads/writes arbitrary keys.
    #[default]
    Free,
    /// Schema-constrained. Writes are validated against this schema;
    /// rows that fail later validation are returned as
    /// [`OrkError::Validation`] (see ADR 0053 §`Open questions`).
    Schema(serde_json::Value),
    /// Pre-baked common shape: `name`, `preferences`, `goals`.
    User,
}

/// Embedder port consumed by [`MemoryStore`] backends. v1 ships an
/// `OpenAiEmbedder` in `ork-llm`; routing through `LlmRouter` is
/// deferred per ADR 0053 §`Embedder selection`.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Vector dimension produced by [`Embedder::embed`]. MUST match the
    /// schema dimension of the underlying vector index.
    fn dimension(&self) -> usize;
    /// Compute one embedding per input string. The order of the returned
    /// `Vec<Vec<f32>>` MUST match `texts`.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, OrkError>;
}

/// Per-call semantic-recall configuration. Defaults match Mastra:
/// recall enabled, `top_k = 6`, scope [`Scope::Resource`].
#[derive(Clone, Debug)]
pub struct SemanticRecallConfig {
    pub enabled: bool,
    pub top_k: usize,
    pub scope: Scope,
}

impl Default for SemanticRecallConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_k: 6,
            scope: Scope::Resource,
        }
    }
}

/// Top-level memory configuration consumed by `CodeAgent` (ADR 0052)
/// and the `ork-memory` backends. Mirrors Mastra's `memoryOptions`.
#[derive(Clone, Debug)]
pub struct MemoryOptions {
    /// Number of last messages from the current thread to inject into
    /// the prompt. `0` disables history injection.
    pub last_messages: usize,
    /// When `true`, the agent's working-memory snapshot is injected
    /// into the prompt and the synthetic `memory.update_working` tool
    /// is registered.
    pub include_working: bool,
    pub semantic_recall: SemanticRecallConfig,
    pub working_memory: Option<WorkingMemoryShape>,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            last_messages: 20,
            include_working: true,
            semantic_recall: SemanticRecallConfig::default(),
            working_memory: Some(WorkingMemoryShape::User),
        }
    }
}

/// Working / semantic memory backend.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Backend identifier used in the [`OrkApp`](../../../../ork-app/src/app.rs)
    /// manifest and in telemetry. Default is the impl's type name slug
    /// (e.g. `"libsql"`, `"postgres"`); customers MAY override.
    fn name(&self) -> &str;

    /// Persist a chat message into the `(tenant, resource, thread)` slot.
    /// If the backend was configured with semantic recall enabled, also
    /// embeds and indexes the message content. Returns the assigned
    /// [`MessageId`].
    async fn append_message(
        &self,
        ctx: &MemoryContext,
        msg: ChatMessage,
    ) -> Result<MessageId, OrkError>;

    /// Most recent N messages for this thread, returned oldest-first so
    /// the LLM sees a natural transcript order.
    async fn last_messages(
        &self,
        ctx: &MemoryContext,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, OrkError>;

    /// Read working memory for `(tenant, resource, agent)`. `Ok(None)`
    /// when no row exists.
    async fn working_memory(
        &self,
        ctx: &MemoryContext,
    ) -> Result<Option<serde_json::Value>, OrkError>;

    /// Upsert working memory for `(tenant, resource, agent)`.
    /// Implementations validate against the configured
    /// [`WorkingMemoryShape`]; out-of-shape writes return
    /// [`OrkError::Validation`].
    async fn set_working_memory(
        &self,
        ctx: &MemoryContext,
        v: serde_json::Value,
    ) -> Result<(), OrkError>;

    /// Top-K semantic-recall hits for `query`, scoped by the backend's
    /// configured [`Scope`]. Returned in descending similarity order.
    async fn semantic_recall(
        &self,
        ctx: &MemoryContext,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RecallHit>, OrkError>;

    /// All threads owned by `resource_id` within `tenant`, newest first.
    async fn list_threads(
        &self,
        tenant_id: TenantId,
        resource_id: &ResourceId,
    ) -> Result<Vec<ThreadSummary>, OrkError>;

    /// Hard-delete all rows for the current `(tenant, resource, thread)`
    /// (messages + embeddings). Working memory survives because it is
    /// resource-scoped, not thread-scoped. ADR 0053 ships this as the
    /// GDPR-deletion primitive.
    async fn delete_thread(&self, ctx: &MemoryContext) -> Result<(), OrkError>;
}
