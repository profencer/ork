//! `LlmRouter` — the global [`LlmProvider`] consumed by the agent loop and
//! the workflow engine (ADR 0012 §`Routing`).
//!
//! The router holds an operator-side catalog (built from
//! [`ork_common::config::LlmConfig`] at boot, with all `env`-form headers
//! eagerly resolved per the ADR's "fail loud at boot" rule) plus a tenant
//! resolver that can hand back per-tenant overrides. On each call:
//!
//! 1. Look up the resolved provider id from the [`ChatRequest`] (or the
//!    operator/tenant defaults when the request leaves it `None`).
//! 2. Merge tenant overrides over the operator catalog (id-collision
//!    replaces, per ADR 0012 §`Provider catalog` mirroring ADR 0010).
//! 3. Resolve the model via the precedence chain
//!    (`request → tenant → resolved-provider default`).
//! 4. Cache and reuse the materialised [`OpenAiCompatibleProvider`] keyed
//!    by `(tenant_id, provider_id)`. See the comment on [`CacheKey`] for
//!    the rationale (the open question called out in the ADR).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::config::{HeaderValueSource, LlmConfig, LlmProviderConfig};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::a2a::ResolveContext;
use ork_core::ports::llm::{
    ChatRequest, ChatResponse, LlmChatStream, LlmProvider, ModelCapabilities,
};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::openai_compatible::OpenAiCompatibleProvider;

/// Async hook used by the router to fetch tenant-side LLM catalog overrides.
/// Lives behind a trait so `ork-llm` does not import `ork-persistence`
/// (AGENTS.md §3.4 hexagonal invariant).
///
/// `ork-api` plugs in a [`TenantRepository`](#)-backed implementation;
/// CLI binaries and tests use [`NoopTenantLlmCatalog`]. Returning `None`
/// means "no tenant context in scope" — the router falls back to the
/// operator catalog only.
#[async_trait]
pub trait TenantLlmCatalog: Send + Sync {
    /// Fetch the catalog overrides for `tenant_id`. Returning `Ok(None)`
    /// means "tenant exists but has no LLM overrides"; an `Err` propagates
    /// out of the router as `OrkError::LlmProvider`.
    async fn lookup(&self, tenant_id: TenantId) -> Result<Option<TenantLlmCatalogEntry>, OrkError>;
}

/// What [`TenantLlmCatalog::lookup`] hands back: the per-tenant override
/// catalog plus the optional tenant-level default selectors. Mirrors the
/// `TenantSettings` slice this ADR introduced — kept as a discrete struct
/// here so `ork-llm` does not have to depend on `ork-core::models::tenant`
/// transitively (the router only needs the catalog shape, not the full
/// `TenantSettings` blob).
#[derive(Debug, Clone, Default)]
pub struct TenantLlmCatalogEntry {
    pub providers: Vec<LlmProviderConfig>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
}

/// `TenantLlmCatalog` impl for callers that have no tenant resolver wired
/// up (CLI, unit tests, dev mode without `ork-persistence`). Always
/// returns `None`, which collapses the router to "operator catalog only"
/// behaviour.
pub struct NoopTenantLlmCatalog;

#[async_trait]
impl TenantLlmCatalog for NoopTenantLlmCatalog {
    async fn lookup(
        &self,
        _tenant_id: TenantId,
    ) -> Result<Option<TenantLlmCatalogEntry>, OrkError> {
        Ok(None)
    }
}

/// Cache key for materialised providers. We picked
/// `(Option<TenantId>, ProviderId)` over the alternative
/// (resolved-header-set hashed) because:
///
/// - It mirrors the `(tenant_id, server_id)` model from `mcp_servers`
///   (ADR 0010), which keeps operators from learning two cache models.
/// - `(tenant, provider)` keys are bounded by the number of tenants ×
///   the number of catalog entries; header-hash keys are unbounded
///   across the value space.
///
/// Header-hash keying would also work and would invalidate
/// automatically on tenant edits (a new header set hashes differently),
/// at the cost of unbounded keyspace. We picked `(tenant, provider)`
/// for the predictable mental model and bounded memory; the trade-off
/// is that a tenant who edits their own override sees one stale cached
/// client until the tenant-cache layer drops it. Acceptable — the
/// router doesn't claim to be a hot-reload surface (called out in
/// ADR 0012 §`Negative / costs`).
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
struct CacheKey {
    /// `None` ⇒ pure operator entry; `Some` ⇒ tenant override (id may
    /// or may not collide with an operator entry).
    tenant_id: Option<TenantId>,
    provider_id: String,
}

