//! Pure-Rust Kafka backend built on [`rskafka`](https://docs.rs/rskafka).
//!
//! Implements [`Producer`] and [`Consumer`] against a real Kafka cluster without pulling in
//! `librdkafka` / C dependencies — see ADR-0004 amendment in
//! [`docs/adrs/0004-hybrid-kong-kafka-transport.md`](../../../docs/adrs/0004-hybrid-kong-kafka-transport.md).
//!
//! ## Phase-1 limitations
//!
//! - **Single partition only.** [`Self::publish`] and [`Self::subscribe`] both target
//!   partition `0`. Multi-partition fan-out is a follow-up; the trait surface stays the
//!   same when we add it. This matches the expected topic shape during Phase-1 (per-task
//!   topics like `agent.status.<task_id>` are inherently single-partition).
//! - **`StartOffset::Latest`.** Subscribers only receive records published after the
//!   subscription is established. ADR-0008's three-tier replay (Redis cache + Postgres) is
//!   layered on top of this for late SSE clients.
//! - **No compression.** Records are produced with [`Compression::NoCompression`]; the
//!   `rskafka` compression crate features are disabled in [`Cargo.toml`] to keep the build
//!   pure-Rust (no `zstd-sys` C builds).
//!
//! ## Security (ADR-0020 §`Kafka trust`)
//!
//! [`Self::connect`] honours `KafkaTransport` + `KafkaAuth` from
//! [`ork_common::config::KafkaConfig`]:
//!
//! - `Plaintext` is allowed only when `env == "dev"`. Any other deployment value
//!   (`staging`, `production`, ...) hard-errors at connect time with a
//!   [`EventingError::Config`] referencing this ADR. The check fires inside the backend
//!   so CLI tools and tests share the same guardrail as `ork-api`.
//! - `Tls { ca_path?, client_cert_path?, client_key_path? }` builds a `rustls::ClientConfig`
//!   on system roots augmented with an optional operator CA bundle, plus an optional
//!   client cert / key for mTLS to the brokers.
//! - SASL: `Scram { username, password_env, mechanism }` resolves `password_env` at
//!   connect time and feeds `rskafka::client::SaslConfig::ScramSha{256,512}`.
//!   `Oauthbearer { token_env }` resolves the bearer token similarly and wires
//!   `rskafka::client::SaslConfig::Oauthbearer` with a one-shot callback (rotation is
//!   the operator's responsibility today; a follow-up ADR can layer KMS-driven refresh
//!   on top of this hook).
//!
//! Live integration tests live in [`crates/ork-eventing/tests/rskafka_roundtrip.rs`] behind
//! `#[ignore]` and a `RSKAFKA_BROKERS` env var, so CI does not require a broker.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::FutureExt;
use futures::stream::StreamExt;
use ork_common::config::{KafkaAuth, KafkaConfig, KafkaTransport, ScramMechanism};
use rskafka::client::consumer::{StartOffset, StreamConsumerBuilder};
use rskafka::client::partition::{Compression, UnknownTopicHandling};
use rskafka::client::{Client, ClientBuilder};
use rskafka::client::{Credentials, OauthBearerCredentials, SaslConfig};
use rskafka::record::Record;

use crate::consumer::{ConsumedMessage, Consumer, MessageStream};
use crate::error::EventingError;
use crate::producer::Producer;

/// Pure-Rust Kafka backend. Cheap to clone (the inner [`Client`] is `Arc`-shared).
#[derive(Clone)]
pub struct RsKafkaBackend {
    client: Arc<Client>,
}

