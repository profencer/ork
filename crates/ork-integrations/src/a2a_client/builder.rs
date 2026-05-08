//! [`A2aRemoteAgentBuilder`] — the [`RemoteAgentBuilder`] implementation used by the
//! static-config loader (`ork-api`), the discovery subscriber (`ork-eventing`), and
//! the workflow inline-card overlay (`ork-core::workflow::engine`). Centralising
//! construction here keeps `A2aClientConfig`, the auth defaults, and the Redis
//! cache identical across the three registration paths.

use std::sync::Arc;

use async_trait::async_trait;
use ork_a2a::AgentCard;
use ork_cache::KeyValueCache;
use ork_common::config::A2aAuthToml;
use ork_common::error::OrkError;
use ork_core::a2a::context::AgentId;
use ork_core::ports::agent::Agent;
use ork_core::ports::delegation_publisher::DelegationPublisher;
use ork_core::ports::remote_agent_builder::RemoteAgentBuilder;
use ork_security::MeshTokenSigner;
use secrecy::SecretString;
use url::Url;

use super::agent::A2aRemoteAgent;
use super::auth::{A2aAuth, CcTokenCache, fetch_client_credentials_token};
use super::card_fetch::CardFetcher;
use super::config::A2aClientConfig;

/// Default builder: applies a single shared HTTP client + cache + auth +
/// (optional) Kafka publisher + (optional) mesh token signer to every
/// constructed agent. The signer is shared across every outbound
/// `A2aRemoteAgent`, so a single ork instance mints with one issuer/audience.
pub struct A2aRemoteAgentBuilder {
    pub http: reqwest::Client,
    pub cache: Arc<dyn KeyValueCache>,
    pub default_auth: A2aAuth,
    pub default_cfg: A2aClientConfig,
    pub kafka: Option<Arc<dyn DelegationPublisher>>,
    pub mesh_signer: Option<Arc<dyn MeshTokenSigner>>,
}

impl A2aRemoteAgentBuilder {
    pub fn new(
        http: reqwest::Client,
        cache: Arc<dyn KeyValueCache>,
        default_auth: A2aAuth,
        default_cfg: A2aClientConfig,
        kafka: Option<Arc<dyn DelegationPublisher>>,
        mesh_signer: Option<Arc<dyn MeshTokenSigner>>,
    ) -> Self {
        Self {
            http,
            cache,
            default_auth,
            default_cfg,
            kafka,
            mesh_signer,
        }
    }

    /// Build an agent from `card` using the supplied auth (overriding the builder
    /// default). Used by the static-config loader so each `[[remote_agents]]` entry
    /// can carry its own credentials.
    pub fn build_with_auth(
        &self,
        card: AgentCard,
        auth: A2aAuth,
    ) -> Result<Arc<dyn Agent>, OrkError> {
        let id: AgentId = card.name.clone();
        let base_url = card.url.clone().ok_or_else(|| {
            OrkError::Validation(format!(
                "remote agent card '{id}' has no `url`; cannot build A2aRemoteAgent"
            ))
        })?;
        Ok(Arc::new(A2aRemoteAgent::new(
            id,
            card,
            base_url,
            auth,
            self.http.clone(),
            &self.default_cfg,
            self.kafka.clone(),
            self.mesh_signer.clone(),
        )))
    }
}

#[async_trait]
impl RemoteAgentBuilder for A2aRemoteAgentBuilder {
    async fn build(&self, card: AgentCard) -> Result<Arc<dyn Agent>, OrkError> {
        self.build_with_auth(card, self.default_auth.clone())
    }

    async fn build_inline(
        &self,
        card_url: Url,
        auth: Option<A2aAuthToml>,
    ) -> Result<Arc<dyn Agent>, OrkError> {
        let auth = match auth {
            Some(t) => resolve_auth_from_toml(&t)?,
            None => self.default_auth.clone(),
        };
        let base = base_url_from_card_url(&card_url);
        let fetcher = CardFetcher::new(
            self.http.clone(),
            self.cache.clone(),
            self.default_cfg.card_refresh_interval,
        );
        let bearer = resolve_bearer_for_card(&self.http, &auth).await?;
        let card = fetcher.fetch(&base, &auth, bearer.as_ref()).await?;
        self.build_with_auth(card, auth)
    }
}