pub struct LlmRouter {
    /// Pre-resolved operator providers, keyed by id. Built at
    /// [`Self::from_config`] time; env vars are resolved eagerly so a
    /// missing secret fails the binary at boot, not at first request.
    operator_providers: HashMap<String, Arc<OpenAiCompatibleProvider>>,
    /// Operator-side fallback selector. Mirrored from
    /// [`LlmConfig::default_provider`] at boot; the router never mutates it.
    operator_default_provider: Option<String>,
    /// Tenant-side resolver. Always present; defaults to
    /// [`NoopTenantLlmCatalog`] when no `ork-persistence` is wired up.
    tenant_catalog: Arc<dyn TenantLlmCatalog>,
    /// Materialised tenant-override providers keyed per [`CacheKey`].
    /// `RwLock` rather than `Mutex` because reads dominate writes (we
    /// only insert on cache miss).
    ///
    /// `TODO(ADR-0012-followup): bound or LRU` — today this map grows
    /// without bound proportional to (#tenants × #unique override ids
    /// per tenant). Long-running processes with high tenant churn will
    /// see slow per-process memory growth. Acceptable for now (called
    /// out in ADR 0012 §`Negative / costs`); revisit when the first
    /// operator runs into it.
    tenant_provider_cache: RwLock<HashMap<CacheKey, Arc<OpenAiCompatibleProvider>>>,
}

impl std::fmt::Debug for LlmRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmRouter")
            .field(
                "operator_providers",
                &self.operator_providers.keys().collect::<Vec<_>>(),
            )
            .field("operator_default_provider", &self.operator_default_provider)
            .finish_non_exhaustive()
    }
}

impl LlmRouter {
    /// Build the router from the operator catalog plus a tenant
    /// resolver. Resolves every operator-side `env`-form header at
    /// construction time; a missing variable returns
    /// `OrkError::LlmProvider` so `ork-api`'s `main()` aborts before
    /// serving any traffic.
    pub fn from_config(
        cfg: &LlmConfig,
        tenant_catalog: Arc<dyn TenantLlmCatalog>,
    ) -> Result<Self, OrkError> {
        let mut operator_providers = HashMap::with_capacity(cfg.providers.len());
        for entry in &cfg.providers {
            let provider = build_provider(entry)?;
            if operator_providers
                .insert(entry.id.clone(), Arc::new(provider))
                .is_some()
            {
                return Err(OrkError::LlmProvider(format!(
                    "duplicate llm provider id `{}` in operator catalog",
                    entry.id
                )));
            }
        }

        if let Some(def) = cfg.default_provider.as_deref()
            && !operator_providers.contains_key(def)
        {
            return Err(OrkError::LlmProvider(format!(
                "default_provider `{def}` is not present in [llm.providers]"
            )));
        }

        Ok(Self {
            operator_providers,
            operator_default_provider: cfg.default_provider.clone(),
            tenant_catalog,
            tenant_provider_cache: RwLock::new(HashMap::new()),
        })
    }

    /// Read the current [`ResolveContext`] tenant id and look up its
    /// [`TenantLlmCatalogEntry`] (if any). Shared by [`Self::resolve`],
    /// [`Self::resolved_provider_id_for`], and the
    /// [`LlmProvider::capabilities_for`] impl below so they all see the
    /// same tenant view per call.
    async fn lookup_tenant_entry(
        &self,
    ) -> Result<(Option<TenantId>, Option<TenantLlmCatalogEntry>), OrkError> {
        let tenant = ResolveContext::current().map(|c| c.tenant_id);
        let entry = match tenant {
            Some(id) => self.tenant_catalog.lookup(id).await?,
            None => None,
        };
        Ok((tenant, entry))
    }

