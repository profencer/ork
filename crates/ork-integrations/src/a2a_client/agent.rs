//! [`A2aRemoteAgent`] — the [`Agent`] implementation that talks A2A 1.0 over
//! HTTPS+SSE (and, when the card carries the `transport-hint` extension and the
//! caller requested fire-and-forget, optionally over Kafka).
//!
//! Layout of a single call:
//!
//! 1. [`Self::route`] decides between `Route::Http` and `Route::Kafka(topic)`.
//! 2. For HTTP we build a [`JsonRpcRequest`] over the appropriate
//!    [`MessageSendParams`] / [`TaskIdParams`] envelope.
//! 3. [`Self::post`] applies tenant + traceparent + auth headers and runs the
//!    request through `backon::Retryable` honouring `Retry-After`.
//! 4. For streaming (`message/stream`) we hand the response body to
//!    [`super::sse::parse_a2a_sse`] and forward the stream to the caller; on a
//!    mid-stream disconnect we surface [`OrkError::A2aStreamLost`] AFTER any
//!    events that already arrived.
//! 5. Failure mapping follows the ADR-0007 table 1:1.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use futures::stream::{BoxStream, StreamExt};
use ork_a2a::extensions::{
    EXT_TENANT_REQUIRED, EXT_TRANSPORT_HINT, PARAM_KAFKA_REQUEST_TOPIC, PARAM_TENANT_HEADER,
};
use ork_a2a::{
    A2aMethod, AgentCard, JsonRpcRequest, JsonRpcResponse, MessageSendParams, SendMessageResult,
    Task, TaskCancelParams, TaskEvent as A2aTaskEvent, TaskId,
};
use ork_common::error::OrkError;
use ork_core::a2a::context::{AgentContext, AgentId};
use ork_core::ports::agent::{Agent, AgentEventStream};
use ork_core::ports::delegation_publisher::DelegationPublisher;
use reqwest::StatusCode;
use secrecy::SecretString;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use super::auth::{A2aAuth, TokenProvider, apply_auth, fetch_client_credentials_token};
use super::config::{A2aClientConfig, RetryPolicy};
use super::sse::parse_a2a_sse;

