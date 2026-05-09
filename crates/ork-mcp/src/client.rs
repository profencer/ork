//! High-level MCP client: ties [`McpConfigSources`] (where do server
//! configs come from?), [`SessionPool`] (long-lived rmcp connections),
//! and [`TtlCache`] (`tools/list` results) together behind the
//! [`ToolExecutor`] trait so the rest of ork-core treats `mcp:*` tools
//! exactly like the GitHub/GitLab/code tools it already knows.
//!
//! ## Tenant resolution (ADR 0010 §`Server registration`)
//!
//! When the engine asks for `mcp:atlassian.search_jira` we:
//!
//! 1. Strip the `mcp:` prefix and split on the first `.` (see
//!    [`parse_mcp_tool_name`]).
//! 2. Resolve the `atlassian` server id via [`McpConfigSources::resolve`]
//!    — tenant-scoped settings win over the global `[mcp]` section. The
//!    workflow-inline overlay is stubbed here (returns `None`); a
//!    follow-up will hook it into the engine's existing inline-card
//!    resolver.
//! 3. Pull (or open) the tenant-isolated session from
//!    [`SessionPool::acquire`].
//! 4. Issue `tools/call` and return the structured result.
//!
//! ## Refresh loop semantics
//!
//! [`McpClient::refresh_all`] is invoked from the ork-api boot path on a
//! ticker (see ADR 0010 §`Tool discovery`). It iterates **every**
//! `(tenant_id, server_id)` already represented in the descriptor cache
//! plus the global `[mcp.servers]` entries against every known tenant,
//! calls `tools/list`, and overwrites the cached descriptor list. A
//! single failing server is logged via `tracing::warn!` and skipped — it
//! must never poison the cache for the other servers, otherwise one
//! flaky vendor would silently hide every other tool from the LLM.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::AgentContext;
use ork_core::workflow::engine::ToolExecutor;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::auth::CcTokenCache;
use crate::cache::TtlCache;
use crate::config::McpServerConfig;
use crate::descriptor::{McpToolDescriptor, parse_mcp_tool_name};
use crate::session::SessionPool;
use crate::transport::connect;

/// Resolves an MCP server id to a concrete [`McpServerConfig`] for a
/// tenant. Sources are consulted in priority order, mirroring ADR 0010
/// §`Server registration`: per-tenant settings win over global config;
/// the workflow-inline source is stubbed in v1.
#[derive(Default)]
pub struct McpConfigSources {
    /// Global `[mcp.servers]` from `AppConfig`. Indexed by id so
    /// [`resolve`] is O(1) once we fall through to the global tier.
    global: HashMap<String, McpServerConfig>,
}

impl McpConfigSources {
    /// Build an empty source set. Use [`Self::with_global`] to populate
    /// the global tier; tenant + workflow-inline overlays are added by
    /// later ADR-0010 follow-ups (see module docs).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the global server set. Idempotent so reload paths can call
    /// it without leaking stale entries.
    #[must_use]
    pub fn with_global(mut self, servers: Vec<McpServerConfig>) -> Self {
        self.global.clear();
        for srv in servers {
            self.global.insert(srv.id.clone(), srv);
        }
        self
    }

    /// Resolve a server config for `(tenant_id, server_id)`. Today we only
    /// consult globals; the tenant overlay lands with the
    /// `TenantSettings.mcp_servers` field (ADR 0010 follow-up,
    /// Task 8 in `mcp_tool_plane_*.plan.md`).
    ///
    /// # Errors
    ///
    /// [`OrkError::Integration`] when no source knows about `server_id`.
    pub async fn resolve(
        &self,
        _tenant_id: TenantId,
        server_id: &str,
    ) -> Result<McpServerConfig, OrkError> {
        // TODO(ADR-0010): tenant_repo overlay + workflow-inline overlay.
        self.global.get(server_id).cloned().ok_or_else(|| {
            OrkError::Integration(format!(
                "ADR-0010: unknown MCP server `{server_id}` (no entry in tenant settings or [mcp.servers] global config)"
            ))
        })
    }

    /// Snapshot of the global server entries; used by
    /// [`McpClient::refresh_all`] when no tenant has populated the
    /// descriptor cache yet.
    #[must_use]
    pub fn global_servers(&self) -> Vec<McpServerConfig> {
        self.global.values().cloned().collect()
    }

