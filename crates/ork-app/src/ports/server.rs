//! Hexagonal HTTP server stub (ADR 0056 expands this surface).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use tokio::sync::oneshot;

use crate::types::ServerConfig;

#[async_trait]
pub trait Server: Send + Sync {
    /// Starts listening using `config` (host/port); returns once sockets accept.
    async fn start(&self, config: Arc<ServerConfig>) -> Result<ServeHandle, OrkError>;
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
