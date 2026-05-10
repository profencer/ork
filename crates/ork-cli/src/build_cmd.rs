//! `ork build` — release build + manifest extraction. ADR-0057 §`ork build`.
//!
//! v1 produces:
//! 1. `target/release/<bin>` (via `cargo build --release --bin <name>`).
//! 2. `target/release/ork-manifest.json` next to the binary, captured by
//!    spawning the binary with `ORK_INSPECT_MANIFEST=1` (ADR-0057
//!    §`--ork-inspect-manifest`).
//!
//! The Studio bundle build (`pnpm install && pnpm build` from
//! `crates/ork-studio/web/`) is intentionally **not** wired here — the
//! `ork-studio` crate does not exist yet (ADR-0055 is Proposed). When
//! the user binary depends on `ork-webui` (the existing SPA host), this
//! verb prints a heads-up that bundle build/embed lands with ADR-0055.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::dev::builder::{BuildOutcome, cargo_build};
use crate::dev::discovery;

#[derive(Args)]
pub struct BuildArgs {
    /// Build in release mode. Defaults to true; pass `--release=false` for a
    /// debug profile build (rare, but useful for measuring bundle size).
    #[arg(long, default_value_t = true)]
    pub release: bool,
}

pub async fn run(args: BuildArgs) -> Result<()> {
    let resolved = discovery::resolve_user_bin().context("ork build: discover user binary")?;
    eprintln!(
        "ork build: target = {} (workspace at {})",
        resolved.bin_name,
        resolved.workspace_root.display()
    );

    let outcome = cargo_build(
        &resolved.workspace_root,
        &resolved.bin_name,
        if args.release { "release" } else { "dev" },
    )
    .await
    .context("ork build: cargo build")?;

    let artifact = match outcome {
        BuildOutcome::Success { artifact, .. } => artifact,
        BuildOutcome::Failed { stderr } => {
            eprintln!("{stderr}");
            bail!("ork build: cargo build failed");
        }
    };

    eprintln!("ork build: produced {}", artifact.display());

    let manifest_path = manifest_path_for(&artifact);
    write_manifest_json(&artifact, &manifest_path)
        .await
        .context("ork build: extract --ork-inspect-manifest")?;
    eprintln!("ork build: wrote manifest to {}", manifest_path.display());

    if has_dep(&resolved.workspace_root, "ork-studio")
        || has_dep(&resolved.workspace_root, "ork-webui")
    {
        eprintln!(
            "ork build: studio bundle build/embed is not wired in this ADR — implement ADR-0055 \
             (the `ork-studio` crate) to enable `--features ork-webui/embed-spa` builds."
        );
    }

    Ok(())
}

fn manifest_path_for(artifact: &Path) -> PathBuf {
    let mut p = artifact
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    p.push("ork-manifest.json");
    p
}

async fn write_manifest_json(bin: &Path, dest: &Path) -> Result<()> {
    let output = tokio::process::Command::new(bin)
        .env("ORK_INSPECT_MANIFEST", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawn {}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "manifest extraction: {} exited with status {}; stderr:\n{}",
            bin.display(),
            output.status,
            stderr
        );
    }
    let manifest_json = String::from_utf8(output.stdout)
        .context("manifest extraction: stdout was not valid UTF-8")?;
    // Re-pretty-print so the file has stable formatting independent of
    // OrkApp::serve's serializer.
    let parsed: serde_json::Value =
        serde_json::from_str(&manifest_json).context("parse manifest JSON")?;
    let pretty = serde_json::to_string_pretty(&parsed)?;
    tokio::fs::write(dest, pretty)
        .await
        .with_context(|| format!("write {}", dest.display()))
}

/// Cheap check: does the workspace root's `Cargo.lock` mention `dep`?
/// Avoids parsing TOML; the lockfile is regenerated on every build so a
/// substring search is sufficient for the ADR-0057 heads-up message.
fn has_dep(workspace_root: &Path, dep: &str) -> bool {
    let lock = workspace_root.join("Cargo.lock");
    let Ok(text) = std::fs::read_to_string(&lock) else {
        return false;
    };
    text.contains(&format!("name = \"{dep}\""))
}
