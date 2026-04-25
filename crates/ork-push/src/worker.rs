//! In-process delivery worker for ADR-0009 push notifications.
//!
//! Workflow:
//!
//! 1. Subscribe to `<namespace>.push.outbox`.
//! 2. For each [`PushOutboxEnvelope`], fetch every `a2a_push_configs` row
//!    registered for the task and tenant.
//! 3. Sign the body with [`JwksProvider::sign_detached`] (the body is the
//!    canonical JSON envelope subscribers receive).
//! 4. POST to each subscriber with the headers documented in ADR-0009:
//!    * `Content-Type: application/json`
//!    * `X-A2A-Signature: <detached JWS>`
//!    * `X-A2A-Key-Id: <kid>`
//!    * `X-A2A-Timestamp: <RFC3339>`
//!    * `Authorization: Bearer <token>` when the config has a token.
//! 5. Retry per `WorkerConfig::retry_minutes` (defaults to `[1, 5, 30]`).
//! 6. On exhaustion write a row to `a2a_push_dead_letter`.
//!
//! The worker is best-effort: a failed delivery never blocks the inbound
//! API request that produced the envelope. Concurrency is bounded by
//! [`WorkerConfig::max_concurrency`] — each in-flight POST holds one slot
//! in a `tokio::sync::Semaphore`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use ork_a2a::topics;
use ork_core::ports::a2a_push_dead_letter_repo::{
    A2aPushDeadLetterRepository, A2aPushDeadLetterRow,
};
use ork_core::ports::a2a_push_repo::A2aPushConfigRepository;
use ork_eventing::EventingClient;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::outbox::PushOutboxEnvelope;
use crate::signing::JwksProvider;

/// Operator-tunable knobs that mirror the `[push]` section of `AppConfig`.
///
/// `retry_intervals` is exposed as `Vec<Duration>` (rather than the wire-level
/// `Vec<u64>` minutes that `AppConfig` carries) so integration tests can pass
/// sub-second waits without monkey-patching the worker. The boot-time builder
/// in `ork-api/src/main.rs` does the conversion.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Wait between attempts. After the final retry the payload is written
    /// to `a2a_push_dead_letter`. ADR-0009 default: `[1m, 5m, 30m]`.
    pub retry_intervals: Vec<Duration>,
    /// Per-attempt HTTP timeout.
    pub request_timeout_secs: u64,
    /// Maximum concurrent in-flight POSTs.
    pub max_concurrency: usize,
    /// `User-Agent` header sent with every POST.
    pub user_agent: String,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            retry_intervals: vec![
                Duration::from_secs(60),
                Duration::from_secs(5 * 60),
                Duration::from_secs(30 * 60),
            ],
            request_timeout_secs: 10,
            max_concurrency: 32,
            user_agent: format!("ork-push/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl WorkerConfig {
    /// Convenience builder used by the API boot path: convert the wire-level
    /// `[push]` minutes vector to in-memory `Duration`s.
    #[must_use]
    pub fn from_minutes(minutes: &[u64]) -> Vec<Duration> {
        minutes
            .iter()
            .map(|m| Duration::from_secs(m * 60))
            .collect()
    }
}

/// Wire shape of the JSON body POSTed to subscribers. Pinned here because the
/// envelope crosses the A2A trust boundary and ADR-0009 calls it out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushNotification {
    pub task_id: String,
    pub tenant_id: String,
    pub state: String,
    pub occurred_at: chrono::DateTime<Utc>,
}

pub struct PushDeliveryWorker {
    eventing: EventingClient,
    namespace: String,
    push_repo: Arc<dyn A2aPushConfigRepository>,
    dead_letter_repo: Arc<dyn A2aPushDeadLetterRepository>,
    jwks: Arc<JwksProvider>,
    cfg: WorkerConfig,
    http: Client,
}