impl RsKafkaBackend {
    /// Connect to the given bootstrap brokers, applying the ADR-0020 transport
    /// + auth posture from [`KafkaConfig`].
    ///
    /// `env` is the runtime deployment selector
    /// ([`ork_common::config::AppConfig::env`]) — when it is anything other
    /// than `"dev"` the function refuses to dial a `Plaintext` transport.
    pub async fn connect(cfg: &KafkaConfig, env: &str) -> Result<Self, EventingError> {
        if cfg.brokers.is_empty() {
            return Err(EventingError::Config(
                "RsKafkaBackend requires at least one broker".into(),
            ));
        }

        // ADR-0020 §`Kafka trust`: hard error when running outside dev with
        // PLAINTEXT — the backend is the right home for the guardrail because
        // the same backend is shared by `ork-api`, the CLI, and the
        // integration tests.
        if env != "dev" && matches!(cfg.transport, KafkaTransport::Plaintext) {
            return Err(EventingError::Config(format!(
                "PLAINTEXT Kafka transport is forbidden when ORK__ENV={env:?} (ADR-0020 §`Kafka trust`); set [kafka.transport] kind = \"tls\" or run with ORK__ENV=dev"
            )));
        }

        if cfg.security_protocol.is_some() || cfg.sasl_mechanism.is_some() {
            static LEGACY_KEY_WARNING: OnceLock<()> = OnceLock::new();
            LEGACY_KEY_WARNING.get_or_init(|| {
                tracing::warn!(
                    "[kafka] security_protocol / sasl_mechanism are pre-ADR-0020 keys and no longer drive runtime behaviour; configure [kafka.transport] and [kafka.auth] instead"
                );
            });
        }

        let mut builder = ClientBuilder::new(cfg.brokers.clone());

        if let KafkaTransport::Tls {
            ca_path,
            client_cert_path,
            client_key_path,
        } = &cfg.transport
        {
            let tls = build_tls_client_config(
                ca_path.as_deref(),
                client_cert_path.as_deref(),
                client_key_path.as_deref(),
            )?;
            builder = builder.tls_config(Arc::new(tls));
        }

        if let Some(sasl) = build_sasl_config(&cfg.auth)? {
            builder = builder.sasl_config(sasl);
        }

        let client = builder
            .build()
            .await
            .map_err(|e| EventingError::Backend(format!("rskafka build: {e}")))?;

        Ok(Self {
            client: Arc::new(client),
        })
    }

    fn header_vec_to_btreemap(
        headers: &[(String, Vec<u8>)],
    ) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut map = std::collections::BTreeMap::new();
        for (k, v) in headers {
            map.insert(k.clone(), v.clone());
        }
        map
    }
}

fn build_tls_client_config(
    ca_path: Option<&std::path::Path>,
    client_cert_path: Option<&std::path::Path>,
    client_key_path: Option<&std::path::Path>,
) -> Result<rustls::ClientConfig, EventingError> {
    use std::fs::File;
    use std::io::BufReader;

    let mut roots = rustls::RootCertStore::empty();
    // System roots: rustls 0.23 ships them only behind a feature flag; we
    // intentionally skip platform-roots and rely on the operator-provided CA
    // bundle. If unset, we still load webpki-style empty roots; the broker
    // handshake will fail with a clear "unknown issuer" error rather than
    // silently trust everything.
    if let Some(path) = ca_path {
        let mut reader = BufReader::new(File::open(path).map_err(|e| {
            EventingError::Config(format!("kafka.transport.ca_path {path:?}: {e}"))
        })?);
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.map_err(|e| {
                EventingError::Config(format!("kafka.transport.ca_path {path:?}: {e}"))
            })?;
            roots.add(cert).map_err(|e| {
                EventingError::Config(format!("kafka.transport.ca_path {path:?}: {e}"))
            })?;
        }
    }

    // rustls 0.23 requires an explicit `CryptoProvider`; we pin `ring` (matches
    // the workspace `rustls = { features = ["ring"] }`). This avoids depending
    // on `install_default()` ordering across the binary.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| EventingError::Config(format!("kafka.transport TLS provider: {e}")))?
        .with_root_certificates(roots);

    let cfg = match (client_cert_path, client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let mut cert_reader = BufReader::new(File::open(cert_path).map_err(|e| {
                EventingError::Config(format!(
                    "kafka.transport.client_cert_path {cert_path:?}: {e}"
                ))
            })?);
            let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
                .collect::<Result<_, _>>()
                .map_err(|e| {
                    EventingError::Config(format!(
                        "kafka.transport.client_cert_path {cert_path:?}: {e}"
                    ))
                })?;

            let mut key_reader = BufReader::new(File::open(key_path).map_err(|e| {
                EventingError::Config(format!("kafka.transport.client_key_path {key_path:?}: {e}"))
            })?);
            let key = rustls_pemfile::private_key(&mut key_reader)
                .map_err(|e| {
                    EventingError::Config(format!(
                        "kafka.transport.client_key_path {key_path:?}: {e}"
                    ))
                })?
                .ok_or_else(|| {
                    EventingError::Config(format!(
                        "kafka.transport.client_key_path {key_path:?}: no private key found"
                    ))
                })?;

            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| EventingError::Config(format!("kafka.transport client_auth: {e}")))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(EventingError::Config(
                "kafka.transport: client_cert_path and client_key_path must be set together".into(),
            ));
        }
    };

    Ok(cfg)
}

