//! ADR-0021 §`Decision points` step 4: scope-checked decorator over an
//! [`ArtifactStore`].
//!
//! The decorator wraps a backend with a caller-scope set and gates each
//! method on the matching `artifact:<scope>:<action>` shape from
//! ADR-0021 §`Vocabulary`. `<scope>` is `tenant` for tenant-wide refs
//! (i.e. `context_id == None`) and `context-<uuid>` for refs scoped to
//! a specific A2A context.
//!
//! Constructed once per request: `ScopeCheckedArtifactStore::new(inner,
//! caller.scopes.clone())`. Reusing the same decorator across requests
//! would carry the wrong scope set; that footgun is the same one ADR-0021
//! §`ArtifactStore boundary` calls out for "raw access bypasses the
//! check".
//!
//! The matcher is shared with [`ork_security::ScopeChecker::allows`] so
//! a `tenant:*:*` grant — disallowed by the format validator at mint
//! time — cannot accidentally satisfy this gate.
//!
//! Tool-side wiring (`artifact_*` tools, workflow spillover) is a
//! follow-up: those paths build their own `Arc<dyn ArtifactStore>` and
//! today rely on the route-level gate plus the in-store tenant filter
//! (`ArtifactRef::tenant_id == ctx.tenant_id`).

use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use ork_security::ScopeChecker;
use std::sync::Arc;
use url::Url;

/// Wraps an [`ArtifactStore`] and rejects calls that the caller's scope
/// set does not authorise.
pub struct ScopeCheckedArtifactStore {
    inner: Arc<dyn ArtifactStore>,
    caller_scopes: Vec<String>,
}

impl ScopeCheckedArtifactStore {
    /// Build a per-request decorator. `caller_scopes` is the snapshot of
    /// `AuthContext::scopes` (or `AgentContext::caller::scopes`) the
    /// request entered with — sharing the decorator across requests is
    /// the footgun ADR-0021 §`ArtifactStore boundary` calls out.
    #[must_use]
    pub fn new(inner: Arc<dyn ArtifactStore>, caller_scopes: Vec<String>) -> Self {
        Self {
            inner,
            caller_scopes,
        }
    }

    fn scope_token_for_scope(scope: &ArtifactScope) -> String {
        match scope.context_id {
            Some(ctx) => format!("context-{}", ctx.0),
            None => "tenant".into(),
        }
    }

    fn scope_token_for_ref(r: &ArtifactRef) -> String {
        match r.context_id {
            Some(ctx) => format!("context-{}", ctx.0),
            None => "tenant".into(),
        }
    }

    fn require(&self, scope_token: &str, action: &str) -> Result<(), OrkError> {
        let required = format!("artifact:{scope_token}:{action}");
        ScopeChecker::require(&self.caller_scopes, &required)
    }
}