    /// Resolve `(provider, model)` for `req` given the current
    /// [`ResolveContext`]. The model can be `None` here — the wire client
    /// falls back to its `default_model` in that case.
    async fn resolve(
        &self,
        req: &ChatRequest,
    ) -> Result<(Arc<OpenAiCompatibleProvider>, Option<String>), OrkError> {
        let (tenant, tenant_entry) = self.lookup_tenant_entry().await?;

        let provider_id =
            pick_provider_id(req, tenant_entry.as_ref(), &self.operator_default_provider)
                .ok_or_else(|| {
                    OrkError::LlmProvider(
                        "no provider selected and no default_provider configured at any level"
                            .into(),
                    )
                })?;

        let provider = self
            .resolve_provider(tenant, &provider_id, tenant_entry.as_ref())
            .await?;

        // Model precedence: ChatRequest.model > tenant default >
        // resolved-provider's default_model (handled inside the
        // provider). The caller is responsible for collapsing the
        // step → agent precedence onto `req.model` before we get here
        // (see `LocalAgent::send_stream`).
        let model = req
            .model
            .clone()
            .or_else(|| tenant_entry.as_ref().and_then(|t| t.default_model.clone()));

        Ok((provider, model))
    }

    /// Resolve the provider id that *would* answer `req` under the
    /// current [`ResolveContext`], without materialising the provider
    /// or touching the cache. Public so callers (e.g. the agent loop's
    /// pre-flight capability check) can reason about which provider is
    /// in scope before issuing the actual `chat`/`chat_stream` call.
    pub async fn resolved_provider_id_for(&self, req: &ChatRequest) -> Result<String, OrkError> {
        let (_, tenant_entry) = self.lookup_tenant_entry().await?;
        pick_provider_id(req, tenant_entry.as_ref(), &self.operator_default_provider).ok_or_else(
            || {
                OrkError::LlmProvider(
                    "no provider selected and no default_provider configured at any level".into(),
                )
            },
        )
    }

    async fn resolve_provider(
        &self,
        tenant: Option<TenantId>,
        provider_id: &str,
        tenant_entry: Option<&TenantLlmCatalogEntry>,
    ) -> Result<Arc<OpenAiCompatibleProvider>, OrkError> {
        // Tenant override path: id-collision replaces the operator entry.
        if let (Some(tid), Some(entry)) = (tenant, tenant_entry)
            && let Some(cfg) = entry.providers.iter().find(|p| p.id == provider_id)
        {
            let key = CacheKey {
                tenant_id: Some(tid),
                provider_id: provider_id.to_string(),
            };
            if let Some(p) = self.tenant_provider_cache.read().await.get(&key) {
                return Ok(p.clone());
            }
            let provider = Arc::new(build_provider(cfg)?);
            self.tenant_provider_cache
                .write()
                .await
                .insert(key, provider.clone());
            return Ok(provider);
        }

        // Otherwise fall through to the operator catalog.
        self.operator_providers
            .get(provider_id)
            .cloned()
            .ok_or_else(|| {
                OrkError::LlmProvider(format!(
                    "provider `{provider_id}` is not in the operator catalog and no tenant override matches"
                ))
            })
    }
}

/// Resolve env-form headers once into literal strings and feed the
/// resulting [`OpenAiCompatibleProvider`] back to the caller. Any unset
/// env var here returns `OrkError::LlmProvider` so the boot-time builder
/// can blow up before the API binds a port.
fn build_provider(cfg: &LlmProviderConfig) -> Result<OpenAiCompatibleProvider, OrkError> {
    let mut headers: HashMap<String, String> = HashMap::with_capacity(cfg.headers.len());
    for (name, source) in &cfg.headers {
        let value = match source {
            HeaderValueSource::Value { value } => value.clone(),
            HeaderValueSource::Env { env } => std::env::var(env).map_err(|_| {
                OrkError::LlmProvider(format!(
                    "llm provider `{}` header `{}` references env var `{}` which is unset",
                    cfg.id, name, env
                ))
            })?,
        };
        headers.insert(name.clone(), value);
    }
    Ok(OpenAiCompatibleProvider::new(
        cfg.id.clone(),
        cfg.base_url.clone(),
        cfg.default_model.clone(),
        headers,
        cfg.capabilities.clone(),
    ))
}

