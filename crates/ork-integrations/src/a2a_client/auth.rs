//! Authentication variants for [`super::A2aRemoteAgent`] (ADR-0007 §`Auth variants`).
//!
//! All five variants from the ADR are first-class. Static credentials are wrapped in
//! [`SecretString`] so leaks via `Debug`/`tracing` are scrubbed; OAuth2 client_credentials
//! tokens are cached process-wide and refreshed 60s before expiry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ork_common::error::OrkError;
use reqwest::RequestBuilder;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Header name used for static API-key auth when the operator doesn't override it.
pub const DEFAULT_API_KEY_HEADER: &str = "X-API-Key";

/// Pull-style token provider for [`A2aAuth::OAuth2AuthorizationCode`]. Plugin code or
/// the API can implement this against any IdP/refresh-token store. The implementation
/// MUST handle its own caching/refresh — `apply_auth` calls `token()` on every request.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> Result<SecretString, OrkError>;
}

/// All five auth variants from ADR-0007. Wired so the static-config loader can pick
/// the static variants and plugin code can plug a custom [`TokenProvider`] for
/// authorization-code flows.
#[derive(Clone)]
pub enum A2aAuth {
    /// No auth header (the remote endpoint is unauthenticated or relies on mTLS at
    /// the proxy).
    None,
    /// `Authorization: Bearer <secret>` static token.
    StaticBearer(SecretString),
    /// Static API key sent in `header` (defaults to `X-API-Key`).
    StaticApiKey { header: String, value: SecretString },
    /// OAuth2 `client_credentials` grant. Tokens are cached per
    /// `(token_url, client_id, scopes)` and refreshed 60s before expiry.
    OAuth2ClientCredentials {
        token_url: url::Url,
        client_id: String,
        client_secret: SecretString,
        scopes: Vec<String>,
        cache: Arc<CcTokenCache>,
    },
    /// OAuth2 `authorization_code` flow with refresh — caller-supplied
    /// [`TokenProvider`] (plugin code).
    OAuth2AuthorizationCode {
        token_provider: Arc<dyn TokenProvider>,
    },
    /// Mutual TLS — paths to the client cert and key. The reqwest client is built
    /// with these once at construction; no per-request work is needed.
    Mtls {
        cert_path: PathBuf,
        key_path: PathBuf,
    },
}

impl std::fmt::Debug for A2aAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "A2aAuth::None"),
            Self::StaticBearer(_) => write!(f, "A2aAuth::StaticBearer(<redacted>)"),
            Self::StaticApiKey { header, .. } => f
                .debug_struct("A2aAuth::StaticApiKey")
                .field("header", header)
                .field("value", &"<redacted>")
                .finish(),
            Self::OAuth2ClientCredentials {
                token_url,
                client_id,
                scopes,
                ..
            } => f
                .debug_struct("A2aAuth::OAuth2ClientCredentials")
                .field("token_url", token_url)
                .field("client_id", client_id)
                .field("scopes", scopes)
                .finish(),
            Self::OAuth2AuthorizationCode { .. } => {
                write!(f, "A2aAuth::OAuth2AuthorizationCode(<provider>)")
            }
            Self::Mtls {
                cert_path,
                key_path,
            } => f
                .debug_struct("A2aAuth::Mtls")
                .field("cert_path", cert_path)
                .field("key_path", key_path)
                .finish(),
        }
    }
}

/// Process-wide cache of `client_credentials` tokens. Keyed by `(token_url, client_id, scopes)`.
/// Refreshes a token when fewer than [`CC_REFRESH_BUFFER`] remain before expiry.
#[derive(Default)]
pub struct CcTokenCache {
    inner: Mutex<HashMap<String, CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    value: SecretString,
    expires_at: Instant,
}

/// Re-fetch a `client_credentials` token this many seconds before the IdP-stated
/// expiry to avoid races between sending the request and the token going stale.
pub const CC_REFRESH_BUFFER: Duration = Duration::from_secs(60);

impl CcTokenCache {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Cache key used for `client_credentials` tokens.
    fn key(token_url: &url::Url, client_id: &str, scopes: &[String]) -> String {
        let mut sorted = scopes.to_vec();
        sorted.sort();
        format!("{}|{}|{}", token_url, client_id, sorted.join(" "))
    }

    /// Return a cached token if it has more than [`CC_REFRESH_BUFFER`] remaining.
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

    /// Insert/overwrite a token returned by the IdP.
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

    /// Test-only: forcibly purge.
    #[cfg(test)]
    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }
}

