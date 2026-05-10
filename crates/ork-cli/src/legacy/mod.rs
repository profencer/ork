//! Legacy demo-shaped subcommands rehomed under `ork legacy <subcommand>`
//! so existing scripts keep working until the demos are reauthored
//! against the ADR-0048 platform shape (ADR-0057 §`Legacy subcommands`).

use anyhow::Result;
use clap::Subcommand;

pub mod admin;
pub mod change_plan;
pub mod standup;
pub mod webui;
pub mod workflow;

/// Existing demo verbs; will be removed when demos are reauthored
/// against the new platform shape (ADR-0048).
#[derive(Subcommand)]
pub enum LegacyCmd {
    /// Generate a standup brief from your recent commits, PRs, and issues.
    Standup(standup::StandupArgs),
    /// Run the change-plan workflow locally (clones repos, code search, multi-agent plan).
    ChangePlan(change_plan::ChangePlanArgs),
    /// Administrative operations (DB-bound).
    Admin {
        #[command(subcommand)]
        cmd: admin::AdminCommand,
    },
    /// Workflow file utilities.
    Workflow {
        #[command(subcommand)]
        cmd: workflow::WorkflowCmd,
    },
    /// Web UI (ADR-0017): Vite dev server for `client/webui/frontend`.
    Webui {
        #[command(subcommand)]
        cmd: webui::WebuiCommand,
    },
}

pub async fn run(cmd: LegacyCmd, verbose: bool) -> Result<()> {
    match cmd {
        LegacyCmd::Standup(args) => standup::run(args, verbose).await,
        LegacyCmd::ChangePlan(args) => change_plan::run(args, verbose).await,
        LegacyCmd::Admin { cmd } => admin::run(cmd, verbose).await,
        LegacyCmd::Workflow { cmd } => workflow::run(cmd),
        LegacyCmd::Webui { cmd } => webui::run(cmd).await,
    }
}