/// Routing decision for a single call. `Kafka` only fires when the card's
/// `transport-hint` extension carries a request topic, the agent has a
/// configured [`DelegationPublisher`], and the caller explicitly opted into
/// fire-and-forget delivery.
///
/// The variants are constructed via [`A2aRemoteAgent::route`], whose only
/// non-test caller today is [`super::super::workflow_inline_card`]-style code
/// in `ork-core::workflow::delegation` (ADR-0006). The helper lives here so
/// the routing rule travels with the agent definition rather than being
/// duplicated in each delegation site.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum Route {
    Http,
    Kafka(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CallStyle {
    /// `message/send` — caller waits for a typed response.
    SyncSend,
    /// `message/stream` — caller wants an SSE stream of [`A2aTaskEvent`].
    Streaming,
    /// `tasks/cancel` — small request, ignores the response payload aside from RPC errors.
    Cancel,
}

/// Remote A2A agent. Cheap to clone (everything inside is `Arc` / value-typed).
pub struct A2aRemoteAgent {
    id: AgentId,
    card: AgentCard,
    base_url: url::Url,
    auth: A2aAuth,
    http: reqwest::Client,
    /// Optional Kafka publisher used by the [`A2aRemoteAgent::route`] decision
    /// for the transport-hint short-circuit. Today only routing tests read it;
    /// once ADR-0006 delegation routes through `Agent::send`, the field will
    /// be exercised in the hot path.
    #[allow(dead_code)]
    kafka: Option<Arc<dyn DelegationPublisher>>,
    retry: RetryPolicy,
    stream_idle_timeout: Duration,
    user_agent: String,
    /// Header name used to carry the tenant id ("X-Tenant-Id" by default; overridden
    /// via the `tenant-required` extension).
    tenant_header: String,
    /// Cached resolution of the AC-flow [`TokenProvider`] (None unless `auth` is
    /// `OAuth2AuthorizationCode`); read in [`A2aRemoteAgent::resolve_bearer`]
    /// via the `auth` enum so the field is intentionally read-only metadata.
    #[allow(dead_code)]
    token_provider: Option<Arc<dyn TokenProvider>>,
    /// Per-agent in-process lock used to serialize CC token refreshes for that
    /// agent (the cache itself is already process-wide).
    _cc_refresh_lock: Mutex<()>,
}

impl A2aRemoteAgent {
    /// Build a new remote agent. The HTTP client is taken as-is so callers can
    /// share a single `reqwest::Client` (connection pool) across many remotes.
    pub fn new(
        id: AgentId,
        card: AgentCard,
        base_url: url::Url,
        auth: A2aAuth,
        http: reqwest::Client,
        cfg: &A2aClientConfig,
        kafka: Option<Arc<dyn DelegationPublisher>>,
    ) -> Self {
        let tenant_header = resolve_tenant_header(&card);
        let token_provider = match &auth {
            A2aAuth::OAuth2AuthorizationCode { token_provider } => Some(token_provider.clone()),
            _ => None,
        };
        Self {
            id,
            card,
            base_url,
            auth,
            http,
            kafka,
            retry: cfg.retry,
            stream_idle_timeout: cfg.stream_idle_timeout,
            user_agent: cfg.user_agent.clone(),
            tenant_header,
            token_provider,
            _cc_refresh_lock: Mutex::new(()),
        }
    }

    /// Decide between HTTP and the Kafka short-circuit. The Kafka path only fires
    /// for fire-and-forget delivery (no body to await) when the card carries
    /// the `transport-hint` extension and the agent has a `DelegationPublisher`
    /// configured.
    #[allow(dead_code)]
    pub(crate) fn route(&self, call: CallStyle, fire_and_forget: bool) -> Route {
        if !fire_and_forget || call != CallStyle::SyncSend {
            return Route::Http;
        }
        if self.kafka.is_none() {
            return Route::Http;
        }
        let Some(topic) = kafka_request_topic(&self.card) else {
            return Route::Http;
        };
        Route::Kafka(topic)
    }

    /// Resolve a fresh bearer for the active OAuth2 variant, or return `None` if
    /// no resolution is needed. Call this once per request, not once per retry.
    async fn resolve_bearer(&self) -> Result<Option<SecretString>, OrkError> {
        match &self.auth {
            A2aAuth::OAuth2ClientCredentials {
                token_url,
                client_id,
                client_secret,
                scopes,
                cache,
            } => {
                let _guard = self._cc_refresh_lock.lock().await;
                let token = fetch_client_credentials_token(
                    &self.http,
                    token_url,
                    client_id,
                    client_secret,
                    scopes,
                    cache,
                )
                .await?;
                Ok(Some(token))
            }
            A2aAuth::OAuth2AuthorizationCode { token_provider } => {
                Ok(Some(token_provider.token().await?))
            }
            _ => Ok(None),
        }
    }

    /// Build the JSON-RPC POST URL — A2A 1.0 puts every method at the agent's base URL.
    fn rpc_url(&self) -> url::Url {
        self.base_url.clone()
    }

    /// Apply tenant header, traceparent, user-agent, and auth header to `req`.
    fn apply_default_headers(
        &self,
        mut req: reqwest::RequestBuilder,
        ctx: &AgentContext,
        bearer: Option<&SecretString>,
    ) -> reqwest::RequestBuilder {
        req = req
            .header("user-agent", &self.user_agent)
            .header(self.tenant_header.as_str(), ctx.tenant_id.to_string());
        if let Some(tp) = ctx.trace_ctx.as_deref() {
            req = req.header("traceparent", tp);
        }
        apply_auth(req, &self.auth, bearer)
    }

    /// POST a JSON-RPC envelope and return the typed result on the first 2xx
    /// response. Honours the configured retry policy for 5xx and connection-
    /// level errors (4xx-not-429 are *not* retried).
    pub(crate) async fn post<P, R>(
        &self,
        ctx: &AgentContext,
        envelope: JsonRpcRequest<P>,
    ) -> Result<R, OrkError>
    where
        P: Serialize + Clone + Send + Sync,
        R: DeserializeOwned,
    {
        let bearer = self.resolve_bearer().await?;
        let url = self.rpc_url();
        let body = serde_json::to_vec(&envelope)
            .map_err(|e| OrkError::Internal(format!("serialize JSON-RPC envelope: {e}")))?;

        let policy = self.retry;
        let url_for_retry = url.clone();
        let attempt_fn = || async {
            let req = self
                .http
                .post(url_for_retry.clone())
                .header("content-type", "application/json")
                .body(body.clone());
            let req = self.apply_default_headers(req, ctx, bearer.as_ref());
            let resp = req.send().await.map_err(|e| RetryableError::Transient {
                msg: format!("transport error to {url_for_retry}: {e}"),
                retry_after: None,
            })?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = parse_retry_after(resp.headers());
                return Err(RetryableError::Transient {
                    msg: format!("{url_for_retry} returned 429"),
                    retry_after,
                });
            }
            if status.is_server_error() {
                return Err(RetryableError::Transient {
                    msg: format!("{url_for_retry} returned {status}"),
                    retry_after: parse_retry_after(resp.headers()),
                });
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(RetryableError::Permanent(OrkError::A2aClient(
                    status.as_u16() as i32,
                    format!("{url_for_retry} returned {status}: {body}"),
                )));
            }

            let bytes = resp.bytes().await.map_err(|e| RetryableError::Transient {
                msg: format!("read body from {url_for_retry}: {e}"),
                retry_after: None,
            })?;

            let envelope: JsonRpcResponse<serde_json::Value> = serde_json::from_slice(&bytes)
                .map_err(|e| {
                    RetryableError::Permanent(OrkError::A2aClient(
                        502,
                        format!("malformed JSON-RPC envelope from {url_for_retry}: {e}"),
                    ))
                })?;
            if let Some(rpc_err) = envelope.error {
                return Err(RetryableError::Permanent(OrkError::A2aClient(
                    rpc_err.code,
                    rpc_err.message,
                )));
            }
            let raw = envelope.result.ok_or_else(|| {
                RetryableError::Permanent(OrkError::A2aClient(
                    502,
                    format!("JSON-RPC response from {url_for_retry} missing result"),
                ))
            })?;
            let typed: R = serde_json::from_value(raw).map_err(|e| {
                RetryableError::Permanent(OrkError::A2aClient(
                    502,
                    format!("decode JSON-RPC result from {url_for_retry}: {e}"),
                ))
            })?;
            Ok::<_, RetryableError>(typed)
        };

        let backoff = ExponentialBuilder::default()
            .with_min_delay(policy.initial_delay)
            .with_max_delay(policy.max_delay)
            .with_max_times((policy.max_attempts.saturating_sub(1)) as usize)
            .with_factor(policy.factor);
        let result = attempt_fn
            .retry(backoff)
            .sleep(tokio::time::sleep)
            .when(|e| matches!(e, RetryableError::Transient { .. }))
            .notify(|err, dur| {
                if let RetryableError::Transient { msg, retry_after } = err {
                    let wait = retry_after.unwrap_or(dur);
                    tracing::warn!(retry_in_ms = wait.as_millis() as u64, %msg, "A2A retrying");
                }
            })
            .await;

        result.map_err(|e| match e {
            RetryableError::Transient { msg, .. } => OrkError::A2aClient(502, msg),
            RetryableError::Permanent(err) => err,
        })
    }

    /// POST `message/stream` and return an SSE-backed event stream. Failure mapping
    /// matches `post()` for the initial response; once we begin streaming, the only
    /// transport-level error path is [`OrkError::A2aStreamLost`].
    pub(crate) async fn post_sse<P>(
        &self,
        ctx: &AgentContext,
        envelope: JsonRpcRequest<P>,
    ) -> Result<BoxStream<'static, Result<A2aTaskEvent, OrkError>>, OrkError>
    where
        P: Serialize + Send + Sync,
    {
        let bearer = self.resolve_bearer().await?;
        let url = self.rpc_url();
        let body = serde_json::to_vec(&envelope)
            .map_err(|e| OrkError::Internal(format!("serialize JSON-RPC envelope: {e}")))?;

        let req = self
            .http
            .post(url.clone())
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .body(body);
        let req = self.apply_default_headers(req, ctx, bearer.as_ref());

        let resp = req
            .send()
            .await
            .map_err(|e| OrkError::A2aClient(502, format!("stream POST {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(OrkError::A2aClient(
                status.as_u16() as i32,
                format!("stream POST {url} returned {status}: {txt}"),
            ));
        }

        let body_stream: Pin<
            Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
        > = Box::pin(resp.bytes_stream());
        let idle = self.stream_idle_timeout;
        let event_stream = parse_a2a_sse(body_stream);
        let with_timeout = stream_with_idle_timeout(event_stream, idle);
        Ok(with_timeout)
    }
}

