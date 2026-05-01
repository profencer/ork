//! Forbid infra crate imports under `crates/ork-app/src` + `tests` (ADR 0049).

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {:?}: {}", dir.display(), e));
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
        fs::read_to_string(path).unwrap_or_else(|e| panic!("read {:?}: {}", path.display(), e));
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        for banned in ["axum", "sqlx", "reqwest", "rmcp", "rskafka"] {
            let prefix = format!("use {banned}");
            let pubpfx = format!("pub use {banned}");
            if line.starts_with(&prefix) || line.starts_with(&pubpfx) {
                panic!(
                    "forbidden `{banned}` import in ork-app at {}:{}: {}",
                    path.display(),
                    i + 1,
                    raw.trim_end()
                );
            }
        }
    }
}

#[test]
fn ork_app_tree_has_no_infra_use_imports() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rs_files(&crate_root.join("src"), &mut files);
    collect_rs_files(&crate_root.join("tests"), &mut files);
    files.sort();
    assert!(!files.is_empty(), "expected scanned .rs files");
    for f in files {
        check_file(&f);
    }
}
