//! `ork migrate` — apply or list bundled Postgres / libsql migrations.
//!
//! ADR-0057 ships this verb as a clap-visible **stub**. The migration
//! runner needs an opinionated story for (a) which crate owns which
//! migration files, (b) cross-database (Postgres vs libsql) support, and
//! (c) a CI matrix (testcontainers-backed Postgres) to cover the apply
//! path. Each is a follow-up ADR; until then this command exits 2.

use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct MigrateArgs {
    /// Database URL (postgres://… or libsql://…).
    #[arg(long)]
    pub db: Option<String>,

    /// Print the planned migrations without applying.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(_args: MigrateArgs) -> Result<()> {
    eprintln!(
        "ork migrate: planned for a follow-up ADR. The runner needs cross-database support \
         (Postgres + libsql), a clear story for migration ownership across ork crates, and a \
         testcontainers-backed CI matrix. v1 of ADR-0057 lands the verb shape only. Exiting 2."
    );
    std::process::exit(2);
}
