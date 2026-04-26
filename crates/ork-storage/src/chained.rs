//! Route artifact names to backends: `s3:path/to` vs default store (ADR-0016).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};

use crate::key_prefix_with_name;
use std::time::Duration;
use url::Url;

/// Split `s3:foo/bar` into `Some("s3")` and `foo/bar`. If there is no leading
/// `scheme:` or the part before `:` is not a well-formed scheme id, the whole
/// string is a logical name for the default backend.
pub fn split_scheme_name(name: &str) -> (Option<&str>, &str) {
    let Some((head, rest)) = name.split_once(':') else {
        return (None, name);
    };
    if head.is_empty() {
        return (None, name);
    }
    if !head
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return (None, name);
    }
    (Some(head), rest)
}

/// Backends by scheme name + a default.
pub struct ChainedArtifactStore {
    by_scheme: HashMap<String, Arc<dyn ArtifactStore>>,
    default: Arc<dyn ArtifactStore>,
}

impl ChainedArtifactStore {
    /// Build a store. The `other` set must not contain the `default` scheme.
    /// # Errors
    /// Duplicate or empty scheme, or a backend whose `scheme()` collides.
    pub fn new(
        default: Arc<dyn ArtifactStore>,
        other: impl IntoIterator<Item = Arc<dyn ArtifactStore>>,
    ) -> Result<Self, OrkError> {
        let def_key = default.scheme().to_string();
        let mut by_scheme = HashMap::new();
        for s in other {
            let k = s.scheme().to_string();
            if k == def_key {
                return Err(OrkError::Validation(format!(
                    "chained: duplicate default scheme `{k}`"
                )));
            }
            if by_scheme.insert(k, s).is_some() {
                return Err(OrkError::Validation("chained: duplicate scheme".into()));
            }
        }
        Ok(Self { by_scheme, default })
    }

    fn resolve(&self, scheme: &str) -> Option<&Arc<dyn ArtifactStore>> {
        self.by_scheme.get(scheme)
    }
}

#[async_trait]
impl ArtifactStore for ChainedArtifactStore {
    fn scheme(&self) -> &'static str {
        "chained"
    }

    async fn put(
        &self,
        scope: &ArtifactScope,
        name: &str,
        body: ArtifactBody,
        meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        let (maybe_scheme, subname) = split_scheme_name(name);
        let backend = if let Some(sc) = maybe_scheme {
            self.resolve(sc).ok_or_else(|| {
                OrkError::Validation(format!("unknown artifact scheme `{sc}` in name `{name}`"))
            })?
        } else {
            &self.default
        };
        let inner_name = if maybe_scheme.is_some() {
            subname
        } else {
            name
        };
        backend.put(scope, inner_name, body, meta).await
    }

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        if let Some(b) = self.resolve(&r#ref.scheme) {
            return b.get(r#ref).await;
        }
        if r#ref.scheme == self.default.scheme() {
            return self.default.get(r#ref).await;
        }
        Err(OrkError::NotFound(format!(
            "no backend for scheme `{}`",
            r#ref.scheme
        )))
    }

    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        if let Some(b) = self.resolve(&r#ref.scheme) {
            return b.head(r#ref).await;
        }
        if r#ref.scheme == self.default.scheme() {
            return self.default.head(r#ref).await;
        }
        Err(OrkError::NotFound(format!(
            "no backend for scheme `{}`",
            r#ref.scheme
        )))
    }

    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        // Union (tool uses Postgres index; this is best-effort for generic callers).
        let mut all = self.default.list(scope, prefix).await?;
        for b in self.by_scheme.values() {
            let v = b.list(scope, prefix).await?;
            all.extend(v);
        }
        Ok(all)
    }

    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        if let Some(b) = self.resolve(&r#ref.scheme) {
            return b.delete(r#ref).await;
        }
        if r#ref.scheme == self.default.scheme() {
            return self.default.delete(r#ref).await;
        }
        Err(OrkError::NotFound(format!(
            "no backend for scheme `{}`",
            r#ref.scheme
        )))
    }

    async fn presign_get(
        &self,
        r#ref: &ArtifactRef,
        ttl: Duration,
    ) -> Result<Option<Url>, OrkError> {
        if let Some(b) = self.resolve(&r#ref.scheme) {
            return b.presign_get(r#ref, ttl).await;
        }
        if r#ref.scheme == self.default.scheme() {
            return self.default.presign_get(r#ref, ttl).await;
        }
        Ok(None)
    }
}

/// Primary row key (column `storage_key`) for persistence: the inner backend key.
#[must_use]
pub fn storage_key_for_chained(r#ref: &ArtifactRef) -> String {
    key_prefix_with_name(
        &ArtifactScope {
            tenant_id: r#ref.tenant_id,
            context_id: r#ref.context_id,
        },
        &format!("{}/v{}", r#ref.name, r#ref.version),
    )
}
