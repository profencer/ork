//! Minimal OrkApp binary used by `inspect_smoke.rs`. Run with
//! `ORK_INSPECT_MANIFEST=1`; the early-exit in `OrkApp::serve()` prints
//! the manifest and exits without binding a listener — so this fixture
//! does not register any `serve_backend`.

use ork_app::OrkApp;
use ork_app::types::ServerConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tracing → stderr keeps stdout clean for the manifest JSON;
    // a future builder warning would otherwise corrupt smoke-test JSON parsing.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            ..ServerConfig::default()
        })
        .build()?;
    let handle = app.serve().await?;
    if !handle.is_inspect_only() {
        return Err(
            "inspect fixture: OrkApp::serve returned a real handle; this fixture is meant to \
             be run with ORK_INSPECT_MANIFEST=1"
                .into(),
        );
    }
    Ok(())
}
