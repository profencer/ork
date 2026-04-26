//! Local filesystem `ArtifactStore` (ADR-0016).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::StreamExt;
use ork_common::error::OrkError;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::time::Duration;
use url::Url;

const META_FILE: &str = "meta.json";
const BLOB: &str = "blob";

/// On-disk `ArtifactStore` for single-node and dev. Layout:  
/// `<root>/<tenant_id>/<context_uuid>/<name…>/v<version>/{`blob` | `meta.json`}`.
pub struct FilesystemArtifactStore {
    root: PathBuf,
}

impl FilesystemArtifactStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    fn base_for(&self, scope: &ArtifactScope, name: &str) -> Result<PathBuf, OrkError> {
        validate_logical_name(name)?;
        let mut p = self
            .root
            .join(scope.tenant_id.0.to_string())
            .join(scope.context_key_uuid().to_string());
        for seg in name.split('/') {
            p = p.join(seg);
        }
        Ok(p)
    }

    fn next_version(artifact_dir: &Path) -> Result<u32, OrkError> {
        if !artifact_dir.exists() {
            return Ok(1);
        }
        let mut maxv = 0u32;
        for e in std::fs::read_dir(artifact_dir)
            .map_err(|e| OrkError::Internal(format!("fs list versions: {e}")))?
        {
            let e = e.map_err(|e| OrkError::Internal(e.to_string()))?;
            if let Some(s) = e.file_name().to_str()
                && let Some(n) = s.strip_prefix('v')
                && let Ok(x) = n.parse::<u32>()
            {
                maxv = maxv.max(x);
            }
        }
        Ok(maxv.saturating_add(1).max(1))
    }

    fn resolve_read_version(artifact_dir: &Path, version: u32) -> Result<u32, OrkError> {
        if version != 0 {
            return Ok(version);
        }
        let mut maxv = 0u32;
        if !artifact_dir.exists() {
            return Err(OrkError::NotFound("artifact not found".into()));
        }
        for e in std::fs::read_dir(artifact_dir).map_err(|e| OrkError::Internal(e.to_string()))? {
            let e = e.map_err(|e| OrkError::Internal(e.to_string()))?;
            if let Some(s) = e.file_name().to_str()
                && let Some(n) = s.strip_prefix('v')
                && let Ok(x) = n.parse::<u32>()
            {
                maxv = maxv.max(x);
            }
        }
        if maxv == 0 {
            return Err(OrkError::NotFound("no versions".into()));
        }
        Ok(maxv)
    }
}

/// Logical name: non-empty, no `..` segments, no empty segments.
fn validate_logical_name(name: &str) -> Result<(), OrkError> {
    if name.is_empty() {
        return Err(OrkError::Validation("empty artifact name".into()));
    }
    for p in name.split('/') {
        if p.is_empty() || p == ".." {
            return Err(OrkError::Validation("invalid artifact name path".into()));
        }
    }
    Ok(())
}