/// IdP token-endpoint response (RFC 6749 §5.1). We only model the fields we use.
#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Fetch a fresh `client_credentials` token, populate the cache, and return it.
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
        .map_err(|e| {
            OrkError::A2aClient(
                e.status().map(|s| s.as_u16() as i32).unwrap_or(0),
                format!("oauth2 token POST {token_url}: {e}"),
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(OrkError::A2aClient(
            status.as_u16() as i32,
            format!("oauth2 token endpoint {token_url} returned {status}"),
        ));
    }

    let body: TokenEndpointResponse = resp.json().await.map_err(|e| {
        OrkError::A2aClient(
            502,
            format!("oauth2 token endpoint {token_url} returned malformed JSON: {e}"),
        )
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

/// Apply `auth` to `req`. For OAuth2 variants the caller is expected to have already
/// resolved a fresh token via [`fetch_client_credentials_token`] /
/// [`TokenProvider::token`]; this helper only attaches the resolved bearer.
pub fn apply_auth(
    req: RequestBuilder,
    auth: &A2aAuth,
    resolved_bearer: Option<&SecretString>,
) -> RequestBuilder {
    match auth {
        A2aAuth::None => req,
        A2aAuth::StaticBearer(token) => req.bearer_auth(token.expose_secret()),
        A2aAuth::StaticApiKey { header, value } => req.header(header, value.expose_secret()),
        A2aAuth::OAuth2ClientCredentials { .. } | A2aAuth::OAuth2AuthorizationCode { .. } => {
            match resolved_bearer {
                Some(token) => req.bearer_auth(token.expose_secret()),
                None => req,
            }
        }
        A2aAuth::Mtls { .. } => req,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn debug_redacts_static_bearer() {
        let a = A2aAuth::StaticBearer(SecretString::from("super-secret-token"));
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("super-secret-token"), "must redact: {dbg}");
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn debug_redacts_static_api_key() {
        let a = A2aAuth::StaticApiKey {
            header: "X-API-Key".into(),
            value: SecretString::from("very-secret-key"),
        };
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("very-secret-key"));
        assert!(dbg.contains("X-API-Key"));
    }

    #[tokio::test]
    async fn cc_cache_returns_fresh_token_until_buffer_window() {
        let cache = CcTokenCache::default();
        let url = url::Url::parse("https://idp.example.com/token").unwrap();
        let scopes = vec!["scope-a".to_string()];
        cache
            .store(
                &url,
                "client",
                &scopes,
                SecretString::from("first"),
                Duration::from_secs(120),
            )
            .await;
        let cached = cache.get_fresh(&url, "client", &scopes).await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().expose_secret(), "first");
    }

    #[tokio::test]
    async fn cc_cache_drops_token_inside_refresh_buffer() {
        let cache = CcTokenCache::default();
        let url = url::Url::parse("https://idp.example.com/token").unwrap();
        let scopes = vec!["scope-a".to_string()];
        // TTL <= buffer => should be returned as `None` so caller refreshes.
        cache
            .store(
                &url,
                "client",
                &scopes,
                SecretString::from("about-to-expire"),
                Duration::from_secs(30),
            )
            .await;
        assert!(cache.get_fresh(&url, "client", &scopes).await.is_none());
    }

    #[tokio::test]
    async fn cc_cache_keys_on_token_url_client_and_scopes() {
        let cache = CcTokenCache::default();
        let url = url::Url::parse("https://idp.example.com/token").unwrap();
        let scopes_a = vec!["a".to_string()];
        let scopes_b = vec!["b".to_string()];
        cache
            .store(
                &url,
                "client",
                &scopes_a,
                SecretString::from("token-a"),
                Duration::from_secs(120),
            )
            .await;
        assert!(cache.get_fresh(&url, "client", &scopes_a).await.is_some());
        assert!(cache.get_fresh(&url, "client", &scopes_b).await.is_none());
        assert!(cache.get_fresh(&url, "other", &scopes_a).await.is_none());
    }

    #[tokio::test]
    async fn apply_auth_attaches_bearer_for_static_bearer() {
        let client = reqwest::Client::new();
        let req = client.get("https://example.com");
        let auth = A2aAuth::StaticBearer(SecretString::from("static-tok"));
        let req = apply_auth(req, &auth, None);
        let built = req.build().expect("build");
        assert_eq!(
            built
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer static-tok")
        );
    }

    #[tokio::test]
    async fn apply_auth_attaches_custom_header_for_api_key() {
        let client = reqwest::Client::new();
        let req = client.get("https://example.com");
        let auth = A2aAuth::StaticApiKey {
            header: "X-Vendor-Auth".into(),
            value: SecretString::from("k123"),
        };
        let req = apply_auth(req, &auth, None);
        let built = req.build().unwrap();
        assert_eq!(
            built
                .headers()
                .get("x-vendor-auth")
                .and_then(|v| v.to_str().ok()),
            Some("k123")
        );
    }

    #[tokio::test]
    async fn apply_auth_uses_resolved_bearer_for_oauth2_cc() {
        let client = reqwest::Client::new();
        let req = client.get("https://example.com");
        let auth = A2aAuth::OAuth2ClientCredentials {
            token_url: url::Url::parse("https://idp.example.com/token").unwrap(),
            client_id: "abc".into(),
            client_secret: SecretString::from("xyz"),
            scopes: vec!["a2a".into()],
            cache: CcTokenCache::new(),
        };
        let token = SecretString::from("resolved-bearer-tok");
        let built = apply_auth(req, &auth, Some(&token)).build().unwrap();
        assert_eq!(
            built
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer resolved-bearer-tok")
        );
    }
}