/// Translate a toml-shaped auth selector into the runtime [`A2aAuth`] enum,
/// resolving every `*_env` indirection against `std::env`.
pub fn resolve_auth_from_toml(toml: &A2aAuthToml) -> Result<A2aAuth, OrkError> {
    fn env(name: &str) -> Result<SecretString, OrkError> {
        std::env::var(name)
            .map(SecretString::from)
            .map_err(|_| OrkError::Validation(format!("env var `{name}` is not set")))
    }
    Ok(match toml {
        A2aAuthToml::None => A2aAuth::None,
        A2aAuthToml::StaticBearer { value_env } => A2aAuth::StaticBearer(env(value_env)?),
        A2aAuthToml::StaticApiKey { header, value_env } => A2aAuth::StaticApiKey {
            header: header.clone(),
            value: env(value_env)?,
        },
        A2aAuthToml::OAuth2ClientCredentials {
            token_url,
            client_id_env,
            client_secret_env,
            scopes,
        } => A2aAuth::OAuth2ClientCredentials {
            token_url: token_url.clone(),
            client_id: std::env::var(client_id_env).map_err(|_| {
                OrkError::Validation(format!("env var `{client_id_env}` is not set"))
            })?,
            client_secret: env(client_secret_env)?,
            scopes: scopes.clone(),
            cache: CcTokenCache::new(),
        },
        A2aAuthToml::Mtls {
            cert_path,
            key_path,
        } => A2aAuth::Mtls {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
        },
    })
}

/// Strip path/query/fragment off a card URL to recover the agent's base URL.
pub(crate) fn base_url_from_card_url(card_url: &Url) -> Url {
    let mut base = card_url.clone();
    base.set_path("/");
    base.set_query(None);
    base.set_fragment(None);
    base
}

async fn resolve_bearer_for_card(
    http: &reqwest::Client,
    auth: &A2aAuth,
) -> Result<Option<SecretString>, OrkError> {
    match auth {
        A2aAuth::OAuth2ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scopes,
            cache,
        } => Ok(Some(
            fetch_client_credentials_token(
                http,
                token_url,
                client_id,
                client_secret,
                scopes,
                cache,
            )
            .await?,
        )),
        A2aAuth::OAuth2AuthorizationCode { token_provider } => {
            Ok(Some(token_provider.token().await?))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::{AgentCapabilities, AgentSkill};
    use ork_cache::InMemoryCache;

    fn vendor_card(url: Option<&str>) -> AgentCard {
        AgentCard {
            name: "vendor".into(),
            description: "test".into(),
            version: "0.1.0".into(),
            url: url.map(|u| u.parse().unwrap()),
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
            extensions: None,
        }
    }

    #[tokio::test]
    async fn build_returns_arc_agent_with_card_id() {
        let builder = A2aRemoteAgentBuilder::new(
            reqwest::Client::new(),
            Arc::new(InMemoryCache::new()) as Arc<dyn KeyValueCache>,
            A2aAuth::None,
            A2aClientConfig::default(),
            None,
            None,
        );
        let card = vendor_card(Some("https://vendor.example.com/a2a"));
        let agent = builder.build(card).await.unwrap();
        assert_eq!(agent.id(), &"vendor".to_string());
    }

    #[tokio::test]
    async fn build_rejects_card_without_url() {
        let builder = A2aRemoteAgentBuilder::new(
            reqwest::Client::new(),
            Arc::new(InMemoryCache::new()) as Arc<dyn KeyValueCache>,
            A2aAuth::None,
            A2aClientConfig::default(),
            None,
            None,
        );
        let card = vendor_card(None);
        let err = builder
            .build(card)
            .await
            .err()
            .expect("missing url must surface as Validation error");
        assert!(matches!(err, OrkError::Validation(_)), "got: {err}");
    }
}
