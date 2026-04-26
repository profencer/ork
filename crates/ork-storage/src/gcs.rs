//! GCS placeholder (ADR-0016). Real client ships in a follow-up ADR.

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use std::time::Duration;
use url::Url;

/// Placeholder; every method returns [`OrkError::Unsupported`].
pub struct GcsStub;

#[async_trait]
impl ArtifactStore for GcsStub {
    fn scheme(&self) -> &'static str {
        "gcs"
    }

    async fn put(
        &self,
        _scope: &ArtifactScope,
        _name: &str,
        _body: ArtifactBody,
        _meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        Err(OrkError::Unsupported(
            "GCS backend not yet implemented (ADR-0016 stub)".into(),
        ))
    }

    async fn get(&self, _aref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        Err(OrkError::Unsupported(
            "GCS backend not yet implemented (ADR-0016 stub)".into(),
        ))
    }

    async fn head(&self, _aref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        Err(OrkError::Unsupported(
            "GCS backend not yet implemented (ADR-0016 stub)".into(),
        ))
    }

    async fn list(
        &self,
        _scope: &ArtifactScope,
        _prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        Err(OrkError::Unsupported(
            "GCS backend not yet implemented (ADR-0016 stub)".into(),
        ))
    }

    async fn delete(&self, _aref: &ArtifactRef) -> Result<(), OrkError> {
        Err(OrkError::Unsupported(
            "GCS backend not yet implemented (ADR-0016 stub)".into(),
        ))
    }

    async fn presign_get(
        &self,
        _aref: &ArtifactRef,
        _ttl: Duration,
    ) -> Result<Option<Url>, OrkError> {
        Err(OrkError::Unsupported(
            "GCS backend not yet implemented (ADR-0016 stub)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ork_common::types::TenantId;
    use ork_core::ports::artifact_store::ArtifactStore;
    use uuid::Uuid;

    #[tokio::test]
    async fn stub_put_is_unsupported() {
        let g = GcsStub;
        let scope = ArtifactScope {
            tenant_id: TenantId(Uuid::nil()),
            context_id: None,
        };
        let err = g
            .put(
                &scope,
                "x",
                ArtifactBody::Bytes(Bytes::new()),
                ArtifactMeta::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OrkError::Unsupported(_)));
    }
}
