//! ADR-0016 — artifact blob store port (`ArtifactStore`).

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use ork_a2a::{ContextId, TaskId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use url::Url;
use uuid::Uuid;

use crate::a2a::AgentId;

/// When no conversation scope applies, the wire format and the DB use the nil
/// `context_id` (matches ADR-0016 `COALESCE` on the `artifacts` table).
pub const NO_CONTEXT_ID: Uuid = Uuid::from_bytes([0; 16]);

/// Tenant + optional A2A context that scopes an artifact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArtifactScope {
    pub tenant_id: TenantId,
    /// `None` = tenant-wide (backed by [`NO_CONTEXT_ID`] in persistence).
    pub context_id: Option<ContextId>,
}

impl ArtifactScope {
    #[must_use]
    pub fn context_key_uuid(&self) -> Uuid {
        self.context_id.map(|c| c.0).unwrap_or(NO_CONTEXT_ID)
    }
}

/// Address for one version; `version = 0` is an alias for “latest” on read paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRef {
    pub scheme: String,
    pub tenant_id: TenantId,
    pub context_id: Option<ContextId>,
    /// Logical object name. Chained routing can use a `"s3:sub/path.txt"` name.
    pub name: String,
    /// `0` = “latest” for reads. Writers return a monotonically increasing `>= 1`.
    pub version: u32,
    /// Content digest (e.g. SHA-256 hex of object bytes).
    pub etag: String,
}

/// Metadata stored alongside a blob; mirrors `ArtifactMeta` in ADR-0016.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ArtifactMeta {
    pub mime: Option<String>,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub labels: BTreeMap<String, String>,
}

/// One line in `list_artifacts` / index listing.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ArtifactSummary {
    /// Storage scheme (`fs`, `s3`, …) — needed to construct an [`ArtifactRef`]
    /// for `load` / `head` on multi-backend installs.
    pub scheme: String,
    pub name: String,
    pub version: u32,
    pub mime: Option<String>,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub labels: BTreeMap<String, String>,
}

/// Inbound body: bytes or a stream of chunks.
pub enum ArtifactBody {
    Bytes(Bytes),
    Stream(BoxStream<'static, Result<Bytes, OrkError>>),
}

/// Wire: `"{scheme}:{tenant_id}/{context_uuid|nil}/{name}/v{version}"`  
/// The `name` may contain unescaped `/`; the parser takes everything after the
/// second `/` and before the final `/v{n}`.
impl ArtifactRef {
    #[must_use]
    pub fn context_key_uuid(&self) -> Uuid {
        self.context_id.map(|c| c.0).unwrap_or(NO_CONTEXT_ID)
    }

    /// ADR-0016 wire string (used by proxy path segments, tool spillover JSON, etc.).
    #[must_use]
    pub fn to_wire(&self) -> String {
        let ctx = self.context_key_uuid();
        format!(
            "{}:{}/{}/{}/v{}",
            self.scheme, self.tenant_id.0, ctx, self.name, self.version
        )
    }

    /// Parse [`to_wire`] output.
    pub fn parse(s: &str) -> Result<Self, OrkError> {
        let (scheme, path) = s
            .split_once(':')
            .ok_or_else(|| OrkError::Validation("artifact ref: missing ':'".into()))?;
        let scheme = scheme.to_string();
        let (prefix, ver_part) = path.rsplit_once("/v").ok_or_else(|| {
            OrkError::Validation("artifact ref: expected /v{version} suffix".into())
        })?;
        let version: u32 = ver_part
            .parse()
            .map_err(|_| OrkError::Validation("artifact ref: bad version".into()))?;
        let parts: Vec<&str> = prefix.split('/').collect();
        if parts.len() < 3 {
            return Err(OrkError::Validation(
                "artifact ref: expected scheme:tenant/context/name.../vN".into(),
            ));
        }
        let tenant_id = TenantId(
            Uuid::parse_str(parts[0])
                .map_err(|e| OrkError::Validation(format!("artifact ref: bad tenant id: {e}")))?,
        );
        let ctx_u = Uuid::parse_str(parts[1])
            .map_err(|e| OrkError::Validation(format!("artifact ref: bad context id: {e}")))?;
        let context_id = if ctx_u == NO_CONTEXT_ID {
            None
        } else {
            Some(ContextId(ctx_u))
        };
        let name = parts[2..].join("/");
        if name.is_empty() {
            return Err(OrkError::Validation(
                "artifact ref: empty logical name".into(),
            ));
        }
        Ok(Self {
            scheme,
            tenant_id,
            context_id,
            name,
            version,
            etag: String::new(),
        })
    }
}

impl FromStr for ArtifactRef {
    type Err = OrkError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Storage scheme prefix this store handles (e.g. `"fs"`, `"s3"`).
    fn scheme(&self) -> &'static str;

    async fn put(
        &self,
        scope: &ArtifactScope,
        name: &str,
        body: ArtifactBody,
        meta: ArtifactMeta,
    ) -> Result<ArtifactRef, OrkError>;

    async fn get(&self, r#ref: &ArtifactRef) -> Result<ArtifactBody, OrkError>;
    async fn head(&self, r#ref: &ArtifactRef) -> Result<ArtifactMeta, OrkError>;
    async fn list(
        &self,
        scope: &ArtifactScope,
        prefix: Option<&str>,
    ) -> Result<Vec<ArtifactSummary>, OrkError>;
    async fn delete(&self, r#ref: &ArtifactRef) -> Result<(), OrkError>;

    /// Pre-signed download URL for clients that should fetch directly (S3 / GCS).
    /// Default impl returns `None` (force proxying through `ork-api`).
    async fn presign_get(
        &self,
        r#ref: &ArtifactRef,
        ttl: Duration,
    ) -> Result<Option<Url>, OrkError> {
        let _ = (r#ref, ttl);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip() {
        let t = Uuid::new_v4();
        let r = ArtifactRef {
            scheme: "fs".into(),
            tenant_id: TenantId(t),
            context_id: None,
            name: "report.pdf".into(),
            version: 3,
            etag: "abc".into(),
        };
        let w = r.to_wire();
        let p = ArtifactRef::parse(&w).expect("parse");
        assert_eq!(p.scheme, "fs");
        assert_eq!(p.tenant_id.0, t);
        assert_eq!(p.context_id, None);
        assert_eq!(p.name, "report.pdf");
        assert_eq!(p.version, 3);
    }

    #[test]
    fn wire_round_trip_name_with_slash() {
        let t = Uuid::new_v4();
        let r = ArtifactRef {
            scheme: "s3".into(),
            tenant_id: TenantId(t),
            context_id: None,
            name: "sub/report.pdf".into(),
            version: 1,
            etag: String::new(),
        };
        let w = r.to_wire();
        let p = ArtifactRef::parse(&w).expect("parse");
        assert_eq!(p.name, "sub/report.pdf");
    }
}