impl PushDeliveryWorker {
    /// Build the worker. The `reqwest::Client` is constructed once so the
    /// connection pool and DNS cache are shared across deliveries.
    #[must_use]
    pub fn new(
        eventing: EventingClient,
        namespace: String,
        push_repo: Arc<dyn A2aPushConfigRepository>,
        dead_letter_repo: Arc<dyn A2aPushDeadLetterRepository>,
        jwks: Arc<JwksProvider>,
        cfg: WorkerConfig,
    ) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(cfg.request_timeout_secs))
            .user_agent(cfg.user_agent.clone())
            .build()
            .expect("reqwest::Client builder must not fail with default settings");
        Self {
            eventing,
            namespace,
            push_repo,
            dead_letter_repo,
            jwks,
            cfg,
            http,
        }
    }

    /// Run until `cancel` fires. Surfaces fatal Kafka errors via `Err`.
    pub async fn run(self, cancel: CancellationToken) -> Result<()> {
        let topic = topics::push_outbox(&self.namespace);
        let mut stream = self
            .eventing
            .consumer
            .subscribe(&topic)
            .await
            .map_err(|e| anyhow::anyhow!("subscribe push outbox: {e}"))?;
        let semaphore = Arc::new(Semaphore::new(self.cfg.max_concurrency.max(1)));
        let me = Arc::new(self);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    tracing::info!("ADR-0009: push delivery worker cancelled");
                    return Ok(());
                }
                next = stream.next() => {
                    let Some(msg) = next else {
                        tracing::warn!("ADR-0009: push outbox stream ended");
                        return Ok(());
                    };
                    let msg = match msg {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(error = %e, "ADR-0009: outbox subscription error");
                            continue;
                        }
                    };
                    let envelope: PushOutboxEnvelope = match serde_json::from_slice(&msg.payload) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!(error = %e, "ADR-0009: drop malformed outbox envelope");
                            continue;
                        }
                    };
                    let permit = match semaphore.clone().acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => return Ok(()),
                    };
                    let me = me.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        me.handle_envelope(envelope, cancel).await;
                    });
                }
            }
        }
    }

    /// Fan an envelope out to every subscriber and run the per-subscriber
    /// retry loop. Each subscriber is independent — one failure does not
    /// affect the others.
    async fn handle_envelope(&self, envelope: PushOutboxEnvelope, cancel: CancellationToken) {
        let configs = match self
            .push_repo
            .list_for_task(envelope.tenant_id, envelope.task_id)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, task_id = %envelope.task_id, "ADR-0009: list_for_task failed");
                return;
            }
        };
        if configs.is_empty() {
            return;
        }
        let body = PushNotification {
            task_id: envelope.task_id.to_string(),
            tenant_id: envelope.tenant_id.to_string(),
            state: envelope.state.clone(),
            occurred_at: envelope.occurred_at,
        };
        let body_bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "ADR-0009: serialise push body failed");
                return;
            }
        };
        let (jws, kid) = match self.jwks.sign_detached(&body_bytes).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "ADR-0009: sign push body failed");
                return;
            }
        };
        for cfg in configs {
            let url = cfg.url.clone();
            let token = cfg.token.clone();
            let attempts = self.cfg.retry_intervals.len() as i32 + 1;
            let mut last_status: Option<i32> = None;
            let mut last_error: Option<String> = None;
            let mut delivered = false;
            for attempt in 0..attempts {
                if cancel.is_cancelled() {
                    return;
                }
                if attempt > 0 {
                    let delay = self.cfg.retry_intervals[(attempt - 1) as usize];
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                match self
                    .post_once(
                        &url,
                        token.as_deref(),
                        &body_bytes,
                        &jws,
                        &kid,
                        &body.occurred_at,
                    )
                    .await
                {
                    Ok(status) if (200..300).contains(&status) => {
                        delivered = true;
                        tracing::debug!(
                            task_id = %envelope.task_id,
                            url = %url,
                            attempt,
                            status,
                            "ADR-0009: push delivered"
                        );
                        break;
                    }
                    Ok(status) => {
                        last_status = Some(i32::from(status));
                        last_error = Some(format!("non-2xx status {status}"));
                        tracing::warn!(
                            task_id = %envelope.task_id,
                            url = %url,
                            attempt,
                            status,
                            "ADR-0009: push attempt rejected"
                        );
                    }
                    Err(e) => {
                        last_error = Some(e.to_string());
                        tracing::warn!(
                            task_id = %envelope.task_id,
                            url = %url,
                            attempt,
                            error = %e,
                            "ADR-0009: push attempt errored"
                        );
                    }
                }
            }
            if !delivered {
                let row = A2aPushDeadLetterRow {
                    id: Uuid::now_v7(),
                    task_id: envelope.task_id,
                    tenant_id: envelope.tenant_id,
                    config_id: Some(cfg.id),
                    url: url.to_string(),
                    last_status,
                    last_error,
                    attempts,
                    payload: serde_json::to_value(&body).unwrap_or(serde_json::Value::Null),
                    failed_at: Utc::now(),
                };
                if let Err(e) = self.dead_letter_repo.insert(&row).await {
                    tracing::warn!(error = %e, "ADR-0009: dead-letter insert failed");
                }
            }
        }
    }

    /// One HTTP attempt. Returns the response status code, or an error if the
    /// request itself failed (DNS, TLS, connect, timeout).
    async fn post_once(
        &self,
        url: &url::Url,
        token: Option<&str>,
        body: &[u8],
        jws: &str,
        kid: &str,
        occurred_at: &chrono::DateTime<Utc>,
    ) -> Result<u16> {
        let mut req = self
            .http
            .post(url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-A2A-Signature", jws)
            .header("X-A2A-Key-Id", kid)
            .header("X-A2A-Timestamp", occurred_at.to_rfc3339())
            .body(body.to_vec());
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        Ok(resp.status().as_u16())
    }
}
