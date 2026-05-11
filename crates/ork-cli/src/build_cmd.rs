//! `ork build` — release build + manifest extraction + Studio bundle.
//! ADR-0057 §`ork build` + ADR-0055 §`Mount mechanics`.
//!
//! v1 produces:
//! 1. The Studio SPA bundle (`pnpm install --frozen-lockfile && pnpm build`
//!    inside `crates/ork-studio/web/`) when the user binary depends on
//!    `ork-studio`. Bundle is hashed against `src/`, `package.json`,
//!    `pnpm-lock.yaml`, and `vite.config.ts`; an unchanged hash skips
//!    the build so iteration stays fast.
//! 2. `target/release/<bin>` (via `cargo build --release --bin <name>`).
//! 3. `target/release/ork-manifest.json` next to the binary, captured
//!    by spawning the binary with `ORK_INSPECT_MANIFEST=1` (ADR-0057
//!    §`--ork-inspect-manifest`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

use crate::dev::builder::{BuildOutcome, cargo_build};
use crate::dev::discovery;

#[derive(Args)]
pub struct BuildArgs {
    /// Build in release mode. Defaults to true; pass `--release=false` for a
    /// debug profile build (rare, but useful for measuring bundle size).
    #[arg(long, default_value_t = true)]
    pub release: bool,

    /// Skip the Studio bundle build even when `ork-studio` is in the
    /// user binary's dep closure. Useful in CI runners that prebuild the
    /// SPA elsewhere.
    #[arg(long, default_value_t = false)]
    pub no_studio_bundle: bool,
}

pub async fn run(args: BuildArgs) -> Result<()> {
    let resolved = discovery::resolve_user_bin().context("ork build: discover user binary")?;
    eprintln!(
        "ork build: target = {} (workspace at {})",
        resolved.bin_name,
        resolved.workspace_root.display()
    );

    // ADR-0055: build the Studio frontend bundle BEFORE the cargo
    // build so `rust-embed` picks up the freshly-built `web/dist/`
    // contents. The bundle-hash cache makes the no-source-change
    // rebuild a no-op.
    if !args.no_studio_bundle && has_dep(&resolved.workspace_root, "ork-studio") {
        build_studio_bundle(&resolved.workspace_root)
            .await
            .context("ork build: studio bundle")?;
    }

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

/// Run `pnpm install --frozen-lockfile && pnpm build` inside
/// `<workspace>/crates/ork-studio/web/` when the bundle hash differs
/// from the cached `target/.ork-studio-bundle.hash`. Skips silently if
/// the workspace doesn't actually contain `crates/ork-studio/web/`
/// (e.g. the user vendored the bundle into their own project layout).
async fn build_studio_bundle(workspace_root: &Path) -> Result<()> {
    let web = workspace_root.join("crates").join("ork-studio").join("web");
    if !web.exists() {
        eprintln!(
            "ork build: ork-studio in dep closure but `{}` not on disk; skipping bundle build.",
            web.display()
        );
        return Ok(());
    }

    let current_hash = hash_studio_sources(&web)?;
    let cache_path = workspace_root
        .join("target")
        .join(".ork-studio-bundle.hash");
    if let Ok(cached) = std::fs::read_to_string(&cache_path)
        && cached.trim() == current_hash
    {
        eprintln!("ork build: studio bundle hash unchanged — skipping pnpm build");
        return Ok(());
    }

    let pnpm =
        which::which("pnpm").map_err(|e| anyhow::anyhow!("ork build: pnpm not on PATH: {e}"))?;

    eprintln!("ork build: pnpm install --frozen-lockfile (studio)");
    let install = tokio::process::Command::new(&pnpm)
        .args(["install", "--frozen-lockfile"])
        .current_dir(&web)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawn pnpm install")?;
    if !install.success() {
        bail!("ork build: pnpm install failed");
    }

    eprintln!("ork build: pnpm build (studio)");
    let build = tokio::process::Command::new(&pnpm)
        .args(["build"])
        .current_dir(&web)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawn pnpm build")?;
    if !build.success() {
        bail!("ork build: pnpm build failed");
    }

    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&cache_path, &current_hash)
        .with_context(|| format!("write {}", cache_path.display()))?;

    Ok(())
}

fn hash_studio_sources(web: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    // Hash a stable, sorted set of files. Reviewer m8: key by the
    // relative-to-`web` path string with `/` separators so the hash
    // is identical on macOS / Linux / Windows checkouts (the raw
    // `OsStr::as_encoded_bytes` would diverge across platforms).
    let pinned = [
        "package.json",
        "pnpm-lock.yaml",
        "vite.config.ts",
        "tsconfig.json",
        "tailwind.config.ts",
        "postcss.config.js",
        "index.html",
    ];
    for name in &pinned {
        let p = web.join(name);
        if let Ok(bytes) = std::fs::read(&p) {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update(&bytes);
            hasher.update(b"\0");
        }
    }
    walk_into(&mut hasher, web, &web.join("src"))?;
    walk_into(&mut hasher, web, &web.join("scripts"))?;
    let digest = hasher.finalize();
    Ok(format!("{digest:x}"))
}

fn walk_into(hasher: &mut Sha256, root: &Path, dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let ft = entry.file_type().context("file_type")?;
        if ft.is_dir() {
            walk_into(hasher, root, &path)?;
        } else if ft.is_file() {
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let rel = path
                .strip_prefix(root)
                .with_context(|| format!("strip {} from {}", root.display(), path.display()))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            hasher.update(&bytes);
            hasher.update(b"\0");
        }
    }
    Ok(())
}
