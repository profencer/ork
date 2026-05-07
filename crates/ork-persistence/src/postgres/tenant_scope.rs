//! ADR-0020 §`Mesh trust — JWT claims and propagation`: tenant-scoped Postgres
//! transactions.
//!
//! Postgres RLS policies on tenant-scoped tables (configured in
//! [`migrations/001_initial.sql`](../../../../migrations/001_initial.sql),
//! `003_delegation.sql`, `004_a2a_endpoints.sql`, `005_push_notifications.sql`,
//! `007_artifacts.sql`) all read `current_setting('app.current_tenant_id')::UUID`.
//! Until ADR-0020 nothing set that GUC, so the policies were effectively off
//! (the connection role bypassed RLS as superuser). This helper opens a
//! [`Transaction`] and binds `app.current_tenant_id` for its lifetime — every
//! statement run inside the transaction is then subject to the policy check.
//!
//! Callers drive the commit explicitly so multi-statement repo flows can
//! choose between the existing per-method-tx pattern and grouped operations:
//!
//! ```ignore
//! let mut tx = open_tenant_tx(&self.pool, tenant_id).await?;
//! sqlx::query("UPDATE workflow_runs SET status = $1 WHERE id = $2")
//!     .bind("completed")
//!     .bind(run_id.0)
//!     .execute(&mut *tx)
//!     .await?;
//! tx.commit().await?;
//! ```
//!
//! Dropping the tx without committing rolls it back (sqlx default), which is
//! the safer failure mode — partial RLS-scoped writes never linger.

use ork_common::error::OrkError;
use ork_common::types::TenantId;
use sqlx::{PgPool, Postgres, Transaction};

/// Open a Postgres transaction with `app.current_tenant_id` set to `tenant_id`.
/// Every statement executed inside the returned tx is subject to the
/// configured RLS policies; callers MUST `commit()` for writes to persist.
///
/// `set_config(name, value, true)` is the function form of `SET LOCAL`: it
/// scopes the GUC to the current transaction and accepts parameter binding
/// (raw `SET LOCAL` does not).
pub async fn open_tenant_tx(
    pool: &PgPool,
    tenant_id: TenantId,
) -> Result<Transaction<'_, Postgres>, OrkError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| OrkError::Database(format!("open tenant tx: {e}")))?;

    sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
        .bind(tenant_id.0.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("set tenant scope GUC: {e}")))?;

    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Smoke test against a real Postgres `DATABASE_URL`. Skipped in CI runs
    /// that don't provide one (the `#[ignore]` is removed by the integration
    /// test wrapper at `crates/ork-persistence/tests/rls_smoke.rs`, which
    /// drives the same path with a known-tenant assertion).
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing at a Postgres with the migrations applied"]
    async fn open_tenant_tx_sets_guc() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        let tid = TenantId(Uuid::nil());
        let mut tx = open_tenant_tx(&pool, tid).await.expect("open");

        let row: (String,) =
            sqlx::query_as("SELECT current_setting('app.current_tenant_id', true)")
                .fetch_one(&mut *tx)
                .await
                .expect("read GUC");
        assert_eq!(row.0, tid.0.to_string());

        // Drop without commit → rollback (default sqlx behaviour). Confirms
        // failure mode is safe.
        drop(tx);
    }
}
