//! User-binary discovery for `ork dev` / `ork build` / `ork start`.
//!
//! Resolution rules (ADR-0057 §`ork dev`):
//! 1. Honour `[package.metadata.ork] bin = "<name>"` on the workspace
//!    root or any workspace member.
//! 2. Otherwise enumerate `[[bin]]` targets across the workspace and
//!    keep the ones whose transitive deps include `ork-app`. Exactly
//!    one match → use it; 0 → error explaining the metadata key; >1 →
//!    error listing candidates.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cargo_metadata::MetadataCommand;

#[derive(Debug, Clone)]
pub struct ResolvedBin {
    /// Cargo workspace root (parent of the workspace's `Cargo.toml`).
    pub workspace_root: PathBuf,
    /// `cargo metadata`'s `target_directory` (typically `<workspace_root>/target`).
    pub workspace_target_dir: PathBuf,
    /// `[[bin]]` target name to pass to `cargo build --bin <name>`.
    pub bin_name: String,
    /// Path to the package's `Cargo.toml` (useful for diagnostics).
    pub manifest_path: PathBuf,
}

pub fn resolve_user_bin() -> Result<ResolvedBin> {
    let metadata = MetadataCommand::new()
        .exec()
        .context("cargo metadata failed (is `cargo` on PATH?)")?;
    let workspace_root: PathBuf = metadata.workspace_root.clone().into();
    let workspace_target_dir: PathBuf = metadata.target_directory.clone().into();

    if let Some(name) = metadata_bin_override(&metadata) {
        let pkg = metadata
            .packages
            .iter()
            .find(|p| p.targets.iter().any(|t| is_binary(t) && t.name == name))
            .with_context(|| {
                format!(
                    "ork dev: [package.metadata.ork] bin = \"{name}\" but no `[[bin]]` of that \
                     name found in the workspace."
                )
            })?;
        return Ok(ResolvedBin {
            workspace_root,
            workspace_target_dir,
            bin_name: name,
            manifest_path: pkg.manifest_path.clone().into(),
        });
    }

    let workspace_members: HashSet<&cargo_metadata::PackageId> =
        metadata.workspace_members.iter().collect();

    let mut candidates: Vec<(&cargo_metadata::Package, &cargo_metadata::Target)> = Vec::new();
    for pkg in metadata
        .packages
        .iter()
        .filter(|p| workspace_members.contains(&p.id))
    {
        if !package_depends_on_ork_app(&metadata, &pkg.id) {
            continue;
        }
        for tgt in &pkg.targets {
            if is_binary(tgt) {
                candidates.push((pkg, tgt));
            }
        }
    }

    match candidates.len() {
        0 => bail!(
            "ork dev: no workspace binary depends on `ork-app`. Add a `[[bin]]` whose dep \
             closure includes ork-app, or set `[package.metadata.ork] bin = \"<name>\"` on \
             the package you want `ork dev` to run."
        ),
        1 => {
            let (pkg, tgt) = candidates[0];
            Ok(ResolvedBin {
                workspace_root,
                workspace_target_dir,
                bin_name: tgt.name.clone(),
                manifest_path: pkg.manifest_path.clone().into(),
            })
        }
        _ => {
            let names: Vec<String> = candidates.iter().map(|(_, t)| t.name.clone()).collect();
            bail!(
                "ork dev: multiple workspace binaries depend on `ork-app` ({}). Pick one by \
                 setting `[package.metadata.ork] bin = \"<name>\"` on the owning package.",
                names.join(", ")
            )
        }
    }
}

fn is_binary(t: &cargo_metadata::Target) -> bool {
    t.kind.iter().any(|k| k == "bin")
}

fn metadata_bin_override(meta: &cargo_metadata::Metadata) -> Option<String> {
    let workspace_members: HashSet<&cargo_metadata::PackageId> =
        meta.workspace_members.iter().collect();
    for pkg in meta
        .packages
        .iter()
        .filter(|p| workspace_members.contains(&p.id))
    {
        if let Some(bin) = pkg
            .metadata
            .get("ork")
            .and_then(|m| m.get("bin"))
            .and_then(|v| v.as_str())
        {
            return Some(bin.to_string());
        }
    }
    None
}

/// Walks `metadata.resolve` to check whether `pkg_id` transitively depends
/// on the `ork-app` crate via a *normal* dep edge (not dev/build only —
/// those would be misleading because they don't ship with the runtime
/// binary). Has a seen-set to terminate on cycles.
fn package_depends_on_ork_app(
    meta: &cargo_metadata::Metadata,
    pkg_id: &cargo_metadata::PackageId,
) -> bool {
    let Some(resolve) = meta.resolve.as_ref() else {
        return false;
    };
    let mut seen: HashSet<&cargo_metadata::PackageId> = HashSet::new();
    let mut stack: Vec<&cargo_metadata::PackageId> = vec![pkg_id];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(node) = resolve.nodes.iter().find(|n| n.id == *id) else {
            continue;
        };
        for dep in &node.deps {
            // Reviewer m3: only follow Normal dep edges; Development /
            // Build kinds don't end up in the runtime dep graph and would
            // produce false positives (e.g. an example fixture that
            // depends on ork-app via [dev-dependencies]).
            let is_normal = dep
                .dep_kinds
                .iter()
                .any(|k| matches!(k.kind, cargo_metadata::DependencyKind::Normal));
            if !is_normal {
                continue;
            }
            if let Some(pkg) = meta.packages.iter().find(|p| p.id == dep.pkg)
                && pkg.name.as_str() == "ork-app"
            {
                return true;
            }
            stack.push(&dep.pkg);
        }
    }
    false
}