    /// Number of globally-registered servers (test/observability).
    #[must_use]
    pub fn global_len(&self) -> usize {
        self.global.len()
    }
}

/// Default cache TTL for `tools/list` results. ADR 0010 §`Tool discovery`
/// suggests aligning this with `refresh_interval_secs`; we hard-code 5
/// minutes here and let `McpClient::new` accept a custom value.
pub const DEFAULT_DESCRIPTOR_TTL: Duration = Duration::from_secs(300);

/// Cache key for [`McpClient::descriptors`]. Must be `(TenantId, String)`
/// (not `(TenantId, &str)`) so it can outlive the request that produced
/// it; see [`SessionPool`] for the same pattern.
pub type DescriptorKey = (TenantId, String);

/// MCP client: implements [`ToolExecutor`] for `mcp:<server>.<tool>` calls
/// and exposes [`Self::list_tools_for_tenant`] for ADR 0011's tool
/// catalog.
pub struct McpClient {
    sources: Arc<McpConfigSources>,
    sessions: Arc<SessionPool>,
    descriptors: Arc<TtlCache<DescriptorKey, Vec<McpToolDescriptor>>>,
    http: reqwest::Client,
    cc_cache: Arc<CcTokenCache>,
    cancel: CancellationToken,
    /// Eviction-loop handle. Held so the loop terminates with the client
    /// (Drop cancels the token, which the loop selects on).
    eviction_handle: Option<JoinHandle<()>>,
}

impl McpClient {
    /// Construct a new client. `sessions` is consumed; the caller should
    /// have already supplied it the same `cancel` token so shutdown is
    /// coordinated. The eviction sweeper is spawned here so the
    /// background task lifetime is tied to the client value.
    #[must_use]
    pub fn new(
        sources: Arc<McpConfigSources>,
        sessions: Arc<SessionPool>,
        descriptors: Arc<TtlCache<DescriptorKey, Vec<McpToolDescriptor>>>,
        http: reqwest::Client,
        cc_cache: Arc<CcTokenCache>,
        cancel: CancellationToken,
    ) -> Self {
        let eviction_handle = sessions.clone().spawn_eviction_loop();
        Self {
            sources,
            sessions,
            descriptors,
            http,
            cc_cache,
            cancel,
            eviction_handle: Some(eviction_handle),
        }
    }

    /// Default-wired client for tests and the simple `[mcp]` boot path:
    /// global servers only, default 5-minute TTL on the descriptor cache.
    #[must_use]
    pub fn from_global_servers(
        servers: Vec<McpServerConfig>,
        idle_ttl: Duration,
        descriptor_ttl: Duration,
        http: reqwest::Client,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let sources = Arc::new(McpConfigSources::new().with_global(servers));
        let sessions = SessionPool::new(idle_ttl, cancel.clone());
        let descriptors = Arc::new(TtlCache::new(descriptor_ttl));
        let cc_cache = CcTokenCache::new();
        Arc::new(Self::new(
            sources,
            sessions,
            descriptors,
            http,
            cc_cache,
            cancel,
        ))
    }

    /// Borrow the underlying source set; ork-api uses it to discover
    /// which globals to push through `refresh_all` per tenant.
    #[must_use]
    pub fn sources(&self) -> &Arc<McpConfigSources> {
        &self.sources
    }

    /// Borrow the descriptor cache; ADR 0011's tool catalog will read
    /// from here too.
    #[must_use]
    pub fn descriptors(&self) -> &Arc<TtlCache<DescriptorKey, Vec<McpToolDescriptor>>> {
        &self.descriptors
    }

    /// Open or reuse the rmcp session for `(tenant_id, server_id)` and
    /// run the supplied closure. Centralises the `connect` plumbing so
    /// both [`Self::execute`] and [`Self::refresh_one`] go through the
    /// same single-flight code path.
    async fn with_session<F, Fut, T>(
        &self,
        tenant_id: TenantId,
        server_cfg: &McpServerConfig,
        f: F,
    ) -> Result<T, OrkError>
    where
        F: FnOnce(Arc<RunningService<RoleClient, ()>>) -> Fut,
        Fut: std::future::Future<Output = Result<T, OrkError>>,
    {
        let server_id = server_cfg.id.clone();
        let session = self
            .sessions
            .acquire(tenant_id, &server_id, || {
                let cfg = server_cfg.clone();
                let http = self.http.clone();
                let cc = self.cc_cache.clone();
                async move { connect(&cfg, &http, &cc).await }
            })
            .await?;
        f(session).await
    }