#[async_trait]
impl ArtifactStore for ScopeCheckedArtifactStore {
    fn scheme(&self) -> &'static str {
        self.inner.scheme()
    }

    async fn put(
        &self,
        scope: &ArtifactScope,
        name: &str,
        body: ArtifactBody,
        meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        self.require(&Self::scope_token_for_scope(scope), "write")?;
        self.inner.put(scope, name, body, meta).await
    }

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        self.require(&Self::scope_token_for_ref(r#ref), "read")?;
        self.inner.get(r#ref).await
    }

    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        self.require(&Self::scope_token_for_ref(r#ref), "read")?;
        self.inner.head(r#ref).await
    }

    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        self.require(&Self::scope_token_for_scope(scope), "read")?;
        self.inner.list(scope, prefix).await
    }

    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        self.require(&Self::scope_token_for_ref(r#ref), "delete")?;
        self.inner.delete(r#ref).await
    }

    async fn presign_get(
        &self,
        r#ref: &ArtifactRef,
        ttl: Duration,
    ) -> Result<Option<Url>, OrkError> {
        self.require(&Self::scope_token_for_ref(r#ref), "read")?;
        self.inner.presign_get(r#ref, ttl).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ork_a2a::ContextId;
    use ork_common::types::TenantId;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Minimal in-memory store: the decorator's gate is the SUT, not
    /// any backend behaviour.
    struct MemStore {
        calls: Mutex<Vec<&'static str>>,
    }

    impl MemStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, name: &'static str) {
            self.calls.lock().unwrap().push(name);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ArtifactStore for MemStore {
        fn scheme(&self) -> &'static str {
            "mem"
        }

        async fn put(
            &self,
            _scope: &ArtifactScope,
            name: &str,
            _body: ArtifactBody,
            _meta: ArtifactMeta,
        ) -> Result<ArtifactRef, OrkError> {
            self.record("put");
            Ok(ArtifactRef {
                scheme: "mem".into(),
                tenant_id: TenantId(Uuid::nil()),
                context_id: None,
                name: name.into(),
                version: 1,
                etag: "deadbeef".into(),
            })
        }

        async fn get(&self, _r: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
            self.record("get");
            Ok(ArtifactBody::Bytes(Bytes::from_static(b"x")))
        }

        async fn head(&self, _r: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
            self.record("head");
            Ok(ArtifactMeta::default())
        }

        async fn list(
            &self,
            _s: &ArtifactScope,
            _prefix: Option<&str>,
        ) -> Result<Vec<ArtifactSummary>, OrkError> {
            self.record("list");
            Ok(vec![])
        }

        async fn delete(&self, _r: &ArtifactRef) -> Result<(), OrkError> {
            self.record("delete");
            Ok(())
        }
    }

    fn tenant_scope() -> ArtifactScope {
        ArtifactScope {
            tenant_id: TenantId(Uuid::nil()),
            context_id: None,
        }
    }

    fn context_scope(ctx: Uuid) -> ArtifactScope {
        ArtifactScope {
            tenant_id: TenantId(Uuid::nil()),
            context_id: Some(ContextId(ctx)),
        }
    }

    fn aref_with(ctx: Option<Uuid>) -> ArtifactRef {
        ArtifactRef {
            scheme: "mem".into(),
            tenant_id: TenantId(Uuid::nil()),
            context_id: ctx.map(ContextId),
            name: "blob".into(),
            version: 1,
            etag: "x".into(),
        }
    }

    fn meta() -> ArtifactMeta {
        ArtifactMeta {
            mime: None,
            size: 0,
            created_at: chrono::Utc::now(),
            created_by: None,
            task_id: None,
            labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn put_denied_without_write_scope() {
        let inner = MemStore::new();
        let dec = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec!["artifact:tenant:read".into()],
        );
        let err = dec
            .put(
                &tenant_scope(),
                "blob",
                ArtifactBody::Bytes(Bytes::from_static(b"x")),
                meta(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OrkError::Forbidden(ref m) if m.contains("artifact:tenant:write")));
        assert!(
            inner.calls().is_empty(),
            "denied call must not reach backend"
        );
    }

    #[tokio::test]
    async fn get_allowed_with_tenant_read() {
        let inner = MemStore::new();
        let dec = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec!["artifact:tenant:read".into()],
        );
        dec.get(&aref_with(None)).await.expect("granted");
        assert_eq!(inner.calls(), vec!["get"]);
    }

    #[tokio::test]
    async fn get_on_context_ref_requires_context_scope() {
        let ctx_uuid = Uuid::from_u128(0xfafa);
        let inner = MemStore::new();
        // Caller has tenant-wide read but NOT the per-context scope.
        let dec = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec!["artifact:tenant:read".into()],
        );
        match dec.get(&aref_with(Some(ctx_uuid))).await {
            Err(OrkError::Forbidden(m)) => {
                assert!(
                    m.contains(&format!("artifact:context-{ctx_uuid}:read")),
                    "deny message must surface the context scope, got `{m}`"
                );
            }
            Err(other) => panic!("expected Forbidden, got {other:?}"),
            Ok(_) => panic!("expected Forbidden, got Ok"),
        }

        // With the per-context scope, the call goes through.
        let dec2 = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec![format!("artifact:context-{ctx_uuid}:read")],
        );
        if let Err(e) = dec2.get(&aref_with(Some(ctx_uuid))).await {
            panic!("granted call should succeed, got {e:?}");
        }
    }

    #[tokio::test]
    async fn list_requires_read_on_chosen_scope() {
        let inner = MemStore::new();
        let dec = ScopeCheckedArtifactStore::new(inner.clone() as Arc<dyn ArtifactStore>, vec![]);
        assert!(dec.list(&tenant_scope(), None).await.is_err());
        let dec2 = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec!["artifact:tenant:read".into()],
        );
        dec2.list(&tenant_scope(), None).await.expect("granted");
    }

    #[tokio::test]
    async fn delete_requires_delete_action() {
        let ctx_uuid = Uuid::from_u128(0xbabe);
        let inner = MemStore::new();
        // tenant:write does NOT grant tenant:delete (3-segment vocab).
        let dec = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec!["artifact:tenant:write".into()],
        );
        assert!(dec.delete(&aref_with(None)).await.is_err());

        let dec2 = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec!["artifact:tenant:delete".into()],
        );
        dec2.delete(&aref_with(None)).await.expect("granted");

        // Per-context delete also wires through.
        let dec3 = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec![format!("artifact:context-{ctx_uuid}:delete")],
        );
        dec3.delete(&aref_with(Some(ctx_uuid)))
            .await
            .expect("granted");
    }

    #[tokio::test]
    async fn put_uses_context_scope_when_scope_has_context() {
        let ctx_uuid = Uuid::from_u128(0xdada);
        let inner = MemStore::new();
        let dec = ScopeCheckedArtifactStore::new(
            inner.clone() as Arc<dyn ArtifactStore>,
            vec![format!("artifact:context-{ctx_uuid}:write")],
        );
        dec.put(
            &context_scope(ctx_uuid),
            "blob",
            ArtifactBody::Bytes(Bytes::from_static(b"x")),
            meta(),
        )
        .await
        .expect("granted");
    }
}
