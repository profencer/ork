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
///
/// A second variant ([`Self::inspect_only`]) is returned by
/// [`crate::OrkApp::serve`] when the binary was invoked with
/// `--ork-inspect-manifest` (or `ORK_INSPECT_MANIFEST=1`) — see ADR-0057
/// `ork inspect`. That handle never bound a TCP listener and shutdown is
/// a no-op.
pub struct ServeHandle {
    /// `0.0.0.0:0` for [`Self::inspect_only`] handles.
    pub local_addr: SocketAddr,
    inner: ServeHandleInner,
}

enum ServeHandleInner {
    Real {
        shutdown_tx: oneshot::Sender<()>,
        join: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    },
    InspectOnly,
}

impl ServeHandle {
    pub fn new(
        local_addr: SocketAddr,
        shutdown_tx: oneshot::Sender<()>,
        join: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    ) -> Self {
        Self {
            local_addr,
            inner: ServeHandleInner::Real { shutdown_tx, join },
        }
    }

    /// No-op handle for the manifest-inspection early-exit (ADR-0057).
    /// `local_addr` is the unspecified `0.0.0.0:0`; [`Self::shutdown`] is
    /// a synchronous `Ok`.
    #[must_use]
    pub fn inspect_only() -> Self {
        Self {
            local_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            inner: ServeHandleInner::InspectOnly,
        }
    }

    /// True iff this handle was returned by the manifest-inspection
    /// early-exit and never bound a listener.
    #[must_use]
    pub fn is_inspect_only(&self) -> bool {
        matches!(self.inner, ServeHandleInner::InspectOnly)
    }

    /// Signal graceful shutdown and wait up to 5 seconds for the server task.
    pub async fn shutdown(self) -> Result<(), OrkError> {
        let (shutdown_tx, join) = match self.inner {
            ServeHandleInner::Real { shutdown_tx, join } => (shutdown_tx, join),
            ServeHandleInner::InspectOnly => return Ok(()),
        };
        let _ = shutdown_tx.send(());
        let join_result = tokio::time::timeout(Duration::from_secs(5), join)
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

    /// Block until SIGINT (Ctrl-C), then trigger graceful shutdown.
    /// Convenience for the binary-as-composition-root pattern that
    /// the ADR-0057 `ork init` template uses:
    ///
    /// ```ignore
    /// let handle = app.serve().await?;
    /// handle.wait_for_shutdown_signal().await?;
    /// ```
    ///
    /// Returns immediately for [`Self::inspect_only`] handles.
    pub async fn wait_for_shutdown_signal(self) -> Result<(), OrkError> {
        if self.is_inspect_only() {
            return Ok(());
        }
        // tokio::signal::ctrl_c is a documented part of `tokio` (full features) and
        // works on every platform we support. Failure to install the handler is
        // surfaced rather than swallowed so the operator sees the cause.
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| OrkError::Internal(format!("ork-app: install ctrl_c handler: {e}")))?;
        self.shutdown().await
    }
}