#[derive(Debug)]
enum RetryableError {
    Transient {
        msg: String,
        retry_after: Option<Duration>,
    },
    Permanent(OrkError),
}

impl std::fmt::Display for RetryableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient { msg, .. } => write!(f, "transient: {msg}"),
            Self::Permanent(err) => write!(f, "permanent: {err}"),
        }
    }
}
impl std::error::Error for RetryableError {}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let val = headers.get(reqwest::header::RETRY_AFTER)?;
    let s = val.to_str().ok()?;
    s.parse::<u64>().ok().map(Duration::from_secs)
}

#[allow(dead_code)]
fn kafka_request_topic(card: &AgentCard) -> Option<String> {
    card.extensions.as_ref()?.iter().find_map(|ext| {
        if ext.uri == EXT_TRANSPORT_HINT {
            ext.params
                .as_ref()?
                .get(PARAM_KAFKA_REQUEST_TOPIC)?
                .as_str()
                .map(str::to_string)
        } else {
            None
        }
    })
}

fn resolve_tenant_header(card: &AgentCard) -> String {
    card.extensions
        .as_ref()
        .and_then(|exts| {
            exts.iter().find_map(|ext| {
                if ext.uri == EXT_TENANT_REQUIRED {
                    ext.params
                        .as_ref()?
                        .get(PARAM_TENANT_HEADER)?
                        .as_str()
                        .map(str::to_string)
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "X-Tenant-Id".to_string())
}

/// Wrap an event stream so a quiet period longer than `idle` produces an
/// `A2aStreamLost` error AFTER any events received so far.
fn stream_with_idle_timeout(
    inner: BoxStream<'static, Result<A2aTaskEvent, OrkError>>,
    idle: Duration,
) -> BoxStream<'static, Result<A2aTaskEvent, OrkError>> {
    use async_stream::stream;
    let stream = stream! {
        let mut s = inner;
        loop {
            match tokio::time::timeout(idle, s.next()).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    yield Err(OrkError::A2aStreamLost(format!(
                        "no SSE data for {}s",
                        idle.as_secs()
                    )));
                    break;
                }
            }
        }
    };
    Box::pin(stream)
}

#[async_trait]
impl Agent for A2aRemoteAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn card(&self) -> &AgentCard {
        &self.card
    }

    async fn send(
        &self,
        ctx: AgentContext,
        msg: ork_core::a2a::AgentMessage,
    ) -> Result<ork_core::a2a::AgentMessage, OrkError> {
        let envelope = JsonRpcRequest::new(
            Some(serde_json::Value::String(ctx.task_id.to_string())),
            A2aMethod::MessageSend,
            Some(MessageSendParams {
                message: msg,
                configuration: None,
                metadata: None,
            }),
        );
        let result: SendMessageResult = self.post(&ctx, envelope).await?;
        match result {
            SendMessageResult::Message(m) => Ok(m),
            SendMessageResult::Task(task) => {
                task.history.into_iter().last().ok_or_else(|| {
                    OrkError::A2aClient(502, "task returned with empty history".into())
                })
            }
        }
    }

    async fn send_stream(
        &self,
        ctx: AgentContext,
        msg: ork_core::a2a::AgentMessage,
    ) -> Result<AgentEventStream, OrkError> {
        let envelope = JsonRpcRequest::new(
            Some(serde_json::Value::String(ctx.task_id.to_string())),
            A2aMethod::MessageStream,
            Some(MessageSendParams {
                message: msg,
                configuration: None,
                metadata: None,
            }),
        );
        let stream = self.post_sse(&ctx, envelope).await?;
        Ok(Box::pin(stream))
    }

    async fn cancel(&self, ctx: AgentContext, task_id: &TaskId) -> Result<(), OrkError> {
        let envelope = JsonRpcRequest::new(
            Some(serde_json::Value::String(task_id.to_string())),
            A2aMethod::TasksCancel,
            Some(TaskCancelParams {
                id: *task_id,
                metadata: None,
            }),
        );
        let _: Task = self.post(&ctx, envelope).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::{AgentCapabilities, AgentExtension, AgentSkill};

    fn card_with(transport_hint_topic: Option<&str>, tenant_header: Option<&str>) -> AgentCard {
        let mut extensions = Vec::new();
        if let Some(t) = transport_hint_topic {
            let mut params = serde_json::Map::new();
            params.insert(
                PARAM_KAFKA_REQUEST_TOPIC.into(),
                serde_json::Value::String(t.into()),
            );
            extensions.push(AgentExtension {
                uri: EXT_TRANSPORT_HINT.into(),
                description: None,
                params: Some(params),
            });
        }
        if let Some(h) = tenant_header {
            let mut params = serde_json::Map::new();
            params.insert(
                PARAM_TENANT_HEADER.into(),
                serde_json::Value::String(h.into()),
            );
            extensions.push(AgentExtension {
                uri: EXT_TENANT_REQUIRED.into(),
                description: None,
                params: Some(params),
            });
        }
        AgentCard {
            name: "vendor".into(),
            description: "test".into(),
            version: "0.1.0".into(),
            url: Some("https://vendor.example.com/a2a".parse().unwrap()),
            provider: None,
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: false,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "default".into(),
                name: "vendor".into(),
                description: "x".into(),
                tags: vec![],
                examples: vec![],
                input_modes: None,
                output_modes: None,
            }],
            security_schemes: None,
            security: None,
            extensions: if extensions.is_empty() {
                None
            } else {
                Some(extensions)
            },
        }
    }

    fn fake_agent(card: AgentCard, kafka: Option<Arc<dyn DelegationPublisher>>) -> A2aRemoteAgent {
        let cfg = A2aClientConfig::default();
        A2aRemoteAgent::new(
            "vendor".into(),
            card,
            "https://vendor.example.com/a2a".parse().unwrap(),
            A2aAuth::None,
            reqwest::Client::new(),
            &cfg,
            kafka,
        )
    }

    #[test]
    fn route_is_kafka_only_for_fire_and_forget_send_with_topic_and_publisher() {
        struct DummyPub;
        #[async_trait::async_trait]
        impl DelegationPublisher for DummyPub {
            async fn publish_request(
                &self,
                _: &AgentId,
                _: TaskId,
                _: &[u8],
            ) -> Result<(), OrkError> {
                Ok(())
            }
            async fn publish_cancel(&self, _: TaskId) -> Result<(), OrkError> {
                Ok(())
            }
        }

        let card = card_with(Some("ork.a2a.v1.agent.request.vendor"), None);
        let kafka: Arc<dyn DelegationPublisher> = Arc::new(DummyPub);
        let a = fake_agent(card.clone(), Some(kafka.clone()));

        // Fire-and-forget sync send → Kafka.
        match a.route(CallStyle::SyncSend, true) {
            Route::Kafka(t) => assert_eq!(t, "ork.a2a.v1.agent.request.vendor"),
            other => panic!("expected Kafka route, got {other:?}"),
        }

        // Same call but caller wants the response → HTTP.
        assert!(matches!(a.route(CallStyle::SyncSend, false), Route::Http));

        // Streaming and cancel → always HTTP.
        assert!(matches!(a.route(CallStyle::Streaming, true), Route::Http));
        assert!(matches!(a.route(CallStyle::Cancel, true), Route::Http));

        // Without a publisher → HTTP even if topic is set.
        let no_kafka = fake_agent(card.clone(), None);
        assert!(matches!(
            no_kafka.route(CallStyle::SyncSend, true),
            Route::Http
        ));

        // Without a topic → HTTP even if publisher is set.
        let no_topic = fake_agent(card_with(None, None), Some(kafka));
        assert!(matches!(
            no_topic.route(CallStyle::SyncSend, true),
            Route::Http
        ));
    }

    #[test]
    fn tenant_header_resolved_from_extension_when_present() {
        let card = card_with(None, Some("X-Vendor-Tenant"));
        let a = fake_agent(card, None);
        assert_eq!(a.tenant_header, "X-Vendor-Tenant");
    }

    #[test]
    fn tenant_header_defaults_when_extension_absent() {
        let card = card_with(None, None);
        let a = fake_agent(card, None);
        assert_eq!(a.tenant_header, "X-Tenant-Id");
    }
}
