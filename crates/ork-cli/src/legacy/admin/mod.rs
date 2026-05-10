//! `ork legacy admin` — administrative operations (DB-bound).
//! ADR-0057 §`Legacy subcommands`.

use anyhow::Result;
use clap::Subcommand;

pub mod push;

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Push notification administration (ADR-0009).
    Push {
        #[command(subcommand)]
        cmd: push::PushAdminCommand,
    },
}

pub async fn run(cmd: AdminCommand, verbose: bool) -> Result<()> {
    match cmd {
        AdminCommand::Push { cmd } => push::run(cmd, verbose).await,
    }
}