/// Pure helper: pick the provider id given the request and the catalog
/// defaults. `WorkflowStep.provider` and `AgentConfig.provider` reach the
/// router via [`ChatRequest::provider`] (the caller squashes the two onto
/// it), so we only see the request and the tenant/operator defaults at
/// this layer.
fn pick_provider_id(
    req: &ChatRequest,
    tenant_entry: Option<&TenantLlmCatalogEntry>,
    operator_default: &Option<String>,
) -> Option<String> {
    if let Some(p) = req.provider.as_ref() {
        return Some(p.clone());
    }
    if let Some(t) = tenant_entry
        && let Some(p) = t.default_provider.as_ref()
    {
        return Some(p.clone());
    }
    operator_default.clone()
}

#[async_trait]
impl LlmProvider for LlmRouter {
    async fn chat(&self, mut request: ChatRequest) -> Result<ChatResponse, OrkError> {
        let (provider, model) = self.resolve(&request).await?;
        request.model = model;
        debug!(
            provider = %provider.provider_name(),
            model = ?request.model,
            "router resolved chat request"
        );
        provider.chat(request).await
    }

    async fn chat_stream(&self, mut request: ChatRequest) -> Result<LlmChatStream, OrkError> {
        let (provider, model) = self.resolve(&request).await?;
        request.model = model;
        debug!(
            provider = %provider.provider_name(),
            model = ?request.model,
            "router resolved chat_stream request"
        );
        provider.chat_stream(request).await
    }

    fn provider_name(&self) -> &str {
        "router"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // The sync `capabilities()` entrypoint can't see the
        // `ResolveContext` tenant — that lookup is async — so it falls
        // back to the operator default provider only. **This may be
        // wrong for tenant-overridden providers.** Callers that have a
        // `&ChatRequest` in scope should prefer
        // [`Self::capabilities_for`], which honours the same
        // `ResolveContext` chain `chat_stream` does.
        if let Some(def) = self.operator_default_provider.as_deref()
            && let Some(p) = self.operator_providers.get(def)
        {
            return p.capabilities(model);
        }
        if self.operator_providers.is_empty() {
            warn!(
                "LlmRouter::capabilities called but no providers are configured; \
                returning trait default"
            );
        }
        ModelCapabilities::default()
    }

    /// Async, request-aware capability lookup. Walks the same
    /// `ResolveContext`-driven chain `chat_stream` does so the caller
    /// gets the capabilities of the provider that *would* actually
    /// answer the request — including tenant-overridden providers and
    /// non-default operator entries selected via `request.provider`.
    ///
    /// On any resolution failure (no provider could be selected,
    /// tenant lookup errored, etc.) the impl falls back to the sync
    /// [`Self::capabilities`] gentle default rather than propagating
    /// the error — capability lookups are best-effort gates, not
    /// hard failures.
    async fn capabilities_for(&self, request: &ChatRequest) -> ModelCapabilities {
        let (tenant, tenant_entry) = match self.lookup_tenant_entry().await {
            Ok(p) => p,
            Err(_) => return self.capabilities(request.model.as_deref().unwrap_or("")),
        };
        let Some(provider_id) = pick_provider_id(
            request,
            tenant_entry.as_ref(),
            &self.operator_default_provider,
        ) else {
            return self.capabilities(request.model.as_deref().unwrap_or(""));
        };
        let provider = match self
            .resolve_provider(tenant, &provider_id, tenant_entry.as_ref())
            .await
        {
            Ok(p) => p,
            Err(_) => return self.capabilities(request.model.as_deref().unwrap_or("")),
        };
        let model = request
            .model
            .clone()
            .or_else(|| tenant_entry.as_ref().and_then(|t| t.default_model.clone()))
            .unwrap_or_default();
        provider.capabilities(&model)
    }
}

