//! CI-style guard: `ork-tool` stays free of gateway / IO crates (ADR-0051).

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &["axum", "sqlx", "reqwest", "rmcp", "rskafka"];

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).expect("read_dir");
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_forbidden_substrings_in_src() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "expected ork-tool/src/**/*.rs");

    let mut violations = Vec::new();
    for f in files {
        let text = fs::read_to_string(&f).expect("read file");
        let rel = f.strip_prefix(manifest_dir).unwrap_or(&f);
        for ban in FORBIDDEN {
            if text.contains(ban) {
                violations.push(format!("{}: contains `{ban}`", rel.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "forbidden imports / mentions:\n{}",
        violations.join("\n")
    );
}
