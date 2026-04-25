//! Per-server auth resolution for HTTP-flavoured MCP transports.
//!
//! The OAuth2 client-credentials flow mirrors the cache implemented in
//! [`ork-integrations::a2a_client::auth`](
//! ../../ork-integrations/src/a2a_client/auth.rs) introduced by ADR 0007. We
//! deliberately do **not** depend on `ork-integrations` from here because the
//! dep graph runs the other way: `ork-integrations`' `CompositeToolExecutor`
//! routes `mcp:` tools into `ork-mcp` (see ADR 0010 §`Composite routing`),
//! so any `ork-mcp -> ork-integrations` arrow would close a cycle.
//!
//! TODO(ADR-0020 follow-up): move both implementations into a shared
//! `ork-common::http_auth` module once we have more than two consumers; the
//! cache is small enough that the duplication is cheaper than a workspace
//! refactor today.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ork_common::error::OrkError;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::McpAuthConfig;

/// Re-fetch a CC token this many seconds before its IdP-stated expiry so
/// requests don't race against rotation.
pub const CC_REFRESH_BUFFER: Duration = Duration::from_secs(60);

/// Process-wide cache of `client_credentials` tokens. Keyed by
/// `(token_url, client_id, scopes)`; values include the IdP-stated expiry
/// instant.
#[derive(Default)]
pub struct CcTokenCache {
    inner: Mutex<HashMap<String, CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    value: SecretString,
    expires_at: Instant,
}

impl CcTokenCache {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn key(token_url: &url::Url, client_id: &str, scopes: &[String]) -> String {
        let mut sorted = scopes.to_vec();
        sorted.sort();
        format!("{}|{}|{}", token_url, client_id, sorted.join(" "))
    }

    /// Returns a cached token if it has more than [`CC_REFRESH_BUFFER`] left.
    pub async fn get_fresh(
        &self,
        token_url: &url::Url,
        client_id: &str,
        scopes: &[String],
    ) -> Option<SecretString> {
        let key = Self::key(token_url, client_id, scopes);
        let guard = self.inner.lock().await;
        guard.get(&key).and_then(|t| {
            let remaining = t.expires_at.saturating_duration_since(Instant::now());
            if remaining > CC_REFRESH_BUFFER {
                Some(t.value.clone())
            } else {
                None
            }
        })
    }

    pub async fn store(
        &self,
        token_url: &url::Url,
        client_id: &str,
        scopes: &[String],
        token: SecretString,
        ttl: Duration,
    ) {
        let key = Self::key(token_url, client_id, scopes);
        let mut guard = self.inner.lock().await;
        guard.insert(
            key,
            CachedToken {
                value: token,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Fetch (and cache) a fresh OAuth2 `client_credentials` access token.
pub async fn fetch_client_credentials_token(
    http: &reqwest::Client,
    token_url: &url::Url,
    client_id: &str,
    client_secret: &SecretString,
    scopes: &[String],
    cache: &CcTokenCache,
) -> Result<SecretString, OrkError> {
    if let Some(t) = cache.get_fresh(token_url, client_id, scopes).await {
        return Ok(t);
    }

    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "client_credentials".into()),
        ("client_id", client_id.into()),
        ("client_secret", client_secret.expose_secret().to_string()),
    ];
    if !scopes.is_empty() {
        form.push(("scope", scopes.join(" ")));
    }

    let resp = http
        .post(token_url.clone())
        .form(&form)
        .send()
        .await
        .map_err(|e| OrkError::Integration(format!("mcp oauth2 token POST {token_url}: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(OrkError::Integration(format!(
            "mcp oauth2 token endpoint {token_url} returned {status}"
        )));
    }

    let body: TokenEndpointResponse = resp.json().await.map_err(|e| {
        OrkError::Integration(format!(
            "mcp oauth2 token endpoint {token_url} returned malformed JSON: {e}"
        ))
    })?;

    let ttl = body
        .expires_in
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300));
    let secret = SecretString::from(body.access_token);
    cache
        .store(token_url, client_id, scopes, secret.clone(), ttl)
        .await;
    Ok(secret)
}

/// Resolved per-server auth, ready to be attached to a streamable-HTTP
/// transport's `auth_header` / `custom_headers` knobs.
#[derive(Debug)]
pub(crate) struct ResolvedHttpAuth {
    /// Plain Bearer token without the `Bearer ` prefix; passed to
    /// `StreamableHttpClientTransportConfig::auth_header`.
    pub bearer: Option<SecretString>,
    /// `(header_name, header_value)` pairs for vendor `ApiKey`-style auth.
    pub custom: Vec<(String, SecretString)>,
}

impl ResolvedHttpAuth {
    pub(crate) fn empty() -> Self {
        Self {
            bearer: None,
            custom: Vec::new(),
        }
    }
}

