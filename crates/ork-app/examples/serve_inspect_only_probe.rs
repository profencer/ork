//! ADR-0057 §`--ork-inspect-manifest`: probe used by
//! `crates/ork-app/tests/serve_smoke.rs`. Run with
//! `ORK_INSPECT_MANIFEST=1`; the OrkApp early-exits and prints the
//! manifest JSON to stdout. Asserts no TCP listener was bound by
//! returning before the backend check would fire.

use ork_app::OrkApp;
use ork_app::types::ServerConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Reviewer n5: tracing → stderr keeps stdout clean for the manifest JSON;
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
            "probe expected an inspect-only handle (run with ORK_INSPECT_MANIFEST=1)".into(),
        );
    }
    Ok(())
}
