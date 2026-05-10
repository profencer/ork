//! Boots [`OrkApp::serve`] with [`ork_server::AxumServer`](../../ork-server/).

use std::sync::Arc;
use std::time::Duration;

use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_server::AxumServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn healthz_then_shutdown_under_five_seconds() {
    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            ..ServerConfig::default()
        })
        .serve_backend(Arc::new(AxumServer))
        .build()
        .expect("build app");

    let handle = app.serve().await.expect("serve");
    let addr = handle.local_addr;

    let body = tokio::time::timeout(Duration::from_secs(5), async {
        let mut tcp = TcpStream::connect(addr).await?;
        tcp.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut buf = vec![0u8; 512];
        let n = tcp.read(&mut buf).await?;
        Result::<_, std::io::Error>::Ok(buf[..n].to_vec())
    })
    .await
    .expect("timed out waiting for GET /healthz (budget 5s)");

    let body = body.expect("http read");
    let head = String::from_utf8_lossy(&body);
    assert!(
        head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200"),
        "unexpected response: {:?}",
        head.lines().next()
    );

    handle
        .shutdown()
        .await
        .expect("graceful shutdown within 5s");

    let reconn = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await;

    match reconn {
        Err(_) => panic!("still waiting on connect after shutdown (5s timeout)"),
        Ok(Ok(_)) => panic!("unexpected successful TCP reconnect after shutdown"),
        Ok(Err(_)) => {}
    }
}

/// ADR-0057 §`--ork-inspect-manifest`: when `ORK_INSPECT_MANIFEST=1` is in
/// the environment, `OrkApp::serve()` early-exits with an inspect-only
/// handle and never binds a TCP listener. Run as a separate process via
/// `assert_cmd` so we don't mutate the test harness's environment for
/// other tests running in parallel.
#[test]
fn inspect_manifest_env_returns_inspect_only_handle() {
    let status = std::process::Command::new("cargo")
        .args(["build", "--example", "serve_inspect_only_probe"])
        .status()
        .expect("cargo build --example serve_inspect_only_probe");
    assert!(status.success());

    let bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join("examples")
        .join("serve_inspect_only_probe");
    let bin = if bin.exists() {
        bin
    } else {
        bin.with_extension("exe")
    };

    let output = std::process::Command::new(&bin)
        .env("ORK_INSPECT_MANIFEST", "1")
        .output()
        .expect("spawn probe");
    assert!(
        output.status.success(),
        "probe exited with status {}; stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("inspect stdout should be JSON");
    assert!(manifest.get("environment").is_some());
    assert!(manifest.get("server").is_some());
}