    /// Cached tool catalog for a tenant. Returns the union of all per-
    /// `(tenant_id, server_id)` cache entries for this tenant; an entry
    /// that has not yet been refreshed is simply absent from the result.
    ///
    /// ADR 0011 will consume this list when it asks the LLM "which tools
    /// can you call?".
    #[must_use]
    pub fn list_tools_for_tenant(&self, tenant_id: TenantId) -> Vec<McpToolDescriptor> {
        self.descriptors
            .keys()
            .into_iter()
            .filter(|(t, _)| *t == tenant_id)
            .filter_map(|key| self.descriptors.get(&key))
            .flatten()
            .collect()
    }

    /// Refresh the descriptor cache for one `(tenant_id, server_id)`.
    /// Only used by tests + the periodic refresh loop; on-demand
    /// `execute` calls don't pay this cost.
    ///
    /// # Errors
    ///
    /// Whatever [`connect`] / `list_all_tools` returns; the caller is
    /// responsible for *not* propagating per-server errors out of
    /// [`Self::refresh_all`] so one bad server can't poison the others.
    pub async fn refresh_one(
        &self,
        tenant_id: TenantId,
        server_cfg: &McpServerConfig,
    ) -> Result<Vec<McpToolDescriptor>, OrkError> {
        let server_id = server_cfg.id.clone();
        let descriptors = self
            .with_session(tenant_id, server_cfg, |session| {
                let server_id = server_id.clone();
                async move {
                    let tools = session.peer().list_all_tools().await.map_err(|e| {
                        OrkError::Integration(format!(
                            "mcp server `{server_id}` list_tools failed: {e}"
                        ))
                    })?;
                    let descriptors: Vec<McpToolDescriptor> = tools
                        .into_iter()
                        .map(|tool| McpToolDescriptor {
                            server_id: server_id.clone(),
                            tool_name: tool.name.to_string(),
                            description: tool.description.map(|c| c.to_string()),
                            input_schema: serde_json::to_value((*tool.input_schema).clone())
                                .unwrap_or(Value::Null),
                        })
                        .collect();
                    Ok(descriptors)
                }
            })
            .await?;

        self.descriptors
            .insert((tenant_id, server_cfg.id.clone()), descriptors.clone());
        Ok(descriptors)
    }

