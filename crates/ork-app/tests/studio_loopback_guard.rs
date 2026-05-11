//! ADR-0055 AC #4: `OrkApp::serve()` refuses to start Studio on a
//! non-loopback bind when `StudioConfig::Enabled` (no auth) is set.
//! `Disabled` and `EnabledWithAuth(_)` are always permitted.

use std::sync::Arc;

use ork_app::OrkApp;
use ork_app::types::{ServerConfig, StudioAuth, StudioConfig};
use ork_common::error::OrkError;
use ork_server::AxumServer;
use secrecy::SecretString;

fn build(host: &str, studio: StudioConfig) -> OrkApp {
    OrkApp::builder()
        .server(ServerConfig {
            host: host.into(),
            port: 0,
            studio,
            ..ServerConfig::default()
        })
        .serve_backend(Arc::new(AxumServer))
        .build()
        .expect("build app")
}

#[tokio::test]
async fn non_loopback_with_enabled_is_rejected() {
    let app = build("0.0.0.0", StudioConfig::Enabled);
    match app.serve().await {
        Ok(_) => panic!("guard must reject non-loopback bind"),
        Err(OrkError::Configuration { message }) => {
            assert!(
                message.contains("studio refuses non-loopback bind without auth"),
                "expected ADR-0055 message; got: {message}"
            );
        }
        Err(other) => panic!("expected OrkError::Configuration, got {other:?}"),
    }
}

#[tokio::test]
async fn non_loopback_with_enabled_with_auth_is_allowed_to_proceed() {
    // We don't actually bind a public port in CI; assert the guard
    // does NOT reject by booting with port 0 on 127.0.0.1 plus
    // StudioAuth. The guard runs against `cfg.host`, so we use a
    // non-loopback host. We expect the bind itself to fail in CI
    // sandboxes, so any error other than the guard's text is fine.
    let auth =
        StudioAuth::new(SecretString::from("test-token".to_string())).expect("non-empty token");
    let app = build("0.0.0.0", StudioConfig::EnabledWithAuth(auth));
    match app.serve().await {
        Ok(handle) => {
            // Lucky bind; shut it down so the test doesn't leak.
            let _ = handle.shutdown().await;
        }
        Err(OrkError::Configuration { message }) => {
            assert!(
                !message.contains("studio refuses non-loopback bind without auth"),
                "guard fired when EnabledWithAuth is set: {message}"
            );
        }
        Err(_) => {
            // Listener bind failure (sandbox-dependent) is acceptable;
            // the guard did not reject, which is what we're proving.
        }
    }
}

#[tokio::test]
async fn loopback_with_enabled_is_allowed() {
    // 127.0.0.1, port 0 → ephemeral port. Guard must not fire.
    let app = build("127.0.0.1", StudioConfig::Enabled);
    let handle = app.serve().await.expect("loopback + Enabled is allowed");
    handle.shutdown().await.expect("graceful shutdown");
}

#[tokio::test]
async fn disabled_is_always_allowed() {
    let app = build("0.0.0.0", StudioConfig::Disabled);
    // Bind may fail in CI sandboxes; we only care that the guard didn't reject.
    match app.serve().await {
        Ok(handle) => {
            let _ = handle.shutdown().await;
        }
        Err(OrkError::Configuration { message }) => {
            assert!(
                !message.contains("studio refuses"),
                "guard fired when Studio is Disabled: {message}"
            );
        }
        Err(_) => {}
    }
}
