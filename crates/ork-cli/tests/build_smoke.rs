//! ADR-0057 §`Acceptance criteria` #3: `ork build` produces a release
//! binary and (if `ork-studio` is in deps) a Studio bundle.
//!
//! v1 doesn't ship the Studio bundle (per the user-confirmed scope: the
//! `ork-studio` crate is ADR-0055's responsibility) so this test only
//! covers the release build + manifest extraction round-trip.
//!
//! `#[ignore]` because it shells out to `cargo build --release` which is
//! slow even for the example fixture. Run explicitly with
//! `cargo test -p ork-cli --test build_smoke -- --ignored`.

use std::process::Command;

#[test]
#[ignore = "shells out to `cargo build --release`; slow"]
fn build_fixture_then_manifest_extract() {
    // 1. Pre-build the fixture in --release so we exercise the release path
    //    without invoking `ork build` itself (which goes through
    //    cargo_metadata + targets the *workspace* binary, not an example).
    let status = Command::new("cargo")
        .args(["build", "--release", "--example", "ork-build-fixture"])
        .status()
        .expect("cargo build --release --example");
    assert!(status.success());

    let target_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("release")
        .join("examples");
    let bin = {
        let candidate = target_dir.join("ork-build-fixture");
        if candidate.exists() {
            candidate
        } else {
            target_dir.join("ork-build-fixture.exe")
        }
    };
    assert!(bin.exists(), "fixture not at {}", bin.display());

    // 2. Spawn the binary with ORK_INSPECT_MANIFEST=1 — this is what
    //    `ork build` does internally to write `target/release/ork-manifest.json`.
    let output = Command::new(&bin)
        .env("ORK_INSPECT_MANIFEST", "1")
        .output()
        .expect("spawn fixture");
    assert!(
        output.status.success(),
        "fixture exited with status {}; stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("inspect stdout should be JSON");
    assert!(manifest.get("environment").is_some());
    assert!(manifest.get("ork_version").is_some());
}
