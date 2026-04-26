//! AWS S3 / MinIO / R2 `ArtifactStore` (ADR-0016).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedMultipartUpload;
use aws_sdk_s3::types::CompletedPart;
use futures::StreamExt;
use ork_common::error::OrkError;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use url::Url;

use crate::key_prefix_with_name;

const MULTI_THRESHOLD: usize = 8 * 1024 * 1024;
const BLOB: &str = "blob";
const META: &str = "meta.json";

/// S3 / S3-compatible object store.
pub struct S3ArtifactStore {
    client: aws_sdk_s3::Client,
    bucket: String,
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
    labels: BTreeMap<String, String>,
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

    fn to_summary(&self) -> ArtifactSummary {
        ArtifactSummary {
            scheme: "s3".into(),
            name: self.name.clone(),
            version: self.version,
            mime: self.mime.clone(),
            size: self.size,
            created_at: self.created_at,
            labels: self.labels.clone(),
        }
    }
}

impl S3ArtifactStore {
    /// Build a client from region and optional custom endpoint (MinIO / R2).
    /// # Errors
    /// If the AWS SDK fails to load config.
    pub async fn new(
        bucket: impl Into<String>,
        region: impl Into<String>,
        endpoint: Option<String>,
    ) -> Result<Self, OrkError> {
        let bucket = bucket.into();
        let region = region.into();
        let base = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_types::region::Region::new(region))
            .load()
            .await;
        let mut b = aws_sdk_s3::config::Builder::from(&base);
        if let Some(ep) = endpoint {
            b = b.endpoint_url(ep).force_path_style(true);
        }
        let client = aws_sdk_s3::Client::from_conf(b.build());
        Ok(Self { client, bucket })
    }

    fn object_base(scope: &ArtifactScope, name: &str) -> String {
        key_prefix_with_name(scope, name)
    }

    fn version_blob_key(scope: &ArtifactScope, name: &str, version: u32) -> String {
        format!(
            "{}/v{ver}/{BLOB}",
            Self::object_base(scope, name),
            ver = version
        )
    }

    fn version_meta_key(scope: &ArtifactScope, name: &str, version: u32) -> String {
        format!(
            "{}/v{ver}/{META}",
            Self::object_base(scope, name),
            ver = version
        )
    }

    /// Return max existing version number (0 = none).
    async fn max_version(&self, scope: &ArtifactScope, name: &str) -> Result<u32, OrkError> {
        let base = Self::object_base(scope, name);
        let list_prefix = format!("{base}/v");
        let mut maxv = 0u32;
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&list_prefix);
            if let Some(t) = continuation.take() {
                req = req.continuation_token(t);
            }
            let out = req
                .send()
                .await
                .map_err(|e| OrkError::Internal(e.to_string()))?;
            for c in out.contents() {
                let Some(key) = c.key() else {
                    continue;
                };
                if !key.ends_with(&format!("/{BLOB}")) {
                    continue;
                }
                if let Some(rest) = key.strip_prefix(&list_prefix)
                    && let Some(ver) = rest.split('/').next().and_then(|s| s.parse::<u32>().ok())
                {
                    maxv = maxv.max(ver);
                }
            }
            if out.is_truncated() != Some(true) {
                break;
            }
            continuation = out.next_continuation_token().map(|s| s.into());
            if continuation.is_none() {
                break;
            }
        }
        Ok(maxv)
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

    async fn put_object_simple(&self, key: impl AsRef<str>, data: Vec<u8>) -> Result<(), OrkError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key.as_ref())
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| OrkError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn put_multipart(&self, key: &str, data: Vec<u8>) -> Result<(), OrkError> {
        let n = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| OrkError::Internal(e.to_string()))?;
        let upload_id = n
            .upload_id()
            .ok_or_else(|| OrkError::Internal("S3: missing upload_id".into()))?
            .to_string();
        let size = data.len();
        const PART: usize = MULTI_THRESHOLD;
        let mut parts: Vec<CompletedPart> = Vec::new();
        let mut part_n: i32 = 0;
        let mut offset = 0usize;
        while offset < size {
            part_n += 1;
            let end = (offset + PART).min(size);
            let chunk = &data[offset..end];
            let p = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(part_n)
                .body(ByteStream::from(chunk.to_vec()))
                .send()
                .await
                .map_err(|e| OrkError::Internal(e.to_string()))?;
            let etag = p
                .e_tag()
                .ok_or_else(|| OrkError::Internal("S3: missing ETag for part".into()))?
                .to_string();
            parts.push(
                CompletedPart::builder()
                    .e_tag(etag)
                    .part_number(part_n)
                    .build(),
            );
            offset = end;
        }
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| OrkError::Internal(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl ArtifactStore for S3ArtifactStore {
    fn scheme(&self) -> &'static str {
        "s3"
    }

    async fn put(
        &self,
        scope: &ArtifactScope,
        name: &str,
        body: ArtifactBody,
        mut meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError> {
        let data = Self::body_to_vec(body).await?;
        if meta.size == 0 {
            meta.size = data.len() as u64;
        }
        let ver = {
            let m = self.max_version(scope, name).await?;
            m + 1
        };
        let bkey = Self::version_blob_key(scope, name, ver);
        let mkey = Self::version_meta_key(scope, name, ver);
        if data.len() > MULTI_THRESHOLD {
            self.put_multipart(&bkey, data.clone()).await?;
        } else {
            self.put_object_simple(&bkey, data.clone()).await?;
        }
        let mj = MetaJson::from_artifact(name, ver, &meta);
        let mj_s = serde_json::to_vec(&mj).map_err(|e| OrkError::Internal(e.to_string()))?;
        self.put_object_simple(&mkey, mj_s).await?;
        let mut h = sha2::Sha256::new();
        h.update(&data);
        let etag = format!("{:x}", h.finalize());
        Ok(ArtifactRef {
            scheme: "s3".into(),
            tenant_id: scope.tenant_id,
            context_id: scope.context_id,
            name: name.to_string(),
            version: ver,
            etag,
        })
    }

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError> {
        if r#ref.scheme != "s3" {
            return Err(OrkError::Validation("wrong scheme for s3 store".into()));
        }
        let scope = ArtifactScope {
            tenant_id: r#ref.tenant_id,
            context_id: r#ref.context_id,
        };
        let ver = if r#ref.version == 0 {
            let m = self.max_version(&scope, &r#ref.name).await?;
            if m == 0 {
                return Err(OrkError::NotFound("no versions in S3".into()));
            }
            m
        } else {
            r#ref.version
        };
        let k = S3ArtifactStore::version_blob_key(&scope, &r#ref.name, ver);
        let o = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&k)
            .send()
            .await
            .map_err(|e| OrkError::NotFound(format!("S3 get: {e}")))?;
        let b = s3_read_body(o.body).await.map_err(OrkError::Internal)?;
        Ok(ArtifactBody::Bytes(b.into()))
    }

    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError> {
        if r#ref.scheme != "s3" {
            return Err(OrkError::Validation("wrong scheme for s3 store".into()));
        }
        let scope = ArtifactScope {
            tenant_id: r#ref.tenant_id,
            context_id: r#ref.context_id,
        };
        let ver = if r#ref.version == 0 {
            let m = self.max_version(&scope, &r#ref.name).await?;
            if m == 0 {
                return Err(OrkError::NotFound("no versions in S3".into()));
            }
            m
        } else {
            r#ref.version
        };
        let k = S3ArtifactStore::version_meta_key(&scope, &r#ref.name, ver);
        let o = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&k)
            .send()
            .await
            .map_err(|e| OrkError::NotFound(format!("S3 meta: {e}")))?;
        let s = s3_read_body(o.body).await.map_err(OrkError::Internal)?;
        let m: MetaJson =
            serde_json::from_slice(&s).map_err(|e| OrkError::Internal(e.to_string()))?;
        Ok(m.to_meta())
    }

    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError> {
        let pfx = prefix.unwrap_or("");
        let start = key_prefix_with_name(scope, pfx);
        let list_prefix = format!("{start}/");
        let mut rows: Vec<ArtifactSummary> = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&list_prefix);
            if let Some(t) = continuation.take() {
                req = req.continuation_token(t);
            }
            let out = req
                .send()
                .await
                .map_err(|e| OrkError::Internal(e.to_string()))?;
            for c in out.contents() {
                let key = c.key().unwrap_or("");
                if !key.ends_with(&format!("/{META}")) {
                    continue;
                }
                let o = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|e| OrkError::Internal(e.to_string()))?;
                let s = s3_read_body(o.body).await.map_err(OrkError::Internal)?;
                let m: MetaJson =
                    serde_json::from_slice(&s).map_err(|e| OrkError::Internal(e.to_string()))?;
                if let Some(p) = prefix
                    && !m.name.starts_with(p)
                {
                    continue;
                }
                rows.push(m.to_summary());
            }
            if out.is_truncated() != Some(true) {
                break;
            }
            continuation = out.next_continuation_token().map(|s| s.into());
            if continuation.is_none() {
                break;
            }
        }
        Ok(rows)
    }

    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError> {
        if r#ref.scheme != "s3" {
            return Err(OrkError::Validation("wrong scheme for s3 store".into()));
        }
        let scope = ArtifactScope {
            tenant_id: r#ref.tenant_id,
            context_id: r#ref.context_id,
        };
        let ver = if r#ref.version == 0 {
            let m = self.max_version(&scope, &r#ref.name).await?;
            if m == 0 {
                return Err(OrkError::NotFound("no versions in S3".into()));
            }
            m
        } else {
            r#ref.version
        };
        for k in [
            Self::version_blob_key(&scope, &r#ref.name, ver),
            Self::version_meta_key(&scope, &r#ref.name, ver),
        ] {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(&k)
                .send()
                .await
                .map_err(|e| OrkError::Internal(e.to_string()))?;
        }
        Ok(())
    }

    async fn presign_get(
        &self,
        r#ref: &ArtifactRef,
        ttl: Duration,
    ) -> Result<Option<Url>, OrkError> {
        if r#ref.scheme != "s3" {
            return Ok(None);
        }
        let scope = ArtifactScope {
            tenant_id: r#ref.tenant_id,
            context_id: r#ref.context_id,
        };
        let ver = if r#ref.version == 0 {
            let m = self.max_version(&scope, &r#ref.name).await?;
            if m == 0 {
                return Ok(None);
            }
            m
        } else {
            r#ref.version
        };
        let k = S3ArtifactStore::version_blob_key(&scope, &r#ref.name, ver);
        use aws_sdk_s3::presigning::PresigningConfig;
        let pres =
            PresigningConfig::expires_in(ttl).map_err(|e| OrkError::Internal(e.to_string()))?;
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&k)
            .presigned(pres)
            .await
            .map_err(|e| OrkError::Internal(e.to_string()))?;
        let u = out.uri().to_string();
        let url = Url::parse(&u).map_err(|e| OrkError::Internal(e.to_string()))?;
        Ok(Some(url))
    }
}

async fn s3_read_body(b: aws_sdk_s3::primitives::ByteStream) -> Result<Vec<u8>, String> {
    let mut s = b;
    let mut v = Vec::new();
    while let Some(n) = s.next().await {
        let c = n.map_err(|e| e.to_string())?;
        v.extend_from_slice(&c);
    }
    Ok(v)
}
