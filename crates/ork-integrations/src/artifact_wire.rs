//! ADR-0016: turn inline `Part::File` base64 into stored blobs and proxy `file: { uri }`
//! (used by ork-api inbound handlers and by [`crate::a2a_client::A2aRemoteAgent`] outbound).

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(test)]
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use ork_a2a::{ContextId, FileRef, MessageId, Part, TaskId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::artifact_meta_repo::{ArtifactMetaRepo, ArtifactRow};
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactScope, ArtifactStore, NO_CONTEXT_ID,
};
use ork_storage::chained::storage_key_for_chained;
use url::Url;

/// Map A2A default/nil `ContextId` to [`ArtifactScope::context_id`].
#[must_use]
pub fn scope_for(tenant_id: TenantId, context_id: ContextId) -> ArtifactScope {
    ArtifactScope {
        tenant_id,
        context_id: if context_id.0 == NO_CONTEXT_ID {
            None
        } else {
            Some(context_id)
        },
    }
}

/// Upload each `FileRef::Bytes` in `parts` and replace with `FileRef::Uri` at
/// `{public_base}/api/artifacts/…` (ork-api proxy), updating `ArtifactMeta`.
///
// ADR-0021: user-level artifact ACLs.
#[allow(clippy::too_many_arguments)]
pub async fn rewrite_inline_file_parts_to_uris(
    store: &Arc<dyn ArtifactStore>,
    meta: &Arc<dyn ArtifactMetaRepo>,
    public_base: &str,
    tenant_id: TenantId,
    context_id: ContextId,
    task_id: TaskId,
    message_id: MessageId,
    // "inbound" (API) vs "outbound" (A2A client to remote).
    name_prefix: &str,
    parts: Vec<Part>,
) -> Result<Vec<Part>, OrkError> {
    let scope = scope_for(tenant_id, context_id);
    let mut out = Vec::with_capacity(parts.len());
    for (i, p) in parts.into_iter().enumerate() {
        match p {
            Part::File {
                file:
                    FileRef::Bytes {
                        name,
                        mime_type,
                        bytes,
                    },
                metadata,
            } => {
                let raw = bytes.0;
                let data = B64
                    .decode(raw.trim())
                    .map_err(|e| OrkError::Validation(format!("file part base64: {e}")))?;
                let logical = format!("{name_prefix}/{task_id}/{message_id}_{i}");
                let am = ArtifactMeta {
                    mime: mime_type.clone(),
                    size: data.len() as u64,
                    created_at: chrono::Utc::now(),
                    created_by: None,
                    task_id: Some(task_id),
                    labels: BTreeMap::new(),
                };
                let aref = store
                    .put(&scope, &logical, ArtifactBody::Bytes(Bytes::from(data)), am)
                    .await?;
                let head = store.head(&aref).await?;
                let row = ArtifactRow {
                    tenant_id: aref.tenant_id,
                    context_id: aref.context_id,
                    name: aref.name.clone(),
                    version: aref.version,
                    scheme: aref.scheme.clone(),
                    storage_key: storage_key_for_chained(&aref),
                    mime: head.mime.clone(),
                    size: i64::try_from(head.size)
                        .map_err(|e| OrkError::Internal(format!("artifact size: {e}")))?,
                    created_at: head.created_at,
                    created_by: head.created_by.clone(),
                    task_id: head.task_id,
                    labels: serde_json::to_value(&head.labels)
                        .map_err(|e| OrkError::Internal(e.to_string()))?,
                    etag: aref.etag.clone(),
                };
                meta.upsert(&row).await?;
                let wire = aref.to_wire();
                let enc = urlencoding::encode(&wire);
                let uri = Url::parse(&format!("{public_base}/api/artifacts/{enc}"))
                    .map_err(|e| OrkError::Internal(format!("artifact url: {e}")))?;
                out.push(Part::File {
                    file: FileRef::Uri {
                        name,
                        mime_type,
                        uri,
                    },
                    metadata,
                });
            }
            o => out.push(o),
        }
    }
    Ok(out)
}

