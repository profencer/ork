//! ADR-0016: store oversized text/JSON in [`crate::ports::artifact_store::ArtifactStore`]
//! and surface it as a `Part::File` with URI (presign or API proxy).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use ork_a2a::Part;
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use url::Url;

use crate::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore,
};

/// Build a proxy `GET {base}/api/artifacts/…` URL (wire segment path-encoded).
pub fn proxy_artifact_url(base: &str, r: &ArtifactRef) -> Result<Url, OrkError> {
    let path = r.to_wire();
    let enc = urlencoding::encode(&path);
    Url::parse(&format!(
        "{}/api/artifacts/{}",
        base.trim_end_matches('/'),
        enc.as_ref()
    ))
    .map_err(|e| OrkError::Internal(format!("artifact url: {e}")))
}

/// Store `bytes` and return the [`ArtifactRef`] plus a `Part::file` with presign or proxy URI.
pub async fn spill_bytes_to_artifact(
    store: &Arc<dyn ArtifactStore>,
    public_base: Option<&str>,
    scope: &ArtifactScope,
    logical_name: &str,
    bytes: Bytes,
    mime: Option<String>,
    task_id: Option<TaskId>,
) -> Result<(ArtifactRef, Part), OrkError> {
    let n = bytes.len() as u64;
    let meta = ArtifactMeta {
        mime: mime.clone(),
        size: n,
        created_at: Utc::now(),
        created_by: None,
        task_id,
        labels: BTreeMap::new(),
    };
    let aref = store
        .put(scope, logical_name, ArtifactBody::Bytes(bytes), meta)
        .await?;
    if let Some(u) = store.presign_get(&aref, Duration::from_secs(3600)).await? {
        return Ok((aref, Part::file_uri(u, mime)));
    }
    let base = public_base.ok_or_else(|| {
        OrkError::Internal(
            "artifact spill needs presign_get or artifact_public_base (configure API + workflow engine)"
                .into(),
        )
    })?;
    let u = proxy_artifact_url(base, &aref)?;
    Ok((aref, Part::file_uri(u, mime)))
}

/// Store `bytes` at `scope` / `logical_name` and return `Part::file_uri` (presign when available).
pub async fn spill_bytes_to_file_part(
    store: &Arc<dyn ArtifactStore>,
    public_base: Option<&str>,
    scope: &ArtifactScope,
    logical_name: &str,
    bytes: Bytes,
    mime: Option<String>,
    task_id: Option<TaskId>,
) -> Result<Part, OrkError> {
    spill_bytes_to_artifact(
        store,
        public_base,
        scope,
        logical_name,
        bytes,
        mime,
        task_id,
    )
    .await
    .map(|(_, p)| p)
}
