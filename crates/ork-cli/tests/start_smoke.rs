//! ADR-0057 §`Acceptance criteria` #4: `ork start` runs the built binary
//! with production `ServerConfig` defaults.
//!
//! `#[ignore]` because we shell to `cargo build --release --example`
//! and bind a TCP listener; the default `cargo test --workspace` gate
//! would not enjoy spending the wall-clock time. Run explicitly with
//! `cargo test -p ork-cli --test start_smoke -- --ignored --nocapture`.

use std::path::PathBuf;
use std::time::Duration;

#[tokio::test]
#[ignore = "shells out to `cargo build --release` and binds TCP; slow"]
async fn start_fixture_serves_readyz_with_studio_off() {
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--example", "ork-build-fixture"])
        .status()
        .expect("cargo build --release --example");
    assert!(status.success());
    let bin = release_examples_dir().join("ork-build-fixture");
    assert!(bin.exists(), "fixture missing: {}", bin.display());

    let port = pick_port();

    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ork"));
    cmd.arg("start")
        .arg("--bin")
        .arg(&bin)
        .arg("--port")
        .arg(port.to_string());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn `ork start`");

    // Poll /readyz.
    let url = format!("http://127.0.0.1:{port}/readyz");
    let mut ready = false;
    for _ in 0..100 {
        if let Ok(resp) = reqwest::get(&url).await
            && resp.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "binary never became ready on {url}");

    // /swagger-ui should be off in production-like mode.
    let swagger = reqwest::get(format!("http://127.0.0.1:{port}/swagger-ui"))
        .await
        .expect("GET swagger-ui");
    assert!(
        !swagger.status().is_success() || swagger.status() == reqwest::StatusCode::NOT_FOUND,
        "expected /swagger-ui disabled in `ork start`; got {}",
        swagger.status()
    );

    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn release_examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("release")
        .join("examples")
}

fn pick_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    l.local_addr().unwrap().port()
}