fn build_sasl_config(auth: &KafkaAuth) -> Result<Option<SaslConfig>, EventingError> {
    match auth {
        KafkaAuth::None => Ok(None),
        KafkaAuth::Scram {
            username,
            password_env,
            mechanism,
        } => {
            let password = std::env::var(password_env).map_err(|_| {
                EventingError::Config(format!(
                    "kafka.auth.scram.password_env: ${password_env} is not set"
                ))
            })?;
            let creds = Credentials::new(username.clone(), password);
            Ok(Some(match mechanism {
                ScramMechanism::Sha256 => SaslConfig::ScramSha256(creds),
                ScramMechanism::Sha512 => SaslConfig::ScramSha512(creds),
            }))
        }
        KafkaAuth::Oauthbearer { token_env } => {
            // Validate at connect time so a missing env var fails loudly at
            // boot rather than on the first reconnect.
            let _ = std::env::var(token_env).map_err(|_| {
                EventingError::Config(format!(
                    "kafka.auth.oauthbearer.token_env: ${token_env} is not set"
                ))
            })?;
            // rskafka's OAUTHBEARER callback is async + invoked on every
            // (re)authenticate. Re-resolve the env var inside the callback
            // so an operator who rotates the token (file-watcher / sidecar
            // / `kill -HUP`) is observed without an ork-api restart.
            // KMS-driven refresh is a follow-up ADR; this minimal hook
            // already lets a rotation pipeline land without code changes.
            let token_env = Arc::new(token_env.clone());
            let callback: rskafka::client::OauthCallback = Arc::new(move || {
                let env_name = token_env.clone();
                async move {
                    std::env::var(env_name.as_str()).map_err(|e| {
                        Box::new(std::io::Error::other(format!(
                            "kafka oauth token_env ${env_name}: {e}"
                        ))) as Box<dyn std::error::Error + Send + Sync>
                    })
                }
                .boxed()
            });
            Ok(Some(SaslConfig::Oauthbearer(OauthBearerCredentials {
                callback,
                authz_id: None,
                bearer_kvs: Vec::new(),
            })))
        }
    }
}

#[async_trait]
impl Producer for RsKafkaBackend {
    async fn publish(
        &self,
        topic: &str,
        key: Option<&[u8]>,
        headers: &[(String, Vec<u8>)],
        payload: &[u8],
    ) -> Result<(), EventingError> {
        let partition = self
            .client
            .partition_client(topic.to_string(), 0, UnknownTopicHandling::Retry)
            .await
            .map_err(|e| EventingError::Backend(format!("partition_client: {e}")))?;

        let record = Record {
            key: key.map(<[u8]>::to_vec),
            value: Some(payload.to_vec()),
            headers: Self::header_vec_to_btreemap(headers),
            timestamp: chrono::Utc::now(),
        };

        partition
            .produce(vec![record], Compression::NoCompression)
            .await
            .map_err(|e| EventingError::Backend(format!("produce: {e}")))?;

        Ok(())
    }
}

