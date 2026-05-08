//! Boot-time wiring for ADR-0007 `[[remote_agents]]` entries.
//!
//! Builds a single shared `A2aRemoteAgentBuilder` (Redis cache + reqwest client +
//! Kafka publisher), materialises every static entry into a long-lived
//! [`A2aRemoteAgent`], registers it in the [`AgentRegistry`] via
//! `upsert_remote_with_agent`, and spawns a refresh task that re-fetches each
//! card every `card_refresh_interval` so card mutations propagate without a
//! restart.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ork_a2a::AgentCard;
use ork_cache::{InMemoryCache, KeyValueCache, RedisCache};
use ork_common::config::{A2aClientToml, AppConfig, RemoteAgentEntryToml, RetryPolicyToml};
use ork_common::error::OrkError;
use ork_core::a2a::AgentId;
use ork_core::agent_registry::{AgentRegistry, RemoteAgentEntry, TransportHint};
use ork_core::ports::artifact_meta_repo::ArtifactMetaRepo;
use ork_core::ports::artifact_store::ArtifactStore;
use ork_core::ports::delegation_publisher::DelegationPublisher;
use ork_integrations::a2a_client::builder::resolve_auth_from_toml;
use ork_integrations::a2a_client::{
    A2aAuth, A2aClientConfig, A2aRemoteAgentBuilder, CardFetcher, RetryPolicy,
    fetch_client_credentials_token,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// ADR-0016: process-wide `ArtifactStore` + index + public API base for `A2aRemoteAgent` rewrites.
pub type ArtifactA2aWiring = (Arc<dyn ArtifactStore>, Arc<dyn ArtifactMetaRepo>, String);

/// Translate the toml `[a2a_client]` section into the runtime
/// [`A2aClientConfig`]. Pure (no IO) so we can unit-test it.
pub fn a2a_client_config_from_toml(t: &A2aClientToml) -> A2aClientConfig {
    A2aClientConfig {
        request_timeout: t.request_timeout(),
        stream_idle_timeout: t.stream_idle_timeout(),
        retry: retry_from_toml(&t.retry),
        user_agent: t.user_agent.clone(),
        card_refresh_interval: t.card_refresh_interval(),
        ..Default::default()
    }
}

fn retry_from_toml(r: &RetryPolicyToml) -> RetryPolicy {
    RetryPolicy {
        max_attempts: r.max_attempts,
        initial_delay: r.initial_delay(),
        factor: r.factor,
        max_delay: r.max_delay(),
    }
}

/// Resolve `*_env` references against `std::env`. Thin wrapper around
/// [`resolve_auth_from_toml`] so the call sites read naturally.
fn resolve_auth(toml: &ork_common::config::A2aAuthToml) -> Result<A2aAuth, OrkError> {
    resolve_auth_from_toml(toml)
}

/// Build the `KeyValueCache` used by the `CardFetcher`. Falls back to an
/// in-memory cache (with a warning) when Redis is unreachable so dev loops keep
/// working — the production deployment should error loudly here.
pub async fn build_card_cache(redis_url: &str) -> Arc<dyn KeyValueCache> {
    match redis::Client::open(redis_url) {
        Ok(client) => match redis::aio::ConnectionManager::new(client).await {
            Ok(conn) => {
                info!(redis_url, "ADR-0007: A2A card cache backed by Redis");
                return Arc::new(RedisCache::from_connection_manager(conn))
                    as Arc<dyn KeyValueCache>;
            }
            Err(e) => {
                warn!(error = %e, "ADR-0007: Redis unreachable; falling back to in-memory card cache")
            }
        },
        Err(e) => {
            warn!(error = %e, "ADR-0007: invalid redis URL; falling back to in-memory card cache")
        }
    }
    Arc::new(InMemoryCache::new()) as Arc<dyn KeyValueCache>
}

/// Build the shared [`A2aRemoteAgentBuilder`] used by `[[remote_agents]]`, the
/// discovery subscriber (ADR-0005), and workflow inline cards (ADR-0007 §3).
///
/// When `artifacts` is `Some`, ADR-0016 wires outbound `Part::File` (base64) rewrites on
/// every [`A2aRemoteAgent`] built from this builder.
pub fn build_remote_builder(
    http: reqwest::Client,
    cache: Arc<dyn KeyValueCache>,
    cfg: &A2aClientToml,
    kafka: Option<Arc<dyn DelegationPublisher>>,
    // ADR-0016: `Some((store, meta, public_base))` — outbound `Part::File` rewrites for
    // all `A2aRemoteAgent`s from this process-wide builder.
    artifacts: Option<ArtifactA2aWiring>,
    // ADR-0020: `Some(signer)` enables `X-Ork-Mesh-Token` minting on every
    // outbound A2A request. `None` keeps legacy / dev behaviour (bearer-only).
    mesh_signer: Option<Arc<dyn ork_security::MeshTokenSigner>>,
) -> Arc<A2aRemoteAgentBuilder> {
    let mut client_cfg = a2a_client_config_from_toml(cfg);
    if let Some((s, m, b)) = artifacts {
        client_cfg.artifact_store = Some(s);
        client_cfg.artifact_meta = Some(m);
        client_cfg.artifact_public_base = Some(b);
    }
    Arc::new(A2aRemoteAgentBuilder::new(
        http,
        cache,
        A2aAuth::None,
        client_cfg,
        kafka,
        mesh_signer,
    ))
}

/// Resolve all configured `[[remote_agents]]`. Each failure is logged and
/// skipped — one misconfigured vendor must not block the whole server.
pub async fn load_static_remote_agents(
    cfg: &AppConfig,
    builder: &A2aRemoteAgentBuilder,
    http: &reqwest::Client,
    cache: Arc<dyn KeyValueCache>,
    registry: &AgentRegistry,
) {
    if cfg.remote_agents.is_empty() {
        return;
    }
    let client_cfg = a2a_client_config_from_toml(&cfg.a2a_client);
    let ttl = cfg.a2a_client.card_refresh_interval();

    for entry in &cfg.remote_agents {
        match load_one_static_entry(entry, builder, http, &cache, &client_cfg, ttl, registry).await
        {
            Ok(()) => {
                info!(agent = %entry.id, card_url = %entry.card_url, "ADR-0007: registered static remote agent")
            }
            Err(e) => {
                warn!(agent = %entry.id, error = %e, "ADR-0007: failed to load static remote agent; skipping")
            }
        }
    }
}

async fn load_one_static_entry(
    entry: &RemoteAgentEntryToml,
    builder: &A2aRemoteAgentBuilder,
    http: &reqwest::Client,
    cache: &Arc<dyn KeyValueCache>,
    client_cfg: &A2aClientConfig,
    ttl: Duration,
    registry: &AgentRegistry,
) -> Result<(), OrkError> {
    let auth = resolve_auth(&entry.auth)?;
    let card =
        fetch_card_for_entry(http, cache.clone(), &entry.card_url, &auth, client_cfg).await?;
    let agent = builder.build_with_auth(card.clone(), auth)?;

    let id: AgentId = entry.id.clone();
    let registry_entry = RemoteAgentEntry {
        transport_hint: TransportHint::from_card(&card),
        card,
        last_seen: Instant::now(),
        ttl,
        agent: Some(agent.clone()),
    };
    registry
        .upsert_remote_with_agent(id, registry_entry, agent)
        .await;
    Ok(())
}

async fn fetch_card_for_entry(
    http: &reqwest::Client,
    cache: Arc<dyn KeyValueCache>,
    card_url: &url::Url,
    auth: &A2aAuth,
    client_cfg: &A2aClientConfig,
) -> Result<AgentCard, OrkError> {
    let base = base_url_from_card_url(card_url);
    let fetcher = CardFetcher::new(http.clone(), cache, client_cfg.card_refresh_interval);
    let bearer = resolve_bearer_for_card(http, auth).await?;
    fetcher.fetch(&base, auth, bearer.as_ref()).await
}

/// Resolve a bearer token if `auth` requires one for card fetching. Only
/// `OAuth2ClientCredentials` and `OAuth2AuthorizationCode` need a token; all
/// other variants self-attach via `apply_auth`.
async fn resolve_bearer_for_card(
    http: &reqwest::Client,
    auth: &A2aAuth,
) -> Result<Option<secrecy::SecretString>, OrkError> {
    match auth {
        A2aAuth::OAuth2ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scopes,
            cache,
        } => {
            let token = fetch_client_credentials_token(
                http,
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
            let token = token_provider.token().await?;
            Ok(Some(token))
        }
        _ => Ok(None),
    }
}

/// Strip `/.well-known/agent-card.json` (or any path) off the configured URL to
/// recover the agent's base URL. Operators write the full card URL in toml so
/// it matches what they curl; the fetcher needs the base.
pub(crate) fn base_url_from_card_url(card_url: &url::Url) -> url::Url {
    let mut base = card_url.clone();
    base.set_path("/");
    base.set_query(None);
    base.set_fragment(None);
    base
}

/// Spawn the periodic re-fetch loop for every static entry. Each tick refreshes
/// the card; drift triggers a fresh `upsert_remote_with_agent` (rebuilding the
/// `A2aRemoteAgent`). Skips silently if `remote_agents` is empty.
pub fn spawn_card_refresh(
    cfg: &AppConfig,
    builder: Arc<A2aRemoteAgentBuilder>,
    http: reqwest::Client,
    cache: Arc<dyn KeyValueCache>,
    registry: Arc<AgentRegistry>,
    cancel: CancellationToken,
) {
    if cfg.remote_agents.is_empty() {
        return;
    }
    let entries = cfg.remote_agents.clone();
    let client_cfg = a2a_client_config_from_toml(&cfg.a2a_client);
    let ttl = cfg.a2a_client.card_refresh_interval();
    let interval = ttl;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // skip immediate tick — first fetch already happened at boot

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    for entry in &entries {
                        if let Err(e) = refresh_one_entry(entry, &builder, &http, &cache, &client_cfg, ttl, &registry).await {
                            warn!(agent = %entry.id, error = %e, "ADR-0007: card refresh failed");
                        }
                    }
                }
            }
        }
    });
}

