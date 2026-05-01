pub mod a2a;
pub mod agent_registry;
pub mod artifact_spill;
pub mod embeds;
pub mod models;
pub mod ports;
pub mod services;
pub mod streaming;
pub mod workflow;

#[cfg(test)]
mod rig_quarantine_guard {
    //! ADR 0047 — `rig` must not leak into dependency hexagon crates.

    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn no_rig_imports_in_hexagon_crates() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        for crate_dir in ["ork-core", "ork-common", "ork-llm", "ork-api"] {
            let root = workspace.join("crates").join(crate_dir);
            scan_dir(&root).unwrap_or_else(|e| panic!("scan {crate_dir}: {e}"));
        }
    }

    fn scan_dir(dir: &Path) -> std::io::Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            if p.is_dir() {
                // skip target/ if ever nested
                if p.file_name() == Some(OsStr::new("target")) {
                    continue;
                }
                scan_dir(&p)?;
            } else if p.extension() == Some(OsStr::new("rs")) {
                let s = fs::read_to_string(&p)?;
                for (i, line) in s.lines().enumerate() {
                    let t = line.trim_start();
                    // Match rig crate paths after a `use` token without embedding a contiguous
                    // `use`+space+`rig` substring (ADR-0047 CI grep gate).
                    let forbidden = t
                        .strip_prefix("use ")
                        .is_some_and(|rest| rest.starts_with("rig::") || rest.starts_with("rig "));
                    if forbidden {
                        panic!(
                            "forbidden `rig` import: {}:{}: {}",
                            p.display(),
                            i + 1,
                            line.trim_end()
                        );
                    }
                }
            }
        }
        Ok(())
    }
}
