//! Translate an [`McpServerConfig`](crate::config::McpServerConfig) into a
//! live `rmcp` client session.
//!
//! Each variant of [`McpTransportConfig`](crate::config::McpTransportConfig)
//! gets a dedicated builder; all of them collapse to a single
//! [`RunningService<RoleClient, ()>`] so the [`SessionPool`](
//! crate::session::SessionPool) doesn't need to know which transport it
//! holds.
//!
//! ## Streamable HTTP vs. legacy SSE
//!
//! ADR 0010's enum lists three transports: `stdio`, `streamable_http`, and
//! a legacy `sse`. The official `rmcp` SDK (0.16) ships only the first two
//! as standard client transports — its dedicated client-side SSE-only
//! transport was dropped in favour of the streamable-HTTP transport, which
//! speaks SSE for response streaming and is a backwards-compatible superset
//! for most "legacy SSE" endpoints. We therefore route the
//! [`McpTransportConfig::Sse`] variant through the same
//! [`StreamableHttpClientTransport`] builder as
//! [`McpTransportConfig::StreamableHttp`], so operators can keep the
//! configuration shape ADR 0010 promised. A real legacy adapter is tracked
//! in ADR 0010's `Open questions` and will land when `rmcp` re-exposes a
//! standalone `SseClientTransport`.

use std::collections::HashMap;
use std::str::FromStr;

use ork_common::error::OrkError;
use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::child_process::{ConfigureCommandExt, TokioChildProcess};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use secrecy::ExposeSecret;
use tokio::process::Command;
use tracing::warn;

use crate::auth::{CcTokenCache, resolve_http_auth};
use crate::config::{McpServerConfig, McpTransportConfig};

/// Connect to one MCP server using the supplied transport configuration and
/// return the live `rmcp` client session.
///
/// `http` is the shared `reqwest::Client` (one per ork-api process); we
/// don't synthesise a fresh client per server because it would defeat the
/// HTTP/2 connection pooling benefits and double-up TLS handshakes.
///
/// # Errors
///
/// - `Integration` when the IdP round-trip (OAuth2 CC), child-process
///   spawn, or the MCP `initialize` handshake fails.
pub async fn connect(
    server_cfg: &McpServerConfig,
    http: &reqwest::Client,
    cc_cache: &CcTokenCache,
) -> Result<RunningService<RoleClient, ()>, OrkError> {
    match &server_cfg.transport {
        McpTransportConfig::Stdio { command, args, env } => {
            connect_stdio(&server_cfg.id, command, args, env).await
        }
        McpTransportConfig::StreamableHttp { url, auth } => {
            connect_streamable_http(
                &server_cfg.id,
                url,
                auth,
                http,
                cc_cache,
                /* is_legacy_sse= */ false,
            )
            .await
        }
        McpTransportConfig::Sse { url, auth } => {
            connect_streamable_http(
                &server_cfg.id,
                url,
                auth,
                http,
                cc_cache,
                /* is_legacy_sse= */ true,
            )
            .await
        }
    }
}

