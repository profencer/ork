//! axum-backed [`Server`] adapter for [`ork_app::OrkApp::serve`] (ADR-0056).
//!
//! Builds the router via [`ork_api::router_for`], which walks
//! `OrkApp::manifest()` to materialise the auto-generated REST + SSE
//! surface alongside the existing A2A endpoints.

use std::sync::Arc;

use async_trait::async_trait;
use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_app::{ServeHandle, Server};
use ork_common::error::OrkError;
use tokio::sync::oneshot;

/// Stateless HTTP bootstrap; listens on `[ServerConfig.host]:[ServerConfig.port]`.
///
/// Mounts the full ADR-0056 router built from `app.manifest()`.
pub struct AxumServer;

#[async_trait]
impl Server for AxumServer {
    async fn start(&self, app: OrkApp, config: Arc<ServerConfig>) -> Result<ServeHandle, OrkError> {
        let addr = format!("{}:{}", config.host, config.port);
        let listener =
            tokio::net::TcpListener::bind(&addr)
                .await
                .map_err(|e| OrkError::Configuration {
                    message: format!("failed to bind HTTP server to {addr}: {e}"),
                })?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| OrkError::Internal(format!("local_addr after bind: {e}")))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let router = ork_api::router_for(&app, config.as_ref());

        let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });

        let join = tokio::spawn(async move { serve.await });

        Ok(ServeHandle::new(local_addr, shutdown_tx, join))
    }
}
