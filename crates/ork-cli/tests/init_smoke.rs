//! ADR-0057 §`Acceptance criteria` #5: `ork init my-app --template minimal`
//! creates a scaffolded project that `cargo build`s.
//!
//! The structural part runs by default (file tree, placeholder
//! substitution). The `cargo build` part is gated behind
//! `#[ignore]` because it pulls and compiles the full ork workspace
//! and is too slow for the default `cargo test --workspace` gate.
//! Run with `cargo test -p ork-cli --test init_smoke -- --ignored --nocapture`.

use std::path::Path;

use assert_cmd::Command;

#[test]
fn init_minimal_creates_expected_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.current_dir(dir.path())
        .arg("init")
        .arg("hello-ork")
        .arg("--template")
        .arg("minimal");
    cmd.assert().success();

    let project = dir.path().join("hello-ork");
    assert_file(&project, "Cargo.toml");
    assert_file(&project, ".gitignore");
    assert_file(&project, "README.md");
    assert_file(&project, "src/main.rs");

    let cargo_toml = std::fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml.contains("name = \"hello-ork\""),
        "{{name}} not substituted: {cargo_toml}"
    );
    assert!(
        cargo_toml.contains("git = \"https://github.com/your-org/ork\""),
        "default ork dep should be the placeholder git URL: {cargo_toml}"
    );

    let main_rs = std::fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains("hello-ork is listening on"),
        "{{name}} not substituted into main.rs: {main_rs}"
    );
}

#[test]
fn init_minimal_with_ork_source_uses_path_deps() {
    let ork_root = ork_workspace_root();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.current_dir(dir.path())
        .arg("init")
        .arg("hello-ork")
        .arg("--template")
        .arg("minimal")
        .arg("--ork-source")
        .arg(&ork_root);
    cmd.assert().success();

    let cargo_toml = std::fs::read_to_string(dir.path().join("hello-ork/Cargo.toml")).unwrap();
    let app_dep_path = ork_root.join("crates").join("ork-app");
    assert!(
        cargo_toml.contains(&format!("path = \"{}\"", app_dep_path.display())),
        "expected path dep to ork-app in: {cargo_toml}"
    );
}

#[test]
fn init_unknown_template_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.current_dir(dir.path())
        .arg("init")
        .arg("hello-ork")
        .arg("--template")
        .arg("eval");
    cmd.assert().failure().stderr(predicates::str::contains(
        "template `Eval` is not yet available",
    ));
}

#[test]
fn init_existing_directory_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("hello-ork")).unwrap();
    let mut cmd = Command::cargo_bin("ork").expect("ork bin");
    cmd.current_dir(dir.path())
        .arg("init")
        .arg("hello-ork")
        .arg("--template")
        .arg("minimal");
    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));
}

#[test]
#[ignore = "scaffolds a project and runs `cargo build` against the in-repo crates; very slow"]
fn init_minimal_then_cargo_build() {
    let ork_root = ork_workspace_root();
    let dir = tempfile::tempdir().expect("tempdir");

    let mut init = Command::cargo_bin("ork").expect("ork bin");
    init.current_dir(dir.path())
        .arg("init")
        .arg("hello-ork")
        .arg("--template")
        .arg("minimal")
        .arg("--ork-source")
        .arg(&ork_root);
    init.assert().success();

    let project = dir.path().join("hello-ork");
    let mut build = std::process::Command::new("cargo");
    build.current_dir(&project).arg("build");
    let output = build.output().expect("cargo build");
    assert!(
        output.status.success(),
        "cargo build failed in scaffolded project. stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_file(root: &Path, rel: &str) {
    let p = root.join(rel);
    assert!(p.is_file(), "expected file {} to exist", p.display());
}

fn ork_workspace_root() -> std::path::PathBuf {
    // crates/ork-cli/tests/ → ../../..
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/ork-cli")
        .to_path_buf()
}