async fn connect_stdio(
    server_id: &str,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<RunningService<RoleClient, ()>, OrkError> {
    let cmd = Command::new(command).configure(|c| {
        if !args.is_empty() {
            c.args(args);
        }
        if !env.is_empty() {
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
    });

    let proc = TokioChildProcess::new(cmd).map_err(|e| {
        OrkError::Integration(format!(
            "mcp stdio server `{server_id}` (cmd `{command}`) failed to spawn: {e}"
        ))
    })?;

    ().serve(proc).await.map_err(|e| {
        OrkError::Integration(format!(
            "mcp stdio server `{server_id}` initialize failed: {e}"
        ))
    })
}

async fn connect_streamable_http(
    server_id: &str,
    url: &url::Url,
    auth: &crate::config::McpAuthConfig,
    http: &reqwest::Client,
    cc_cache: &CcTokenCache,
    is_legacy_sse: bool,
) -> Result<RunningService<RoleClient, ()>, OrkError> {
    if is_legacy_sse {
        // One-time per-connection breadcrumb so operators can correlate
        // surprise streamable-HTTP behaviour against a config they wrote
        // as `type: sse`. See module docs for the upstream context.
        warn!(
            server_id = %server_id,
            url = %url,
            "ADR-0010: legacy `sse` MCP transport routed through StreamableHttpClient \
             (rmcp 0.16 ships no dedicated SseClientTransport)"
        );
    }

    let resolved = resolve_http_auth(auth, http, cc_cache).await?;

    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(bearer) = resolved.bearer {
        config = config.auth_header(bearer.expose_secret().to_string());
    }
    if !resolved.custom.is_empty() {
        let mut headers = HashMap::with_capacity(resolved.custom.len());
        for (name, value) in resolved.custom {
            let header_name = reqwest::header::HeaderName::from_str(&name).map_err(|e| {
                OrkError::Integration(format!(
                    "mcp server `{server_id}`: invalid HTTP header name `{name}`: {e}"
                ))
            })?;
            let header_value = reqwest::header::HeaderValue::from_str(value.expose_secret())
                .map_err(|e| {
                    OrkError::Integration(format!(
                        "mcp server `{server_id}`: header `{name}` value is not valid HTTP: {e}"
                    ))
                })?;
            headers.insert(header_name, header_value);
        }
        config = config.custom_headers(headers);
    }

    let transport = StreamableHttpClientTransport::with_client(http.clone(), config);

    ().serve(transport).await.map_err(|e| {
        OrkError::Integration(format!(
            "mcp http server `{server_id}` (`{url}`) initialize failed: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::CcTokenCache;
    use crate::config::McpAuthConfig;
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    #[tokio::test]
    async fn connect_stdio_reports_clear_error_for_missing_binary() {
        let cfg = McpServerConfig {
            id: "doesnt-exist".into(),
            transport: McpTransportConfig::Stdio {
                command: "/this/binary/does/not/exist".into(),
                args: vec![],
                env: HashMap::new(),
            },
        };
        let http = reqwest::Client::new();
        let cc = CcTokenCache::new();
        let err = connect(&cfg, &http, &cc).await.unwrap_err();
        match err {
            OrkError::Integration(msg) => {
                assert!(msg.contains("doesnt-exist"));
                assert!(msg.contains("failed to spawn"));
            }
            other => panic!("expected Integration error, got {other:?}"),
        }
    }

    // Same env-var serialiser rationale as `auth.rs::tests`: holding a
    // sync mutex across the await is intentional so concurrent tests
    // don't race on the global env state. See that file for the full
    // explanation.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn connect_streamable_http_attaches_bearer_to_request_url() {
        // We can't easily intercept the bearer header without a full mock
        // server, but we can at least confirm the connect attempt produces
        // a clear Integration error when the URL is unreachable, with the
        // right server id in the message — proving the `auth_header` path
        // didn't panic on an unset env var.
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("ORK_MCP_TRANSPORT_TEST_TOKEN", "tok");
        }

        let cfg = McpServerConfig {
            id: "unreachable".into(),
            transport: McpTransportConfig::StreamableHttp {
                url: url::Url::parse("http://127.0.0.1:1/mcp").unwrap(),
                auth: McpAuthConfig::Bearer {
                    value_env: "ORK_MCP_TRANSPORT_TEST_TOKEN".into(),
                },
            },
        };
        let http = reqwest::Client::new();
        let cc = CcTokenCache::new();
        let err = connect(&cfg, &http, &cc).await.unwrap_err();
        match err {
            OrkError::Integration(msg) => {
                assert!(msg.contains("unreachable"));
            }
            other => panic!("expected Integration, got {other:?}"),
        }
        unsafe {
            std::env::remove_var("ORK_MCP_TRANSPORT_TEST_TOKEN");
        }
    }

    #[tokio::test]
    async fn sse_transport_is_routed_through_streamable_http() {
        // Same trick: unreachable URL, but we exercise the `Sse` arm to
        // make sure it doesn't accidentally panic / unimplemented!() on
        // the legacy variant.
        let cfg = McpServerConfig {
            id: "legacy-sse".into(),
            transport: McpTransportConfig::Sse {
                url: url::Url::parse("http://127.0.0.1:1/sse").unwrap(),
                auth: McpAuthConfig::None,
            },
        };
        let http = reqwest::Client::new();
        let cc = CcTokenCache::new();
        let err = connect(&cfg, &http, &cc).await.unwrap_err();
        match err {
            OrkError::Integration(msg) => assert!(msg.contains("legacy-sse")),
            other => panic!("expected Integration, got {other:?}"),
        }
    }
}