/// For tests: meta repo that only needs `upsert` for the rewrite path.
#[cfg(test)]
pub(crate) struct UpsertOnlyMeta {
    pub inner: std::sync::Mutex<Vec<ArtifactRow>>,
}

#[cfg(test)]
impl UpsertOnlyMeta {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl ArtifactMetaRepo for UpsertOnlyMeta {
    async fn upsert(&self, row: &ArtifactRow) -> Result<(), OrkError> {
        self.inner
            .lock()
            .map_err(|e| OrkError::Internal(e.to_string()))?
            .push(row.clone());
        Ok(())
    }
    async fn latest_version(
        &self,
        _tenant: TenantId,
        _context: Option<ContextId>,
        _name: &str,
    ) -> Result<Option<u32>, OrkError> {
        Ok(None)
    }
    async fn list(
        &self,
        _scope: &ArtifactScope,
        _prefix: Option<&str>,
        _label_eq: Option<(&str, &str)>,
    ) -> Result<Vec<ork_core::ports::artifact_store::ArtifactSummary>, OrkError> {
        Ok(vec![])
    }
    async fn delete_version(
        &self,
        r#ref: &ork_core::ports::artifact_store::ArtifactRef,
    ) -> Result<(), OrkError> {
        let _ = r#ref;
        Ok(())
    }
    async fn delete_all_versions(
        &self,
        _scope: &ArtifactScope,
        _name: &str,
    ) -> Result<u32, OrkError> {
        Ok(0)
    }
    async fn eligible_for_sweep(
        &self,
        _now: chrono::DateTime<chrono::Utc>,
        _default_days: u32,
        _task_days: u32,
    ) -> Result<Vec<ork_core::ports::artifact_store::ArtifactRef>, OrkError> {
        Ok(vec![])
    }
    async fn add_label(
        &self,
        r#ref: &ork_core::ports::artifact_store::ArtifactRef,
        _k: &str,
        _v: &str,
    ) -> Result<(), OrkError> {
        let _ = r#ref;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::Base64String;
    use ork_core::ports::artifact_store::NO_CONTEXT_ID;
    use ork_storage::fs::FilesystemArtifactStore;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[tokio::test]
    async fn rewrite_replaces_bytes_with_uri() {
        let dir = tempdir().expect("tempdir");
        let store: Arc<dyn ArtifactStore> = Arc::new(FilesystemArtifactStore::new(dir.path()));
        let meta: Arc<dyn ArtifactMetaRepo> = Arc::new(UpsertOnlyMeta::new());
        let tid = TenantId(Uuid::new_v4());
        let ctx = ContextId(Uuid::new_v4());
        let task = TaskId::new();
        let msg = MessageId::new();
        let parts = vec![Part::File {
            file: FileRef::Bytes {
                name: Some("a.txt".into()),
                mime_type: Some("text/plain".into()),
                bytes: Base64String("SGk=".into()), // "Hi"
            },
            metadata: None,
        }];
        let out = rewrite_inline_file_parts_to_uris(
            &store,
            &meta,
            "https://api.example",
            tid,
            ctx,
            task,
            msg,
            "outbound",
            parts,
        )
        .await
        .expect("rewrite");
        match &out[0] {
            Part::File {
                file: FileRef::Uri { uri, .. },
                ..
            } => {
                let s = uri.as_str();
                assert!(s.starts_with("https://api.example/api/artifacts/"));
            }
            _ => panic!("expected File Uri"),
        }
    }

    #[test]
    fn scope_maps_nil_uuid_to_none() {
        let t = TenantId(Uuid::new_v4());
        let s = scope_for(t, ContextId(NO_CONTEXT_ID));
        assert!(s.context_id.is_none());
    }
}
