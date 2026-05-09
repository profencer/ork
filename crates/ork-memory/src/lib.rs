//! Working / semantic memory backends and builder façade for ADR
//! [`0053`](../../docs/adrs/0053-memory-working-and-semantic.md).
//!
//! `ork-memory` ships two implementations of the
//! [`MemoryStore`](ork_core::ports::memory_store::MemoryStore) port:
//!
//! - [`Memory::libsql`] (default feature `libsql`) — zero-config dev
//!   store backed by libsql + its native vector type.
//! - [`Memory::postgres`] (feature `postgres`) — production store
//!   backed by an existing `sqlx::PgPool` and the `pgvector`
//!   extension; expects the `mem_messages` / `mem_working` /
//!   `mem_embeddings` tables created by
//!   [`migrations/013_memory_tables.sql`](../../migrations/013_memory_tables.sql).
//!
//! Both backends share [`MemoryOptions`], the validation helpers in
//! [`shapes`], the [`DeterministicMockEmbedder`] used in tests, and
//! the same trait surface — so the [`OrkApp`](../../crates/ork-app/)
//! consumes them through `Arc<dyn MemoryStore>` without distinguishing
//! the backend.
//!
//! ## Hard rule (ADR-0053 §Acceptance criteria)
//!
//! No file under `crates/ork-memory/` imports `axum`, `reqwest`,
//! `rmcp`, or `rskafka`. The crate stays inside the hexagon: domain
//! types from `ork-core` and `ork-a2a`, errors from `ork-common`, and
//! one storage dependency per backend feature. The CI grep guard
//! at `crates/ork-memory/tests/no_infra_imports.rs` enforces this.

pub mod embedder;
pub mod shapes;
pub mod types;

pub use embedder::DeterministicMockEmbedder;
pub use ork_core::ports::memory_store::{
    Embedder, MemoryContext, MemoryOptions, MemoryStore, RecallHit, Scope, SemanticRecallConfig,
    ThreadSummary, WorkingMemoryShape,
};
pub use types::EmbedderSpec;

#[cfg(feature = "libsql")]
pub mod libsql_backend;
#[cfg(feature = "postgres")]
pub mod postgres_backend;

use std::sync::Arc;

use ork_common::error::OrkError;

/// Builder façade. Construct a memory store with one of:
///
/// ```ignore
/// use ork_memory::{Memory, MemoryOptions};
///
/// let memory = Memory::libsql("file:./ork.db")
///     .options(MemoryOptions::default())
///     .open()
///     .await?;
/// ```
pub struct Memory;

impl Memory {
    /// Open a libsql / sqlite-shaped store at `url`. URLs follow
    /// libsql's convention: `"file:./ork.db"`, `":memory:"`, or
    /// `"libsql://example.turso.io"`.
    #[cfg(feature = "libsql")]
    #[must_use]
    pub fn libsql(url: impl Into<String>) -> MemoryBuilder<libsql_backend::LibsqlConnect> {
        MemoryBuilder {
            connect: libsql_backend::LibsqlConnect { url: url.into() },
            options: MemoryOptions::default(),
            embedder: None,
        }
    }

    /// Open a Postgres-backed store on top of an existing pool. The
    /// pool MUST have `pgvector` enabled and the migration applied.
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn postgres(pool: sqlx::PgPool) -> MemoryBuilder<postgres_backend::PostgresConnect> {
        MemoryBuilder {
            connect: postgres_backend::PostgresConnect { pool },
            options: MemoryOptions::default(),
            embedder: None,
        }
    }
}

/// Generic builder shared by both backends. Implementations of
/// [`OpenMemory`] turn the (connect, options, embedder) triple into an
/// `Arc<dyn MemoryStore>`.
pub struct MemoryBuilder<C: OpenMemory> {
    connect: C,
    options: MemoryOptions,
    embedder: Option<Arc<dyn Embedder>>,
}

impl<C: OpenMemory> MemoryBuilder<C> {
    #[must_use]
    pub fn options(mut self, opts: MemoryOptions) -> Self {
        self.options = opts;
        self
    }

    /// Provide an [`Embedder`]. When semantic recall is enabled and no
    /// embedder is set, [`MemoryBuilder::open`] returns
    /// [`OrkError::Configuration`].
    #[must_use]
    pub fn embedder(mut self, e: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(e);
        self
    }

    /// Open the store. Validates that an embedder is present iff
    /// semantic recall is enabled, runs any backend-specific
    /// initialisation, and returns the trait-object handle the
    /// [`OrkApp`](../../crates/ork-app/) registers.
    pub async fn open(self) -> Result<Arc<dyn MemoryStore>, OrkError> {
        if self.options.semantic_recall.enabled && self.embedder.is_none() {
            return Err(OrkError::Configuration {
                message: "semantic recall enabled but no embedder set; call \
                          MemoryBuilder::embedder(...) or disable recall in MemoryOptions"
                    .into(),
            });
        }
        self.connect.open(self.options, self.embedder).await
    }
}

/// Backend-specific connect logic. Hidden trait — the public surface is
/// [`Memory::libsql`] / [`Memory::postgres`].
#[doc(hidden)]
#[async_trait::async_trait]
pub trait OpenMemory: Send + Sync {
    async fn open(
        self,
        options: MemoryOptions,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Arc<dyn MemoryStore>, OrkError>;
}