async fn refresh_one_entry(
    entry: &RemoteAgentEntryToml,
    builder: &A2aRemoteAgentBuilder,
    http: &reqwest::Client,
    cache: &Arc<dyn KeyValueCache>,
    client_cfg: &A2aClientConfig,
    ttl: Duration,
    registry: &AgentRegistry,
) -> Result<(), OrkError> {
    let auth = resolve_auth(&entry.auth)?;
    let card =
        fetch_card_for_entry(http, cache.clone(), &entry.card_url, &auth, client_cfg).await?;
    let agent = builder.build_with_auth(card.clone(), auth)?;

    let id: AgentId = entry.id.clone();
    let registry_entry = RemoteAgentEntry {
        transport_hint: TransportHint::from_card(&card),
        card,
        last_seen: Instant::now(),
        ttl,
        agent: Some(agent.clone()),
    };
    registry
        .upsert_remote_with_agent(id, registry_entry, agent)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::config::A2aAuthToml;
    use std::path::PathBuf;
    use url::Url;

    #[test]
    fn base_url_strips_well_known_path() {
        let url = Url::parse("https://vendor.example.com/.well-known/agent-card.json").unwrap();
        let base = base_url_from_card_url(&url);
        assert_eq!(base.as_str(), "https://vendor.example.com/");
    }

    #[test]
    fn base_url_strips_query_and_fragment() {
        let url = Url::parse("https://vendor.example.com/v2/agent-card.json?x=1#frag").unwrap();
        let base = base_url_from_card_url(&url);
        assert_eq!(base.as_str(), "https://vendor.example.com/");
    }

    #[test]
    fn resolve_auth_translates_static_bearer() {
        unsafe {
            std::env::set_var("ORK_TEST_BEARER", "tok-123");
        }
        let toml = A2aAuthToml::StaticBearer {
            value_env: "ORK_TEST_BEARER".into(),
        };
        let resolved = resolve_auth(&toml).expect("resolved");
        match resolved {
            A2aAuth::StaticBearer(_) => {}
            other => panic!("expected StaticBearer, got {other:?}"),
        }
        unsafe {
            std::env::remove_var("ORK_TEST_BEARER");
        }
    }

    #[test]
    fn resolve_auth_reports_missing_env_var() {
        let toml = A2aAuthToml::StaticBearer {
            value_env: "ORK_TEST_DOES_NOT_EXIST".into(),
        };
        let err = resolve_auth(&toml).expect_err("missing env var must error");
        assert!(matches!(err, OrkError::Validation(_)), "got: {err}");
    }

    #[test]
    fn resolve_auth_translates_mtls_paths() {
        let toml = A2aAuthToml::Mtls {
            cert_path: PathBuf::from("/etc/ork/cert.pem"),
            key_path: PathBuf::from("/etc/ork/key.pem"),
        };
        match resolve_auth(&toml).unwrap() {
            A2aAuth::Mtls {
                cert_path,
                key_path,
            } => {
                assert_eq!(cert_path, PathBuf::from("/etc/ork/cert.pem"));
                assert_eq!(key_path, PathBuf::from("/etc/ork/key.pem"));
            }
            other => panic!("expected Mtls, got {other:?}"),
        }
    }

    #[test]
    fn a2a_client_config_uses_toml_values() {
        let toml = A2aClientToml {
            request_timeout_secs: 7,
            stream_idle_timeout_secs: 11,
            card_refresh_interval_secs: 13,
            user_agent: "ua/1.0".into(),
            retry: RetryPolicyToml {
                max_attempts: 5,
                initial_delay_ms: 200,
                factor: 1.5,
                max_delay_ms: 9_000,
            },
        };
        let cfg = a2a_client_config_from_toml(&toml);
        assert_eq!(cfg.request_timeout, Duration::from_secs(7));
        assert_eq!(cfg.stream_idle_timeout, Duration::from_secs(11));
        assert_eq!(cfg.card_refresh_interval, Duration::from_secs(13));
        assert_eq!(cfg.user_agent, "ua/1.0");
        assert_eq!(cfg.retry.max_attempts, 5);
        assert_eq!(cfg.retry.initial_delay, Duration::from_millis(200));
        assert!((cfg.retry.factor - 1.5).abs() < f32::EPSILON);
        assert_eq!(cfg.retry.max_delay, Duration::from_millis(9_000));
    }
}
