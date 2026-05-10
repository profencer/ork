//! `ork dev` — boot the user binary, watch source, restart on edits.
//! ADR-0057 §`ork dev`.
//!
//! Topology: a single `tokio::select!` supervisor consumes
//! - `ChangeEvent` from the [`watcher`] (debounced),
//! - completion of the in-flight build task,
//! - completion of the running child (e.g. it crashed),
//! - `ctrl_c()` from the operator.
//!
//! Hot reload is binary restart, not in-process patching (see ADR §`ork dev`
//! paragraph 6). On a successful rebuild the old child is SIGTERMed,
//! the new one spawned, and `await_ready()` blocks until `/readyz`
//! returns 200.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use tokio::sync::mpsc;

pub mod builder;
pub mod child;
pub mod discovery;
pub mod studio;
pub mod watcher;

use builder::{BuildOutcome, cargo_build};
use child::AppChild;
use watcher::spawn_watcher;

#[derive(Args)]
pub struct DevArgs {
    /// Listen port forwarded to the user binary as `PORT=<port>`.
    #[arg(long, default_value_t = 4111u16)]
    pub port: u16,

    /// Skip the Studio open-browser hook; the dev server itself still mounts
    /// whatever the user binary mounts.
    #[arg(long, default_value_t = false)]
    pub no_studio: bool,

    /// Suppress opening a browser even if Studio is enabled. (Today this is
    /// a no-op because the Studio bundle isn't wired — see ADR-0055.)
    #[arg(long, default_value_t = false)]
    pub no_open: bool,

    /// Additional directories to watch beyond the default `src/` and
    /// `workflow-templates/`. Repeatable.
    #[arg(long)]
    pub watch: Vec<PathBuf>,
}

pub async fn run(args: DevArgs) -> Result<()> {
    let resolved = discovery::resolve_user_bin().context("ork dev: discover user binary")?;
    eprintln!(
        "ork dev: target = {} (workspace at {})",
        resolved.bin_name,
        resolved.workspace_root.display()
    );

    let mut watch_roots = vec![
        resolved.workspace_root.join("src"),
        resolved.workspace_root.join("workflow-templates"),
    ];
    watch_roots.extend(args.watch.iter().cloned());

    let (mut watch_rx, _debouncer_keep_alive) =
        spawn_watcher(&watch_roots, Duration::from_millis(200))
            .context("ork dev: spawn file watcher")?;

    // Initial build + spawn.
    let mut current_child: Option<AppChild> = match build_and_spawn(&resolved, args.port).await? {
        Some(child) => Some(child),
        None => {
            // Initial build failed; surface and exit.
            eprintln!("ork dev: initial build failed; exiting (nothing to serve).");
            std::process::exit(1);
        }
    };
    studio::open_browser_if_enabled(args.no_studio, args.no_open, args.port);

    let (build_tx, mut build_rx) = mpsc::channel::<BuildOutcome>(1);
    let mut build_in_flight = false;

    // Coalesce a burst of watch events that arrive while a rebuild is
    // already running; the next iteration triggers exactly one rebuild.
    let mut pending_change = false;

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                eprintln!("ork dev: SIGINT received; shutting down child.");
                if let Some(child) = current_child.take() {
                    let _ = child.terminate(Duration::from_secs(5)).await;
                }
                return Ok(());
            }

            change = watch_rx.recv() => {
                let Some(_evt) = change else {
                    eprintln!("ork dev: watcher channel closed; exiting.");
                    return Ok(());
                };
                if build_in_flight {
                    pending_change = true;
                    continue;
                }
                eprintln!("ork dev: change detected, rebuilding…");
                spawn_rebuild(&resolved, build_tx.clone());
                build_in_flight = true;
            }

            outcome = build_rx.recv(), if build_in_flight => {
                build_in_flight = false;
                let outcome = outcome.expect("rebuild task dropped sender");
                match outcome {
                    BuildOutcome::Failed { stderr } => {
                        eprintln!("ork dev: rebuild failed; keeping previous binary serving.");
                        if !stderr.is_empty() {
                            eprintln!("{stderr}");
                        }
                    }
                    BuildOutcome::Success { artifact, stderr } => {
                        if !stderr.is_empty() {
                            eprintln!("{stderr}");
                        }
                        eprintln!("ork dev: rebuild ok; restarting child");
                        // Reviewer M1: terminate the old child *before* spawning
                        // the new one so the new bind doesn't race the old
                        // process for `args.port`. This trades a brief
                        // unavailability window for correctness; the dev-server
                        // reverse-proxy follow-up ADR is what shrinks the gap
                        // (and would also resurrect a meaningful `OrkApp::reload`).
                        if let Some(old) = current_child.take() {
                            let _ = old.terminate(Duration::from_secs(5)).await;
                        }
                        match AppChild::spawn(&artifact, args.port).await {
                            Ok(mut new) => {
                                if let Err(e) = new.await_ready(Duration::from_secs(30)).await {
                                    eprintln!("ork dev: new child failed readyz: {e:#}");
                                }
                                current_child = Some(new);
                            }
                            Err(e) => {
                                eprintln!(
                                    "ork dev: failed to spawn new child after rebuild: {e:#}; \
                                     waiting for next change event"
                                );
                            }
                        }
                    }
                }
                if pending_change {
                    pending_change = false;
                    eprintln!("ork dev: coalesced change while rebuilding; rebuilding again");
                    spawn_rebuild(&resolved, build_tx.clone());
                    build_in_flight = true;
                }
            }
        }
    }
}

async fn build_and_spawn(resolved: &discovery::ResolvedBin, port: u16) -> Result<Option<AppChild>> {
    eprintln!("ork dev: cargo build --bin {}", resolved.bin_name);
    let outcome = cargo_build(&resolved.workspace_root, &resolved.bin_name, "dev")
        .await
        .context("ork dev: cargo build")?;
    let artifact = match outcome {
        BuildOutcome::Success { artifact, stderr } => {
            if !stderr.is_empty() {
                eprintln!("{stderr}");
            }
            artifact
        }
        BuildOutcome::Failed { stderr } => {
            eprintln!("{stderr}");
            return Ok(None);
        }
    };
    eprintln!("ork dev: spawning {}", artifact.display());
    let mut child = AppChild::spawn(&artifact, port).await?;
    let budget = ready_budget();
    child
        .await_ready(budget)
        .await
        .context("ork dev: wait /readyz")?;
    eprintln!("ork dev: ready on http://127.0.0.1:{port}");
    Ok(Some(child))
}

fn spawn_rebuild(resolved: &discovery::ResolvedBin, tx: mpsc::Sender<BuildOutcome>) {
    let workspace_root = resolved.workspace_root.clone();
    let bin = resolved.bin_name.clone();
    tokio::spawn(async move {
        let outcome = match cargo_build(&workspace_root, &bin, "dev").await {
            Ok(o) => o,
            Err(e) => BuildOutcome::Failed {
                stderr: format!("ork dev: cargo build invocation error: {e:#}"),
            },
        };
        let _ = tx.send(outcome).await;
    });
}

fn ready_budget() -> Duration {
    let ms = std::env::var("ORK_DEV_READY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30_000);
    Duration::from_millis(ms)
}
