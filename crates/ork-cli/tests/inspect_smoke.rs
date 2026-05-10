//! ADR-0057 §`Acceptance criteria` #6: `ork inspect <binary>` prints
//! AppManifest as JSON; `--format table` prints a summary.

use assert_cmd::Command;
use predicates::prelude::*;

fn build_inspect_fixture() -> std::path::PathBuf {
    // `cargo test` ensures examples are built when --tests is implied,
    // but to be safe we explicitly build the example here. This mirrors
    // what `ork inspect <bin>` does to a user-built binary.
    let status = std::process::Command::new("cargo")
        .args(["build", "--example", "ork-inspect-fixture"])
        .status()
        .expect("cargo build --example");
    assert!(status.success(), "cargo build --example failed");
    let target_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join("examples");
    let candidate = target_dir.join("ork-inspect-fixture");
    if candidate.exists() {
        candidate
    } else {
        target_dir.join("ork-inspect-fixture.exe")
    }
}

#[test]
fn inspect_path_prints_manifest_json() {
    let fixture = build_inspect_fixture();
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.arg("inspect").arg(&fixture);
    let assert = cmd.assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("inspect output should be JSON");
    assert!(parsed.get("environment").is_some(), "missing environment");
    assert!(parsed.get("agents").is_some(), "missing agents");
    assert!(parsed.get("workflows").is_some(), "missing workflows");
    assert!(parsed.get("tools").is_some(), "missing tools");
    assert!(parsed.get("server").is_some(), "missing server");
    assert!(parsed.get("ork_version").is_some(), "missing ork_version");
}

#[test]
fn inspect_table_format_prints_summary() {
    let fixture = build_inspect_fixture();
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.arg("inspect")
        .arg(&fixture)
        .arg("--format")
        .arg("table");
    cmd.assert().success().stdout(
        predicate::str::contains("environment :")
            .and(predicate::str::contains("agents      :"))
            .and(predicate::str::contains("workflows   :"))
            .and(predicate::str::contains("tools       :")),
    );
}

#[test]
fn inspect_missing_target_errors() {
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.arg("inspect").arg("/nonexistent/path/to/ork-bin");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}