#[async_trait]
impl Consumer for RsKafkaBackend {
    async fn subscribe(&self, topic: &str) -> Result<MessageStream, EventingError> {
        let partition = self
            .client
            .partition_client(topic.to_string(), 0, UnknownTopicHandling::Retry)
            .await
            .map_err(|e| EventingError::Backend(format!("partition_client: {e}")))?;
        let partition = Arc::new(partition);

        let stream = StreamConsumerBuilder::new(partition, StartOffset::Latest)
            .with_max_wait_ms(500)
            .build();

        let mapped = stream.map(|item| match item {
            Ok((record_and_offset, _high_water)) => {
                let record = record_and_offset.record;
                Ok(ConsumedMessage {
                    key: record.key,
                    headers: record.headers.into_iter().collect(),
                    payload: record.value.unwrap_or_default(),
                })
            }
            Err(e) => Err(EventingError::Backend(format!("rskafka stream: {e}"))),
        });

        Ok(Box::pin(mapped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_cfg(brokers: Vec<String>) -> KafkaConfig {
        KafkaConfig {
            brokers,
            ..KafkaConfig::default()
        }
    }

    #[tokio::test]
    async fn empty_brokers_returns_config_error() {
        match RsKafkaBackend::connect(&dev_cfg(vec![]), "dev").await {
            Ok(_) => panic!("expected config error, got Ok"),
            Err(EventingError::Config(_)) => {}
            Err(other) => panic!("expected EventingError::Config, got {other:?}"),
        }
    }

    /// ADR-0020 §`Kafka trust`: PLAINTEXT outside `dev` is a hard error
    /// surfaced from the backend itself so CLI / test harnesses share the
    /// guardrail with `ork-api`. The error message references the ADR so
    /// operators bisecting a failed boot land on the right doc.
    #[tokio::test]
    async fn plaintext_outside_dev_is_hard_error() {
        let cfg = dev_cfg(vec!["broker:9092".into()]);
        let res = RsKafkaBackend::connect(&cfg, "production").await;
        match res {
            Err(EventingError::Config(msg)) => {
                assert!(msg.contains("PLAINTEXT"), "msg = {msg}");
                assert!(msg.contains("ADR-0020"), "msg = {msg}");
            }
            Err(other) => panic!("expected EventingError::Config, got {other:?}"),
            Ok(_) => panic!("expected EventingError::Config, got Ok(...)"),
        }
    }

    /// SCRAM credentials are resolved from env at connect time; missing env
    /// is a config error referencing the env var name (operator can fix it
    /// from the message alone).
    #[test]
    fn sasl_scram_missing_env_is_config_error() {
        // SAFETY: tests run single-threaded with `current_thread`-style
        // semantics for env-var manipulation; the var name is unique to this test.
        unsafe {
            std::env::remove_var("ORK_TEST_SCRAM_MISSING");
        }
        let auth = KafkaAuth::Scram {
            username: "u".into(),
            password_env: "ORK_TEST_SCRAM_MISSING".into(),
            mechanism: ScramMechanism::Sha512,
        };
        match build_sasl_config(&auth) {
            Err(EventingError::Config(msg)) => {
                assert!(msg.contains("ORK_TEST_SCRAM_MISSING"), "msg = {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn sasl_oauthbearer_missing_env_is_config_error() {
        // SAFETY: see above.
        unsafe {
            std::env::remove_var("ORK_TEST_OAUTH_MISSING");
        }
        let auth = KafkaAuth::Oauthbearer {
            token_env: "ORK_TEST_OAUTH_MISSING".into(),
        };
        match build_sasl_config(&auth) {
            Err(EventingError::Config(msg)) => {
                assert!(msg.contains("ORK_TEST_OAUTH_MISSING"), "msg = {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn sasl_scram_resolves_password_from_env() {
        // SAFETY: see above.
        unsafe {
            std::env::set_var("ORK_TEST_SCRAM_OK", "hunter2");
        }
        let auth = KafkaAuth::Scram {
            username: "u".into(),
            password_env: "ORK_TEST_SCRAM_OK".into(),
            mechanism: ScramMechanism::Sha256,
        };
        match build_sasl_config(&auth).expect("ok") {
            Some(SaslConfig::ScramSha256(creds)) => {
                assert_eq!(creds.username, "u");
                assert_eq!(creds.password, "hunter2");
            }
            other => panic!("expected ScramSha256, got {other:?}"),
        }
        unsafe {
            std::env::remove_var("ORK_TEST_SCRAM_OK");
        }
    }

    #[test]
    fn sasl_none_yields_none() {
        assert!(build_sasl_config(&KafkaAuth::None).expect("ok").is_none());
    }

    #[test]
    fn tls_client_cert_without_key_is_config_error() {
        let cert = std::path::Path::new("/tmp/no-such-cert.pem");
        let res = build_tls_client_config(None, Some(cert), None);
        match res {
            Err(EventingError::Config(msg)) => {
                assert!(msg.contains("client_cert_path"), "msg = {msg}");
                assert!(msg.contains("client_key_path"), "msg = {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    /// Symmetric to the above: pinning the inverse `(None, Some(key))`
    /// arm so a future split into separate match arms can't regress
    /// asymmetrically.
    #[test]
    fn tls_client_key_without_cert_is_config_error() {
        let key = std::path::Path::new("/tmp/no-such-key.pem");
        let res = build_tls_client_config(None, None, Some(key));
        match res {
            Err(EventingError::Config(msg)) => {
                assert!(msg.contains("client_cert_path"), "msg = {msg}");
                assert!(msg.contains("client_key_path"), "msg = {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}