/// Capability lookup helper that uses an explicit `(provider_id, model)`
/// pair rather than relying on the global `Self::capabilities` heuristic.
/// Kept on `LlmRouter` (not the trait) because it's router-specific.
impl LlmRouter {
    /// Look up [`ModelCapabilities`] for an explicit `(provider, model)`
    /// pair. Returns `None` when the provider id is unknown. Used by
    /// callers that have a resolved provider id in hand and want the
    /// authoritative answer (`LocalAgent`'s tool gate, ADR-0011 follow-up).
    #[must_use]
    pub fn capabilities_of(&self, provider_id: &str, model: &str) -> Option<ModelCapabilities> {
        self.operator_providers
            .get(provider_id)
            .map(|p| p.capabilities(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_common::config::ModelCapabilitiesEntry;
    use std::collections::BTreeMap;

    fn op_cfg(id: &str) -> LlmProviderConfig {
        LlmProviderConfig {
            id: id.into(),
            base_url: format!("https://example.com/{id}/v1"),
            default_model: Some(format!("{id}-default")),
            headers: BTreeMap::new(),
            capabilities: vec![ModelCapabilitiesEntry {
                model: format!("{id}-default"),
                supports_tools: true,
                supports_streaming: true,
                supports_vision: false,
                max_context: Some(1024),
            }],
        }
    }

    #[test]
    fn pick_provider_id_request_wins() {
        let mut req = ChatRequest::simple(Vec::new(), None, None, None);
        req.provider = Some("from-req".into());
        let tenant = TenantLlmCatalogEntry {
            providers: Vec::new(),
            default_provider: Some("from-tenant".into()),
            default_model: None,
        };
        let op = Some("from-op".into());
        assert_eq!(
            pick_provider_id(&req, Some(&tenant), &op).as_deref(),
            Some("from-req")
        );
    }

    #[test]
    fn pick_provider_id_tenant_default_beats_operator() {
        let req = ChatRequest::simple(Vec::new(), None, None, None);
        let tenant = TenantLlmCatalogEntry {
            providers: Vec::new(),
            default_provider: Some("from-tenant".into()),
            default_model: None,
        };
        let op = Some("from-op".into());
        assert_eq!(
            pick_provider_id(&req, Some(&tenant), &op).as_deref(),
            Some("from-tenant")
        );
    }

    #[test]
    fn pick_provider_id_falls_through_to_operator() {
        let req = ChatRequest::simple(Vec::new(), None, None, None);
        let op = Some("from-op".into());
        assert_eq!(
            pick_provider_id(&req, None, &op).as_deref(),
            Some("from-op")
        );
    }

    #[test]
    fn pick_provider_id_returns_none_when_nothing_set() {
        let req = ChatRequest::simple(Vec::new(), None, None, None);
        assert!(pick_provider_id(&req, None, &None).is_none());
    }

    #[test]
    fn from_config_rejects_default_pointing_at_missing_provider() {
        let cfg = LlmConfig {
            default_provider: Some("does-not-exist".into()),
            providers: vec![op_cfg("openai")],
        };
        let err = LlmRouter::from_config(&cfg, Arc::new(NoopTenantLlmCatalog))
            .expect_err("must error on dangling default");
        match err {
            OrkError::LlmProvider(msg) => assert!(msg.contains("does-not-exist")),
            other => panic!("expected LlmProvider error, got {other:?}"),
        }
    }

    #[test]
    fn from_config_rejects_duplicate_provider_ids() {
        let cfg = LlmConfig {
            default_provider: None,
            providers: vec![op_cfg("openai"), op_cfg("openai")],
        };
        let err = LlmRouter::from_config(&cfg, Arc::new(NoopTenantLlmCatalog)).unwrap_err();
        assert!(matches!(err, OrkError::LlmProvider(m) if m.contains("duplicate")));
    }

    #[test]
    fn from_config_rejects_missing_env_var() {
        // Pick an env var name extremely unlikely to be set in any test
        // environment so this stays deterministic across machines.
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".into(),
            HeaderValueSource::Env {
                env: "ORK_TEST_DEFINITELY_UNSET_HEADER_VAR_FOR_ADR_0012".into(),
            },
        );
        let cfg = LlmConfig {
            default_provider: None,
            providers: vec![LlmProviderConfig {
                id: "openai".into(),
                base_url: "https://example.com".into(),
                default_model: None,
                headers,
                capabilities: Vec::new(),
            }],
        };
        let err = LlmRouter::from_config(&cfg, Arc::new(NoopTenantLlmCatalog)).unwrap_err();
        assert!(matches!(err, OrkError::LlmProvider(m) if m.contains("unset")));
    }

    #[test]
    fn capabilities_of_returns_per_provider_lookup() {
        let cfg = LlmConfig {
            default_provider: Some("openai".into()),
            providers: vec![op_cfg("openai")],
        };
        let r = LlmRouter::from_config(&cfg, Arc::new(NoopTenantLlmCatalog)).unwrap();
        let caps = r
            .capabilities_of("openai", "openai-default")
            .expect("known provider");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context, 1024);
        assert!(r.capabilities_of("missing", "x").is_none());
    }
}
