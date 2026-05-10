//! ADR-0054 acceptance criterion `CI grep`: no source file under
//! `crates/ork-eval/src/` may import `axum`, `reqwest`, `rmcp`, or
//! `rskafka`. ork-eval is a domain crate that talks to LLMs through
//! `ork_core::ports::llm::LlmProvider` and to Postgres through a
//! port (the future `ScorerResultsRepo`); the listed infra crates
//! never appear in its imports.
//!
//! Mirrors the parallel guard in
//! [`crates/ork-app/tests/no_infra_imports.rs`](../../ork-app/tests/no_infra_imports.rs)
//! per ADR-0049.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {}", dir.display(), e));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry: {e}"));
        let path = entry.path();
        let name = path.file_name().and_then(OsStr::to_str);
        if path.is_dir() {
            if name == Some("target") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension() == Some(OsStr::new("rs")) {
            out.push(path);
        }
    }
}

fn check_file(path: &Path) {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        for banned in ["axum", "reqwest", "rmcp", "rskafka"] {
            let prefix = format!("use {banned}");
            let pubpfx = format!("pub use {banned}");
            if line.starts_with(&prefix) || line.starts_with(&pubpfx) {
                panic!(
                    "ADR-0054: forbidden `{banned}` import in ork-eval at {}:{}: {}",
                    path.display(),
                    i + 1,
                    raw.trim_end()
                );
            }
        }
    }
}

#[test]
fn ork_eval_src_has_no_forbidden_infra_imports() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = crate_root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    files.sort();
    assert!(!files.is_empty(), "expected to scan .rs files under src/");
    for f in files {
        check_file(&f);
    }
}

#[test]
fn ork_eval_cargo_toml_excludes_forbidden_infra_deps() {
    let cargo = include_str!("../Cargo.toml");
    let forbidden = ["axum", "reqwest", "rmcp", "rskafka"];

    #[derive(Copy, Clone)]
    enum Sec {
        Other,
        Deps,
    }

    let mut section = Sec::Other;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = match trimmed {
                "[dependencies]" => Sec::Deps,
                _ => Sec::Other,
            };
            continue;
        }
        if matches!(section, Sec::Deps) && !trimmed.is_empty() && !trimmed.starts_with('#') {
            let first = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches(':');
            for b in forbidden {
                assert_ne!(
                    first, b,
                    "ADR-0054: forbidden direct dependency `{b}` in [dependencies] of ork-eval/Cargo.toml"
                );
            }
        }
    }
}
