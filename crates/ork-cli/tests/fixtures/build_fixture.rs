//! Real OrkApp binary used by `build_smoke.rs` to exercise the
//! `cargo build` + manifest-extraction round-trip that `ork build`
//! orchestrates. Distinct from `inspect_fixture.rs` because this one
//! depends on `ork-server` (so the workspace's release build genuinely
//! produces a runnable artefact).

use std::sync::Arc;

use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_server::AxumServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tracing → stderr keeps stdout clean for `--ork-inspect-manifest`.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port,
            ..ServerConfig::default()
        })
        .serve_backend(Arc::new(AxumServer))
        .build()?;
    let handle = app.serve().await?;
    if handle.is_inspect_only() {
        return Ok(());
    }
    handle.wait_for_shutdown_signal().await?;
    Ok(())
}
