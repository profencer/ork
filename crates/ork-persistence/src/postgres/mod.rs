//! Postgres adapters for the `ork-core` repository ports.
//!
//! ADR-0020 §`Mesh trust — JWT claims and propagation`: tenant-scoped queries
//! must run inside a transaction with `app.current_tenant_id` bound, via
//! [`tenant_scope::open_tenant_tx`]. This is the canonical pattern for new
//! and migrated read/write paths. As of the initial Phase-A3 commit only
//! [`workflow_repo`] is fully on the helper; the other tenant-scoped repos
//! still rely on explicit `WHERE tenant_id = $n` filters and migrate
//! incrementally in follow-up commits (a2a_task_repo, a2a_push_repo,
//! a2a_push_dead_letter_repo, artifact_meta_repo, webui_store,
//! workflow_snapshot_repo). [`tenant_repo`] and [`a2a_signing_key_repo`] are
//! special: tenants are admin-managed across tenants and signing keys are
//! KEK-protected (ADR-0009) — neither is row-level-secured today.

pub mod a2a_push_dead_letter_repo;
pub mod a2a_push_repo;
pub mod a2a_signing_key_repo;
pub mod a2a_task_repo;
pub mod artifact_meta_repo;
pub mod tenant_repo;
pub mod tenant_scope;
pub mod webui_store;
pub mod workflow_repo;
pub mod workflow_snapshot_repo;

pub use tenant_scope::open_tenant_tx;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub async fn create_pool(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}
