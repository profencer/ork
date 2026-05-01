//! Minimal axum server seed for [`ork_app::OrkApp::serve`] (ADR 0056).

use std::sync::Arc;

use async_trait::async_trait;
use axum::{Router, http::StatusCode, routing::get};
use ork_app::types::ServerConfig;
use ork_app::{ServeHandle, Server};
use ork_common::error::OrkError;
use tokio::sync::oneshot;

/// Stateless HTTP bootstrap; listens on `[ServerConfig.host]:[ServerConfig.port]`.
///
/// Serves [`GET /healthz`](crate) returning HTTP 200.
pub struct AxumServer;

#[async_trait]
impl Server for AxumServer {
    async fn start(&self, config: Arc<ServerConfig>) -> Result<ServeHandle, OrkError> {
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

        let app = Router::new().route("/healthz", get(|| async { StatusCode::OK }));

        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });

        let join = tokio::spawn(async move { serve.await });

        Ok(ServeHandle::new(local_addr, shutdown_tx, join))
    }
}