/// Resolve env-var indirections in [`McpAuthConfig`] into concrete header
/// values. For [`McpAuthConfig::Oauth2ClientCredentials`] this also drives
/// the IdP round-trip via [`fetch_client_credentials_token`].
pub(crate) async fn resolve_http_auth(
    auth: &McpAuthConfig,
    http: &reqwest::Client,
    cc_cache: &CcTokenCache,
) -> Result<ResolvedHttpAuth, OrkError> {
    match auth {
        McpAuthConfig::None => Ok(ResolvedHttpAuth::empty()),

        McpAuthConfig::Bearer { value_env } => {
            let value = std::env::var(value_env).map_err(|e| {
                OrkError::Integration(format!(
                    "mcp auth: bearer env var `{value_env}` not set: {e}"
                ))
            })?;
            Ok(ResolvedHttpAuth {
                bearer: Some(SecretString::from(value)),
                custom: Vec::new(),
            })
        }

        McpAuthConfig::ApiKey { header, value_env } => {
            let value = std::env::var(value_env).map_err(|e| {
                OrkError::Integration(format!(
                    "mcp auth: api_key env var `{value_env}` not set: {e}"
                ))
            })?;
            Ok(ResolvedHttpAuth {
                bearer: None,
                custom: vec![(header.clone(), SecretString::from(value))],
            })
        }

        McpAuthConfig::Oauth2ClientCredentials {
            token_url,
            client_id_env,
            client_secret_env,
            scopes,
        } => {
            let client_id = std::env::var(client_id_env).map_err(|e| {
                OrkError::Integration(format!(
                    "mcp auth: oauth2 client_id env var `{client_id_env}` not set: {e}"
                ))
            })?;
            let secret_raw = std::env::var(client_secret_env).map_err(|e| {
                OrkError::Integration(format!(
                    "mcp auth: oauth2 client_secret env var `{client_secret_env}` not set: {e}"
                ))
            })?;
            let secret = SecretString::from(secret_raw);
            let token = fetch_client_credentials_token(
                http, token_url, &client_id, &secret, scopes, cc_cache,
            )
            .await?;
            Ok(ResolvedHttpAuth {
                bearer: Some(token),
                custom: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Serialise env-mutating tests inside this module so they don't race
    /// each other (Rust doesn't sandbox `std::env`).
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    #[tokio::test]
    async fn resolve_none_returns_empty() {
        let cache = CcTokenCache::new();
        let http = reqwest::Client::new();
        let res = resolve_http_auth(&McpAuthConfig::None, &http, &cache)
            .await
            .unwrap();
        assert!(res.bearer.is_none());
        assert!(res.custom.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_bearer_reads_env() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: env mutation is serialised by ENV_LOCK above.
        unsafe {
            std::env::set_var("ORK_MCP_TEST_BEARER", "secret-123");
        }
        let cache = CcTokenCache::new();
        let http = reqwest::Client::new();
        let cfg = McpAuthConfig::Bearer {
            value_env: "ORK_MCP_TEST_BEARER".into(),
        };
        let res = resolve_http_auth(&cfg, &http, &cache).await.unwrap();
        assert_eq!(res.bearer.as_ref().unwrap().expose_secret(), "secret-123");
        unsafe {
            std::env::remove_var("ORK_MCP_TEST_BEARER");
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_bearer_errors_when_env_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("ORK_MCP_TEST_BEARER_MISSING");
        }
        let cache = CcTokenCache::new();
        let http = reqwest::Client::new();
        let cfg = McpAuthConfig::Bearer {
            value_env: "ORK_MCP_TEST_BEARER_MISSING".into(),
        };
        let err = resolve_http_auth(&cfg, &http, &cache).await.unwrap_err();
        assert!(matches!(err, OrkError::Integration(_)));
    }

    // The env-var serialiser is intentionally a `std::sync::Mutex`
    // (not tokio) because the underlying `std::env::set_var` call is
    // synchronous and global. The lint about holding it across an
    // await is acceptable: each test holds the guard for its full
    // duration, and other parallel tests block on the `lock()` call
    // rather than racing on the global env state.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_api_key_reads_env() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("ORK_MCP_TEST_API_KEY", "abc-key");
        }
        let cache = CcTokenCache::new();
        let http = reqwest::Client::new();
        let cfg = McpAuthConfig::ApiKey {
            header: "X-Custom-Key".into(),
            value_env: "ORK_MCP_TEST_API_KEY".into(),
        };
        let res = resolve_http_auth(&cfg, &http, &cache).await.unwrap();
        assert!(res.bearer.is_none());
        assert_eq!(res.custom.len(), 1);
        let (header, value) = &res.custom[0];
        assert_eq!(header, "X-Custom-Key");
        assert_eq!(value.expose_secret(), "abc-key");
        unsafe {
            std::env::remove_var("ORK_MCP_TEST_API_KEY");
        }
    }

    #[tokio::test]
    async fn cc_cache_returns_within_buffer() {
        let cache = CcTokenCache::default();
        let url = url::Url::parse("https://idp.example/token").unwrap();
        let scopes = vec!["a".to_string()];
        cache
            .store(
                &url,
                "client",
                &scopes,
                SecretString::from("tok"),
                Duration::from_secs(120),
            )
            .await;
        let cached = cache.get_fresh(&url, "client", &scopes).await;
        assert_eq!(cached.unwrap().expose_secret(), "tok");
    }

    #[tokio::test]
    async fn cc_cache_drops_token_inside_refresh_buffer() {
        let cache = CcTokenCache::default();
        let url = url::Url::parse("https://idp.example/token").unwrap();
        let scopes = vec!["a".to_string()];
        cache
            .store(
                &url,
                "client",
                &scopes,
                SecretString::from("tok"),
                Duration::from_secs(30),
            )
            .await;
        assert!(cache.get_fresh(&url, "client", &scopes).await.is_none());
    }
}