async fn body_to_vec(body: ArtifactBody) -> Result<Vec<u8>, OrkError> {
    match body {
        ArtifactBody::Bytes(b) => Ok(b.to_vec()),
        ArtifactBody::Stream(mut s) => {
            let mut v = Vec::new();
            while let Some(c) = s.next().await {
                v.extend_from_slice(&c?);
            }
            Ok(v)
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct MetaJson {
    name: String,
    version: u32,
    mime: Option<String>,
    size: u64,
    created_at: chrono::DateTime<chrono::Utc>,
    created_by: Option<String>,
    task_id: Option<uuid::Uuid>,
    labels: std::collections::BTreeMap<String, String>,
}

impl MetaJson {
    fn from_artifact(name: &str, version: u32, m: &ArtifactMeta) -> Self {
        Self {
            name: name.to_string(),
            version,
            mime: m.mime.clone(),
            size: m.size,
            created_at: m.created_at,
            created_by: m.created_by.clone(),
            task_id: m.task_id.map(|t| t.0),
            labels: m.labels.clone(),
        }
    }

    fn to_meta(&self) -> ArtifactMeta {
        ArtifactMeta {
            mime: self.mime.clone(),
            size: self.size,
            created_at: self.created_at,
            created_by: self.created_by.clone(),
            task_id: self.task_id.map(ork_a2a::TaskId),
            labels: self.labels.clone(),
        }
    }
}

#[async_trait]
impl ArtifactStore for FilesystemArtifactStore {
    fn scheme(&self) -> &'static str {
        "fs"
    }

    async fn put(
        &self,
        scope: &ArtifactScope,
        name: &str,
        body: ArtifactBody,
        mut meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        if scope.tenant_id.0 == uuid::Uuid::nil() {
            return Err(OrkError::Validation("tenant_id required".into()));
        }
        let data = body_to_vec(body).await?;
        let len = data.len() as u64;
        if meta.size == 0 {
            meta.size = len;
        }
        let base = self.base_for(scope, name)?;
        let ver = Self::next_version(&base)?;
        let vdir = base.join(format!("v{ver}"));
        std::fs::create_dir_all(&vdir).map_err(|e| OrkError::Internal(format!("fs mkdir: {e}")))?;
        let meta_json = MetaJson::from_artifact(name, ver, &meta);
        let blob_path = vdir.join(BLOB);
        std::fs::write(&blob_path, &data)
            .map_err(|e| OrkError::Internal(format!("fs write: {e}")))?;
        let mj = serde_json::to_string_pretty(&meta_json)
            .map_err(|e| OrkError::Internal(e.to_string()))?;
        std::fs::write(vdir.join(META_FILE), mj.as_bytes())
            .map_err(|e| OrkError::Internal(e.to_string()))?;
        let mut h = sha2::Sha256::new();
        h.update(&data);
        let etag = format!("{:x}", h.finalize());
        Ok(ArtifactRef {
            scheme: "fs".into(),
            tenant_id: scope.tenant_id,
            context_id: scope.context_id,
            name: name.to_string(),
            version: ver,
            etag,
        })
    }

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        if r#ref.scheme != "fs" {
            return Err(OrkError::Validation("wrong scheme for fs store".into()));
        }
        let base = self.base_for(
            &ArtifactScope {
                tenant_id: r#ref.tenant_id,
                context_id: r#ref.context_id,
            },
            &r#ref.name,
        )?;
        let ver = Self::resolve_read_version(&base, r#ref.version)?;
        let blob = base.join(format!("v{ver}")).join(BLOB);
        let b = std::fs::read(&blob)
            .map_err(|_| OrkError::NotFound(format!("no blob for {}", r#ref.name)))?;
        Ok(ArtifactBody::Bytes(b.into()))
    }

    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        if r#ref.scheme != "fs" {
            return Err(OrkError::Validation("wrong scheme for fs store".into()));
        }
        let base = self.base_for(
            &ArtifactScope {
                tenant_id: r#ref.tenant_id,
                context_id: r#ref.context_id,
            },
            &r#ref.name,
        )?;
        let ver = Self::resolve_read_version(&base, r#ref.version)?;
        let meta_path = base.join(format!("v{ver}")).join(META_FILE);
        let s = std::fs::read_to_string(&meta_path)
            .map_err(|_| OrkError::NotFound("meta.json missing".into()))?;
        let m: MetaJson =
            serde_json::from_str(&s).map_err(|e| OrkError::Internal(e.to_string()))?;
        Ok(m.to_meta())
    }

    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        let p = self
            .root
            .join(scope.tenant_id.0.to_string())
            .join(scope.context_key_uuid().to_string());
        if !p.exists() {
            return Ok(vec![]);
        }
        let mut out: Vec<ArtifactSummary> = vec![];
        list_under(&p, &mut out, prefix)?;
        Ok(out)
    }

    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        if r#ref.scheme != "fs" {
            return Err(OrkError::Validation("wrong scheme for fs store".into()));
        }
        let base = self.base_for(
            &ArtifactScope {
                tenant_id: r#ref.tenant_id,
                context_id: r#ref.context_id,
            },
            &r#ref.name,
        )?;
        let ver = if r#ref.version == 0 {
            Self::resolve_read_version(&base, 0)?
        } else {
            r#ref.version
        };
        let vdir = base.join(format!("v{ver}"));
        if vdir.exists() {
            std::fs::remove_dir_all(&vdir).map_err(|e| OrkError::Internal(e.to_string()))?;
        } else {
            return Err(OrkError::NotFound("version not found".into()));
        }
        Ok(())
    }

    async fn presign_get(
        &self,
        r#ref: &ArtifactRef,
        _ttl: Duration,
    ) -> Result<Option<Url>, OrkError> {
        let _ = r#ref;
        Ok(None)
    }
}

fn list_under(
    current: &Path,
    out: &mut Vec<ArtifactSummary>,
    prefix: Option<&str>,
) -> Result<(), OrkError> {
    for e in std::fs::read_dir(current).map_err(|e| OrkError::Internal(e.to_string()))? {
        let e = e.map_err(|e| OrkError::Internal(e.to_string()))?;
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let dname = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
        if dname.len() > 1 && dname.starts_with('v') && dname[1..].parse::<u32>().is_ok() {
            let meta_path = path.join(META_FILE);
            if let Ok(s) = std::fs::read_to_string(&meta_path)
                && let Ok(m) = serde_json::from_str::<MetaJson>(&s)
            {
                let pass = match prefix {
                    None => true,
                    Some(p) => m.name.starts_with(p),
                };
                if pass {
                    out.push(ArtifactSummary {
                        scheme: "fs".into(),
                        name: m.name,
                        version: m.version,
                        mime: m.mime.clone(),
                        size: m.size,
                        created_at: m.created_at,
                        labels: m.labels.clone(),
                    });
                }
            }
        } else {
            list_under(&path, out, prefix)?;
        }
    }
    Ok(())
}
