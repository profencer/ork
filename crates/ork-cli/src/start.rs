//! `ork start` — runs the built binary in production mode.
//! ADR-0057 §`ork start`.
//!
//! The CLI does **not** background the process; the operator runs under
//! systemd / k8s / nomad / their existing supervisor. This is the
//! deployment boundary documented in ADR-0023.
//!
//! The contract with the user binary is the env-var surface:
//! - `ORK_PRODUCTION=1` — the binary should construct
//!   [`ork_app::types::ServerConfig::production`] (Studio off, swagger off,
//!   `resume_on_startup=true`).
//! - `ORK_DISABLE_STUDIO=1` — overrides any Studio default.
//! - `PORT=<u16>` — listen port.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::dev::discovery;

#[derive(Args)]
pub struct StartArgs {
    /// Path to a built ork binary. Defaults to `target/release/<resolved bin>`.
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Listen port. Forwarded as `PORT=<port>` to the user binary.
    #[arg(long, default_value_t = 4111u16)]
    pub port: u16,

    /// Re-enable Studio (ADR-0055). Studio is off by default in `ork start`
    /// (production posture).
    #[arg(long, default_value_t = false)]
    pub enable_studio: bool,
}

pub async fn run(args: StartArgs) -> Result<()> {
    let bin = match args.bin {
        Some(p) => p,
        None => default_release_bin().context(
            "ork start: could not resolve a default release binary; pass --bin <path> \
             or run from a workspace whose user crate is discoverable (see `ork dev`'s \
             cargo metadata heuristics).",
        )?,
    };
    if !bin.exists() {
        bail!(
            "ork start: binary {} does not exist; build it with `ork build` first.",
            bin.display()
        );
    }

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.env("ORK_PRODUCTION", "1");
    cmd.env("PORT", args.port.to_string());
    if !args.enable_studio {
        cmd.env("ORK_DISABLE_STUDIO", "1");
    }
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;

    let exit = tokio::select! {
        status = child.wait() => status.context("wait for ork app binary")?,
        _ = tokio::signal::ctrl_c() => {
            // Forward SIGINT semantics: kill_on_drop will reap; explicitly start_kill()
            // keeps Windows happy where SIGTERM doesn't exist.
            let _ = child.start_kill();
            child.wait().await.context("wait for ork app binary after SIGINT")?
        }
    };

    if !exit.success() {
        bail!("ork app binary exited with status {exit}");
    }
    Ok(())
}

fn default_release_bin() -> Result<PathBuf> {
    let resolved = discovery::resolve_user_bin()?;
    let mut p = resolved.workspace_target_dir.clone();
    p.push("release");
    p.push(&resolved.bin_name);
    if cfg!(windows) {
        p.set_extension("exe");
    }
    Ok(p)
}
