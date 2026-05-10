//! `ork` CLI entry point. ADR-0057 §`Decision`.
//!
//! Subcommand surface:
//! - `dev` / `build` / `start` — Mastra-style dev loop.
//! - `init` — scaffold a new project (template embedded via include_dir).
//! - `inspect` — print AppManifest from a built binary or a running URL.
//! - `eval` — thin shim over the existing `OrkEval` runner (ADR-0054).
//! - `lint` / `migrate` — clap-visible stubs reserved for follow-up ADRs.
//! - `legacy <subcommand>` — the prior demo verbs.

use anyhow::Result;
use clap::{Parser, Subcommand};

use ork_cli::{
    build_cmd::{self, BuildArgs},
    dev::{self, DevArgs},
    eval::{self, EvalArgs},
    init::{self, InitArgs},
    inspect::{self, InspectArgs},
    legacy::{self, LegacyCmd},
    lint::{self, LintArgs},
    migrate::{self, MigrateArgs},
    start::{self, StartArgs},
};

#[derive(Parser)]
#[command(name = "ork", version, about = "ork — code-first agent platform CLI")]
struct Cli {
    /// Enable verbose output where supported.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Boot OrkApp + REST + SSE; rebuild on source edits.
    Dev(DevArgs),
    /// Release-build the user binary and extract its AppManifest.
    Build(BuildArgs),
    /// Run the built artefact in production mode.
    Start(StartArgs),
    /// Scaffold a new ork project.
    Init(InitArgs),
    /// Print AppManifest for a binary path or HTTP base URL.
    Inspect(InspectArgs),
    /// Run an offline scorer eval (ADR-0054).
    Eval(EvalArgs),
    /// Static checks against the user project (planned for a follow-up ADR).
    Lint(LintArgs),
    /// Apply Postgres / libsql migrations bundled with ork crates (planned for a follow-up ADR).
    Migrate(MigrateArgs),
    /// Existing demo subcommands; will be removed when demos are reauthored against the new platform.
    #[command(subcommand)]
    Legacy(LegacyCmd),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Dev(args) => dev::run(args).await,
        Cmd::Build(args) => build_cmd::run(args).await,
        Cmd::Start(args) => start::run(args).await,
        Cmd::Init(args) => init::run(args),
        Cmd::Inspect(args) => inspect::run(args).await,
        Cmd::Eval(args) => eval::run(args).await,
        Cmd::Lint(args) => lint::run(args),
        Cmd::Migrate(args) => migrate::run(args),
        Cmd::Legacy(cmd) => legacy::run(cmd, cli.verbose).await,
    }
}
