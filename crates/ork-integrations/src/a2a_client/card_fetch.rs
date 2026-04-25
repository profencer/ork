//! Cached fetcher for [`/.well-known/agent-card.json`] (ADR-0007 §`Card fetch`).
//!
//! - Cache hits (entry younger than [`A2aClientConfig::card_refresh_interval`])
//!   skip the HTTP round-trip.
//! - 404s map to [`OrkError::NotFound`] (per the ADR failure table).
//! - Any other transport-level error maps to [`OrkError::A2aClient`] so the
//!   caller can fail construction loudly.

use std::sync::Arc;
use std::time::Duration;

use ork_a2a::AgentCard;
use ork_cache::KeyValueCache;
use ork_common::error::OrkError;
use reqwest::StatusCode;
use secrecy::SecretString;
use url::Url;

use super::auth::{A2aAuth, apply_auth};

/// Cache-aware fetcher. One instance per `(base_url, auth, cache)` triple is fine
/// (it's effectively a thin function wrapper); typically callers construct a fresh
/// one inline from [`super::A2aRemoteAgentBuilder`].
pub struct CardFetcher {
    http: reqwest::Client,
    cache: Arc<dyn KeyValueCache>,
    ttl: Duration,
}

impl CardFetcher {
    #[must_use]
    pub fn new(http: reqwest::Client, cache: Arc<dyn KeyValueCache>, ttl: Duration) -> Self {
        Self { http, cache, ttl }
    }

    /// Cache key for `base_url`. Stable across processes so a warm Redis is reusable.
    fn cache_key(base_url: &Url) -> String {
        format!("ork:a2a:card:{}", base_url.as_str().trim_end_matches('/'))
    }

    /// Build the canonical card URL (`<base>/.well-known/agent-card.json`).
    pub fn card_url(base_url: &Url) -> Result<Url, OrkError> {
        let mut url = base_url.clone();
        if !url.path().ends_with('/') {
            let p = format!("{}/", url.path());
            url.set_path(&p);
        }
        url.join(".well-known/agent-card.json")
            .map_err(|e| OrkError::Validation(format!("invalid base_url '{base_url}': {e}")))
    }

    /// Fetch (or return cached) card. The cache is checked first; on miss we GET
    /// the canonical card URL and re-populate the cache with the resulting JSON.
    pub async fn fetch(
        &self,
        base_url: &Url,
        auth: &A2aAuth,
        resolved_bearer: Option<&SecretString>,
    ) -> Result<AgentCard, OrkError> {
        let key = Self::cache_key(base_url);
        if let Some(bytes) = self.cache.get(&key).await? {
            if let Ok(card) = serde_json::from_slice::<AgentCard>(&bytes) {
                tracing::trace!(base_url = %base_url, "card cache hit");
                return Ok(card);
            }
            // Corrupted cache entry — fall through to the network and overwrite.
            tracing::warn!(base_url = %base_url, "card cache corrupt; refetching");
        }

        let url = Self::card_url(base_url)?;
        let req = apply_auth(self.http.get(url.clone()), auth, resolved_bearer);
        let resp = req.send().await.map_err(|e| {
            OrkError::A2aClient(
                e.status().map(|s| s.as_u16() as i32).unwrap_or(0),
                format!("card GET {url}: {e}"),
            )
        })?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(OrkError::NotFound(format!("no agent card at {url} (404)")));
        }
        if !status.is_success() {
            return Err(OrkError::A2aClient(
                status.as_u16() as i32,
                format!("card GET {url} returned {status}"),
            ));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| OrkError::A2aClient(502, format!("card GET {url} body read: {e}")))?;
        let card: AgentCard = serde_json::from_slice(&bytes).map_err(|e| {
            OrkError::A2aClient(502, format!("card GET {url} returned malformed JSON: {e}"))
        })?;

        self.cache
            .set_with_ttl(&key, &bytes, self.ttl)
            .await
            .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to populate card cache"));
        Ok(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::{AgentCapabilities, AgentSkill};
    use ork_cache::InMemoryCache;

    fn fake_card() -> AgentCard {
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
            extensions: None,
        }
    }

    #[test]
    fn card_url_appends_well_known_path() {
        let base: Url = "https://api.example.com/a2a/agents/vendor".parse().unwrap();
        let card_url = CardFetcher::card_url(&base).unwrap();
        assert_eq!(
            card_url.as_str(),
            "https://api.example.com/a2a/agents/vendor/.well-known/agent-card.json"
        );
    }

    #[test]
    fn cache_key_is_deterministic() {
        let url: Url = "https://api.example.com/agents/vendor/".parse().unwrap();
        let url_no_slash: Url = "https://api.example.com/agents/vendor".parse().unwrap();
        assert_eq!(
            CardFetcher::cache_key(&url),
            CardFetcher::cache_key(&url_no_slash)
        );
    }

    #[tokio::test]
    async fn cache_hit_returns_card_without_http() {
        let cache = Arc::new(InMemoryCache::new());
        let card = fake_card();
        let bytes = serde_json::to_vec(&card).unwrap();
        let base: Url = "https://api.example.com/a2a/agents/vendor".parse().unwrap();
        cache
            .set_with_ttl(
                &CardFetcher::cache_key(&base),
                &bytes,
                Duration::from_secs(60),
            )
            .await
            .unwrap();

        // Use a client with an unreachable resolver for the host — if we accidentally
        // reach the network the test will fail with a connection error.
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let fetcher = CardFetcher::new(http, cache, Duration::from_secs(60));
        let got = fetcher.fetch(&base, &A2aAuth::None, None).await.unwrap();
        assert_eq!(got.name, "vendor");
    }
}
