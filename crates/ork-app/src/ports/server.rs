//! Hexagonal HTTP server port (ADR 0056). The auto-generated REST + SSE
//! surface lives in `ork-api`; the adapter (`ork_server::AxumServer`)
//! consumes [`crate::OrkApp`] and `config` to build that router.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use tokio::sync::oneshot;

use crate::OrkApp;
use crate::types::ServerConfig;

#[async_trait]
pub trait Server: Send + Sync {
    /// Starts listening using `config` (host/port). The adapter walks
    /// `app.manifest()` (and the live registry on `app`) to materialise
    /// the routes per ADR-0056.
    async fn start(&self, app: OrkApp, config: Arc<ServerConfig>) -> Result<ServeHandle, OrkError>;
}

/// Handle returned by [`Server::start`]: local address plus graceful shutdown within 5s.
pub struct ServeHandle {
    pub local_addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl ServeHandle {
    pub fn new(
        local_addr: SocketAddr,
        shutdown_tx: oneshot::Sender<()>,
        join: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    ) -> Self {
        Self {
            local_addr,
            shutdown_tx,
            join,
        }
    }

    /// Signal graceful shutdown and wait up to 5 seconds for the server task.
    pub async fn shutdown(self) -> Result<(), OrkError> {
        let _ = self.shutdown_tx.send(());
        let join_result = tokio::time::timeout(Duration::from_secs(5), self.join)
            .await
            .map_err(|_| {
                OrkError::Internal(
                    "ork-app: HTTP server graceful shutdown exceeded 5s timeout".into(),
                )
            })?
            .map_err(|e| {
                OrkError::Internal(format!(
                    "ork-app: HTTP server task panicked/join failed: {e}"
                ))
            })?;
        join_result
            .map_err(|e| OrkError::Internal(format!("ork-app: HTTP serve task ended: {e}")))?;
        Ok(())
    }
}