    /// Iterate every `(tenant_id, server_id)` we know about and refresh
    /// its descriptor cache. Per-server failures are logged but
    /// **never** propagated; ADR 0010 §`Tool discovery` makes the
    /// "one bad server doesn't poison the cache" guarantee load-bearing.
    ///
    /// "Known" today means: every (tenant, server) already present in
    /// the descriptor cache (i.e. seen by an earlier refresh or
    /// `execute` call). The boot path warms the cache by calling
    /// [`Self::refresh_for_tenant`] once per tenant after registration.
    pub async fn refresh_all(&self) -> Result<(), OrkError> {
        let keys = self.descriptors.keys();
        if keys.is_empty() {
            debug!("ADR-0010: refresh_all skipped — descriptor cache is empty");
            return Ok(());
        }

        let mut tenants: HashMap<TenantId, BTreeSet<String>> = HashMap::new();
        for (tenant_id, server_id) in keys {
            tenants.entry(tenant_id).or_default().insert(server_id);
        }

        for (tenant_id, server_ids) in tenants {
            for server_id in server_ids {
                match self.sources.resolve(tenant_id, &server_id).await {
                    Ok(cfg) => {
                        if let Err(e) = self.refresh_one(tenant_id, &cfg).await {
                            warn!(
                                error = %e,
                                tenant_id = %tenant_id.0,
                                server_id = %server_id,
                                "ADR-0010: per-server refresh failed; cache untouched for this entry"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            tenant_id = %tenant_id.0,
                            server_id = %server_id,
                            "ADR-0010: cached server vanished from config sources; descriptor cache entry retained but stale"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Warm the descriptor cache for one tenant against the global
    /// `[mcp.servers]` set. Invoked from the boot path so the LLM tool
    /// catalog is populated before the first user request lands.
    pub async fn refresh_for_tenant(&self, tenant_id: TenantId) -> Result<(), OrkError> {
        for cfg in self.sources.global_servers() {
            if let Err(e) = self.refresh_one(tenant_id, &cfg).await {
                warn!(
                    error = %e,
                    tenant_id = %tenant_id.0,
                    server_id = %cfg.id,
                    "ADR-0010: warm-up refresh failed; will retry on next refresh tick"
                );
            }
        }
        Ok(())
    }

    /// Trigger graceful shutdown: cancels the eviction loop's token so
    /// the background task exits at its next tick.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Don't .await the join handle in Drop; the eviction loop
        // selects on the cancel token and will exit on its own.
        if let Some(h) = self.eviction_handle.take() {
            h.abort();
        }
    }
}

#[async_trait]
impl ToolExecutor for McpClient {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &Value,
    ) -> Result<Value, OrkError> {
        let tenant_id = ctx.tenant_id;
        let (server_id, tool) = parse_mcp_tool_name(tool_name)?;
        let server_cfg = self.sources.resolve(tenant_id, &server_id).await?;

        let arguments = match input {
            Value::Null => None,
            Value::Object(map) => Some(map.clone()),
            other => {
                return Err(OrkError::Validation(format!(
                    "MCP tool `{tool_name}` expects a JSON object as input, got {} (ADR-0010 §`Tool discovery`)",
                    other_type(other)
                )));
            }
        };

        let tool_for_log = tool.clone();
        let result = self
            .with_session(tenant_id, &server_cfg, |session| {
                let tool = tool.clone();
                async move {
                    let params = CallToolRequestParams {
                        meta: None,
                        name: tool.into(),
                        arguments,
                        task: None,
                    };
                    session.peer().call_tool(params).await.map_err(|e| {
                        OrkError::Integration(format!("mcp call_tool {tool_for_log}: {e}"))
                    })
                }
            })
            .await?;

        serde_json::to_value(result)
            .map_err(|e| OrkError::Internal(format!("serialise mcp CallToolResult: {e}")))
    }
}

fn other_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpAuthConfig, McpTransportConfig};
    use ork_core::a2a::CallerIdentity;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn test_ctx(tenant: TenantId) -> AgentContext {
        AgentContext {
            tenant_id: tenant,
            task_id: ork_a2a::TaskId::new(),
            parent_task_id: None,
            cancel: CancellationToken::new(),
            caller: CallerIdentity {
                tenant_id: tenant,
                user_id: None,
                scopes: vec![],
                ..CallerIdentity::default()
            },
            push_notification_url: None,
            trace_ctx: None,
            context_id: None,
            workflow_input: serde_json::Value::Null,
            iteration: None,
            delegation_depth: 0,
            delegation_chain: Vec::new(),
            step_llm_overrides: None,
            artifact_store: None,
            artifact_public_base: None,
            resource_id: None,
            thread_id: None,
        }
    }

    fn dummy_global_server(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            transport: McpTransportConfig::StreamableHttp {
                // Unreachable; tests that hit this URL expect an
                // Integration error to bubble up.
                url: url::Url::parse("http://127.0.0.1:1/mcp").unwrap(),
                auth: McpAuthConfig::None,
            },
        }
    }

    fn client_with(servers: Vec<McpServerConfig>) -> Arc<McpClient> {
        McpClient::from_global_servers(
            servers,
            Duration::from_secs(60),
            Duration::from_secs(60),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn execute_rejects_non_mcp_prefixed_names() {
        let client = client_with(vec![]);
        let tenant = TenantId::new();
        let err = client
            .execute(&test_ctx(tenant), "github_recent_activity", &json!({}))
            .await
            .unwrap_err();
        match err {
            OrkError::Validation(msg) => assert!(msg.contains("mcp:")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_rejects_unknown_server() {
        // No globals registered; resolve must fail loudly so misconfig
        // shows up at the API edge rather than as a confusing timeout.
        let client = client_with(vec![]);
        let tenant = TenantId::new();
        let err = client
            .execute(&test_ctx(tenant), "mcp:atlassian.search_jira", &json!({}))
            .await
            .unwrap_err();
        match err {
            OrkError::Integration(msg) => {
                assert!(msg.contains("atlassian"));
                assert!(msg.contains("unknown MCP server"));
            }
            other => panic!("expected Integration, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_rejects_non_object_input() {
        // Ensure callers can't accidentally pass `null` or a primitive
        // and get a confusing rmcp-internal panic; we want a typed
        // Validation error early.
        let client = client_with(vec![dummy_global_server("srv")]);
        let tenant = TenantId::new();
        let err = client
            .execute(&test_ctx(tenant), "mcp:srv.tool", &json!("not-an-object"))
            .await
            .unwrap_err();
        match err {
            OrkError::Validation(msg) => assert!(msg.contains("expects a JSON object")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_tools_for_tenant_returns_cached_descriptors() {
        let client = client_with(vec![]);
        let tenant = TenantId::new();
        let other_tenant = TenantId::new();

        let descriptors = vec![
            McpToolDescriptor {
                server_id: "srv".into(),
                tool_name: "echo".into(),
                description: Some("echo back".into()),
                input_schema: json!({"type": "object"}),
            },
            McpToolDescriptor {
                server_id: "srv".into(),
                tool_name: "ping".into(),
                description: None,
                input_schema: json!({"type": "object"}),
            },
        ];
        client
            .descriptors()
            .insert((tenant, "srv".into()), descriptors.clone());
        client
            .descriptors()
            .insert((other_tenant, "srv".into()), vec![]);

        let listed = client.list_tools_for_tenant(tenant);
        assert_eq!(listed.len(), 2);
        assert!(
            listed.iter().any(|d| d.tool_name == "echo"),
            "list_tools_for_tenant must return cached descriptors for the queried tenant"
        );
        assert!(
            listed.iter().all(|d| d.server_id == "srv"),
            "tenant isolation: must NOT leak descriptors from other tenants"
        );

        let empty = client.list_tools_for_tenant(TenantId::new());
        assert!(
            empty.is_empty(),
            "tenants with no cached descriptors must get an empty list"
        );
    }

    #[tokio::test]
    async fn refresh_all_with_empty_cache_is_noop() {
        // Boot-time invariant: on a cold cache, refresh_all must not
        // attempt to dial any MCP server (no tenants known yet).
        let client = client_with(vec![dummy_global_server("srv")]);
        client.refresh_all().await.expect("noop must not error");
        assert!(client.list_tools_for_tenant(TenantId::new()).is_empty());
    }

    #[tokio::test]
    async fn refresh_all_does_not_poison_cache_on_per_server_failure() {
        // Plant a healthy (cached) descriptor list for one server, then
        // call refresh_all. The cached entry's server is *unreachable*
        // (connect will fail), so refresh_one will error — but the
        // descriptor cache must keep the previously-cached entry
        // intact (we only overwrite on success).
        let client = client_with(vec![dummy_global_server("srv-down")]);
        let tenant = TenantId::new();
        let cached = vec![McpToolDescriptor {
            server_id: "srv-down".into(),
            tool_name: "still-listed".into(),
            description: None,
            input_schema: json!({"type": "object"}),
        }];
        client
            .descriptors()
            .insert((tenant, "srv-down".into()), cached.clone());

        client
            .refresh_all()
            .await
            .expect("must swallow per-server errors");

        let listed = client.list_tools_for_tenant(tenant);
        assert_eq!(
            listed, cached,
            "refresh_all failed for srv-down but the previously-cached descriptors must survive (ADR-0010 §`Tool discovery`)"
        );
    }

    #[test]
    fn config_sources_resolve_returns_clear_error_on_unknown_server() {
        let sources = McpConfigSources::new().with_global(vec![dummy_global_server("known")]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(sources.resolve(TenantId::new(), "missing"))
            .unwrap_err();
        match err {
            OrkError::Integration(msg) => {
                assert!(msg.contains("missing"));
                assert!(msg.contains("[mcp.servers]"));
            }
            other => panic!("expected Integration, got {other:?}"),
        }
    }

    #[test]
    fn config_sources_with_global_overwrites() {
        let sources = McpConfigSources::new()
            .with_global(vec![dummy_global_server("a"), dummy_global_server("b")])
            .with_global(vec![dummy_global_server("only-c")]);
        assert_eq!(sources.global_len(), 1);
    }
}
