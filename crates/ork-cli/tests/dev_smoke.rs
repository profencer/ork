//! ADR-0057 §`Acceptance criteria` #2: `ork dev` boots the user's
//! binary, watches `src/` for changes, restarts on edits, forwards
//! stdout/stderr.
//!
//! `#[ignore]` because it scaffolds a project, runs `cargo build`
//! against the in-repo ork crates, then writes a file change and waits
//! for the second build. Wall-clock time is dominated by the initial
//! cargo build (~30s+ on a cold cache); too slow for the default
//! `cargo test --workspace` gate. Run explicitly with:
//!
//!   cargo test -p ork-cli --test dev_smoke -- --ignored --nocapture

use std::path::Path;
use std::time::Duration;

use assert_cmd::Command as AssertCommand;

#[tokio::test]
#[ignore = "scaffolds a project, runs `cargo build` end-to-end; slow"]
async fn dev_boots_then_rebuilds_on_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("dev-smoke");
    let ork_root = ork_workspace_root();

    // Scaffold via `ork init --ork-source <ork checkout>` so cargo build works.
    let mut init = AssertCommand::cargo_bin("ork").expect("ork bin");
    init.current_dir(dir.path())
        .arg("init")
        .arg("dev-smoke")
        .arg("--template")
        .arg("minimal")
        .arg("--ork-source")
        .arg(&ork_root);
    init.assert().success();

    // Spawn `ork dev` from the scaffolded project.
    let port = pick_port();
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ork"));
    cmd.current_dir(&project)
        .arg("dev")
        .arg("--port")
        .arg(port.to_string())
        .arg("--no-studio")
        .arg("--no-open");
    cmd.kill_on_drop(true);
    let mut dev = cmd.spawn().expect("spawn `ork dev`");

    // Wait for first /readyz.
    wait_ready(port, Duration::from_secs(180))
        .await
        .expect("initial readyz");

    // Capture mtime of the binary (artifact path) before and after the edit.
    let bin_path = project.join("target").join("debug").join("dev-smoke");
    let bin_path = if bin_path.exists() {
        bin_path
    } else {
        bin_path.with_extension("exe")
    };
    let mtime_before = std::fs::metadata(&bin_path).unwrap().modified().unwrap();

    // Edit src/main.rs to trigger a rebuild.
    let main_rs = project.join("src/main.rs");
    let prev = std::fs::read_to_string(&main_rs).unwrap();
    let edited = format!(
        "// dev_smoke edit at {}\n{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        prev
    );
    std::fs::write(&main_rs, edited).unwrap();

    // Wait for the binary mtime to bump (proxy for "rebuilt").
    let rebuilt = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(meta) = std::fs::metadata(&bin_path)
                && let Ok(mt) = meta.modified()
                && mt > mtime_before
            {
                return;
            }
        }
    })
    .await;
    assert!(
        rebuilt.is_ok(),
        "binary never rebuilt after src/main.rs edit"
    );

    // After the rebuild, /readyz should be reachable again.
    wait_ready(port, Duration::from_secs(60))
        .await
        .expect("readyz after rebuild");

    let _ = dev.start_kill();
    let _ = dev.wait().await;
}

async fn wait_ready(port: u16, budget: Duration) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/readyz");
    let started = std::time::Instant::now();
    while started.elapsed() < budget {
        if let Ok(resp) = reqwest::get(&url).await
            && resp.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!("never ready on {url} within {budget:?}"))
}

fn ork_workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/ork-cli")
        .to_path_buf()
}

fn pick_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    l.local_addr().unwrap().port()
}
