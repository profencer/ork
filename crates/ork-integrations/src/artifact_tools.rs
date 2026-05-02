//! ADR-0016 built-in `artifact_*` tools (SAM-compatible names).
//!
// ADR-0021: add scope checks (user scopes) in addition to tenant isolation from context.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use futures::StreamExt;
use ork_a2a::Part;
use ork_common::error::OrkError;
use ork_core::a2a::AgentContext;
use ork_core::ports::artifact_meta_repo::{ArtifactMetaRepo, ArtifactRow};
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore, ArtifactSummary,
};
use ork_core::workflow::engine::ToolExecutor;
use ork_storage::chained::storage_key_for_chained;
use url::Url;
use uuid::Uuid;

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

/// Reject a leading path segment that looks like a different tenant id (ADR-0016 isolation).
fn reject_cross_tenant_name(
    name: &str,
    tenant: ork_common::types::TenantId,
) -> Result<(), OrkError> {
    if let Some(first) = name.split('/').next()
        && let Ok(u) = Uuid::parse_str(first)
        && u != tenant.0
    {
        return Err(OrkError::Forbidden(
            "artifact name may not be prefixed with another tenant id".into(),
        ));
    }
    Ok(())
}

fn scope_from_ctx(ctx: &AgentContext) -> ArtifactScope {
    ArtifactScope {
        tenant_id: ctx.tenant_id,
        context_id: ctx.context_id,
    }
}

fn decode_data(data: &str) -> Result<Vec<u8>, OrkError> {
    let t = data.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(b) = B64.decode(t) {
        return Ok(b);
    }
    Ok(t.as_bytes().to_vec())
}

async fn read_body_to_vec(body: ArtifactBody) -> Result<Vec<u8>, OrkError> {
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

fn build_ref(scheme: &str, scope: &ArtifactScope, name: &str, version: u32) -> ArtifactRef {
    ArtifactRef {
        scheme: scheme.into(),
        tenant_id: scope.tenant_id,
        context_id: scope.context_id,
        name: name.to_string(),
        version,
        etag: String::new(),
    }
}

/// Executes ADR-0016 artifact tool calls: [`ArtifactStore`] + [`ArtifactMetaRepo`].
pub struct ArtifactToolExecutor {
    store: Arc<dyn ArtifactStore>,
    meta: Arc<dyn ArtifactMetaRepo>,
    /// e.g. `https://api.example` — for [`load_artifact`] when `presign_get` is unavailable.
    public_api_base: Option<String>,
}

impl ArtifactToolExecutor {
    /// Wire blob store, Postgres (or in-memory) index, and the public ork API base (no path).
    #[must_use]
    pub fn new(
        store: Arc<dyn ArtifactStore>,
        meta: Arc<dyn ArtifactMetaRepo>,
        public_api_base: Option<String>,
    ) -> Self {
        Self {
            store,
            meta,
            public_api_base,
        }
    }

    async fn upsert_index_row(
        &self,
        aref: &ArtifactRef,
        head: &ArtifactMeta,
    ) -> Result<(), OrkError> {
        let row = ArtifactRow {
            tenant_id: aref.tenant_id,
            context_id: aref.context_id,
            name: aref.name.clone(),
            version: aref.version,
            scheme: aref.scheme.clone(),
            storage_key: storage_key_for_chained(aref),
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
        self.meta.upsert(&row).await
    }

    /// Fetch scheme + version for one row from the index (exact `name` match).
    /// Build the JSON tool result for an already-loaded object body.
    fn part_from_loaded_bytes(
        bytes: Vec<u8>,
        mime: &Option<String>,
    ) -> Result<serde_json::Value, OrkError> {
        if let Some(m) = mime
            && (m == "application/json" || m.ends_with("+json"))
        {
            let j: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| OrkError::Internal(format!("json artifact: {e}")))?;
            return Ok(serde_json::json!({ "part": Part::data(j) }));
        }
        if let Ok(text) = std::str::from_utf8(&bytes) {
            return Ok(serde_json::json!({ "part": Part::text(text) }));
        }
        Err(OrkError::Internal(
            "load_artifact: non-utf8 data needs presign or API base for binary".into(),
        ))
    }

    async fn find_summary_for(
        &self,
        scope: &ArtifactScope,
        name: &str,
        want_version: u32,
    ) -> Result<ArtifactSummary, OrkError> {
        let rows = self
            .meta
            .list(scope, if name.is_empty() { None } else { Some(name) }, None)
            .await?;
        let mut v = want_version;
        if v == 0 {
            v = rows
                .iter()
                .filter(|r| r.name == name)
                .map(|r| r.version)
                .max()
                .ok_or_else(|| OrkError::NotFound(format!("no artifact named {name}")))?;
        }
        rows.into_iter()
            .find(|r| r.name == name && r.version == v)
            .ok_or_else(|| OrkError::NotFound(format!("artifact {name} v{v} not found")))
    }

    async fn create_artifact(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("name (string) required".into()))?;
        validate_logical_name(name)?;
        reject_cross_tenant_name(name, ctx.tenant_id)?;
        let data = input["data"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("data (string) required".into()))?;
        let bytes = decode_data(data)?;
        let mime = input["mime"].as_str().map(std::string::ToString::to_string);
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        if let Some(obj) = input["labels"].as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    labels.insert(k.clone(), s.to_string());
                } else {
                    labels.insert(k.clone(), v.to_string());
                }
            }
        }
        let scope = scope_from_ctx(ctx);
        let mut meta = ArtifactMeta {
            mime: mime.clone(),
            size: bytes.len() as u64,
            created_at: chrono::Utc::now(),
            created_by: None,
            task_id: Some(ctx.task_id),
            labels,
        };
        if meta.size == 0 {
            meta.size = bytes.len() as u64;
        }
        let aref = self
            .store
            .put(&scope, name, ArtifactBody::Bytes(Bytes::from(bytes)), meta)
            .await?;
        let head = self.store.head(&aref).await?;
        self.upsert_index_row(&aref, &head).await?;
        Ok(serde_json::json!({
            "ref": aref.to_wire(),
            "version": aref.version
        }))
    }

    async fn append_artifact(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("name (string) required".into()))?;
        validate_logical_name(name)?;
        reject_cross_tenant_name(name, ctx.tenant_id)?;
        let data = input["data"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("data (string) required".into()))?;
        let append = decode_data(data)?;
        let scope = scope_from_ctx(ctx);
        let v = self
            .meta
            .latest_version(ctx.tenant_id, ctx.context_id, name)
            .await?
            .ok_or_else(|| {
                OrkError::NotFound(format!("no prior version of {name} to append to"))
            })?;
        let s = self.find_summary_for(&scope, name, v).await?;
        let aref = build_ref(s.scheme.as_str(), &scope, name, v);
        let head_m = self.store.head(&aref).await?;
        let old = read_body_to_vec(self.store.get(&aref).await?).await?;
        let mut new_data = old;
        new_data.extend_from_slice(&append);
        let mime = input["mime"]
            .as_str()
            .map_or(head_m.mime.clone(), |m| Some(m.to_string()));
        let mut meta = ArtifactMeta {
            mime,
            size: new_data.len() as u64,
            created_at: chrono::Utc::now(),
            created_by: None,
            task_id: Some(ctx.task_id),
            labels: head_m.labels,
        };
        if meta.size == 0 {
            meta.size = new_data.len() as u64;
        }
        let new_ref = self
            .store
            .put(
                &scope,
                name,
                ArtifactBody::Bytes(Bytes::from(new_data)),
                meta,
            )
            .await?;
        let head = self.store.head(&new_ref).await?;
        self.upsert_index_row(&new_ref, &head).await?;
        Ok(serde_json::json!({ "ref": new_ref.to_wire(), "version": new_ref.version }))
    }

    async fn list_artifacts(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let scope = scope_from_ctx(ctx);
        let prefix = input["prefix"].as_str();
        let label_eq = match (input["label"].as_str(), input["value"].as_str()) {
            (Some(l), Some(v)) => Some((l, v)),
            (None, None) => None,
            _ => {
                return Err(OrkError::Validation(
                    "list_artifacts: provide both label and value, or neither".into(),
                ));
            }
        };
        let rows = self.meta.list(&scope, prefix, label_eq).await?;
        Ok(serde_json::json!({ "artifacts": rows }))
    }

    async fn load_artifact(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("name (string) required".into()))?;
        validate_logical_name(name)?;
        reject_cross_tenant_name(name, ctx.tenant_id)?;
        let want_v = u32::try_from(
            input["version"]
                .as_u64()
                .or_else(|| input["version"].as_i64().map(|i| i as u64))
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let max_inline: usize = input["max_inline_bytes"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(65_536) as _;
        let scope = scope_from_ctx(ctx);
        let s = self.find_summary_for(&scope, name, want_v).await?;
        let v = s.version;
        let aref = build_ref(s.scheme.as_str(), &scope, name, v);
        // Prefer inline text/data for objects that fit `max_inline_bytes` so LLM / agent
        // tool results are readable in-process. If we returned only `/api/artifacts/...`
        // whenever `public_api_base` is set, models would see a URL, not the bytes —
        // which breaks workflows that expect to quote or summarize content (ADR-0016).
        if (s.size as usize) <= max_inline {
            let body = self.store.get(&aref).await?;
            let bytes = read_body_to_vec(body).await?;
            if bytes.len() > max_inline {
                return Err(OrkError::Internal(
                    "load_artifact: object exceeds max_inline_bytes".into(),
                ));
            }
            return Self::part_from_loaded_bytes(bytes, &s.mime);
        }
        if let Some(u) = self
            .store
            .presign_get(&aref, Duration::from_secs(3600))
            .await?
        {
            let part = Part::file_uri(u, s.mime.clone());
            return Ok(serde_json::json!({ "part": part }));
        }
        if let Some(base) = &self.public_api_base {
            let path = aref.to_wire();
            let path_enc = urlencoding::encode(&path);
            let u = Url::parse(&format!(
                "{}/api/artifacts/{}",
                base.trim_end_matches('/'),
                path_enc.as_ref()
            ))
            .map_err(|e| OrkError::Internal(format!("proxy url: {e}")))?;
            let part = Part::file_uri(u, s.mime.clone());
            return Ok(serde_json::json!({ "part": part }));
        }
        let body = self.store.get(&aref).await?;
        let bytes = read_body_to_vec(body).await?;
        if bytes.len() > max_inline {
            return Err(OrkError::Internal(
                "load_artifact: large object but no presign or public API base".into(),
            ));
        }
        Self::part_from_loaded_bytes(bytes, &s.mime)
    }

    async fn artifact_meta(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("name (string) required".into()))?;
        validate_logical_name(name)?;
        reject_cross_tenant_name(name, ctx.tenant_id)?;
        let want_v = u32::try_from(
            input["version"]
                .as_u64()
                .or_else(|| input["version"].as_i64().map(|i| i as u64))
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let scope = scope_from_ctx(ctx);
        let s = self.find_summary_for(&scope, name, want_v).await?;
        let v = s.version;
        let aref = build_ref(s.scheme.as_str(), &scope, name, v);
        let m = self.store.head(&aref).await?;
        serde_json::to_value(m).map_err(|e| OrkError::Internal(e.to_string()))
    }

    async fn delete_artifact(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("name (string) required".into()))?;
        validate_logical_name(name)?;
        reject_cross_tenant_name(name, ctx.tenant_id)?;
        let scope = scope_from_ctx(ctx);
        let vfield = &input["version"];
        let delete_all = vfield.is_null() || vfield.as_str() == Some("*");

        if delete_all {
            let rows = self
                .meta
                .list(
                    &scope,
                    if name.is_empty() { None } else { Some(name) },
                    None,
                )
                .await?;
            let exact: Vec<_> = rows.into_iter().filter(|r| r.name == name).collect();
            for s in &exact {
                let aref = build_ref(s.scheme.as_str(), &scope, &s.name, s.version);
                self.store.delete(&aref).await?;
            }
            let n = self.meta.delete_all_versions(&scope, name).await?;
            return Ok(serde_json::json!({ "deleted": n }));
        }

        if let Some(n) = vfield
            .as_u64()
            .or_else(|| vfield.as_i64().map(|i| i as u64))
        {
            let ver = u32::try_from(n).map_err(|_| OrkError::Validation("bad version".into()))?;
            let s = self.find_summary_for(&scope, name, ver).await?;
            let aref = build_ref(s.scheme.as_str(), &scope, name, s.version);
            self.store.delete(&aref).await?;
            self.meta.delete_version(&aref).await?;
            return Ok(serde_json::json!({ "deleted": 1u32 }));
        }
        Err(OrkError::Validation(
            "delete_artifact: version must be a number, \"*\", or absent for delete-all".into(),
        ))
    }

    async fn pin_artifact(
        &self,
        ctx: &AgentContext,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| OrkError::Validation("name (string) required".into()))?;
        validate_logical_name(name)?;
        reject_cross_tenant_name(name, ctx.tenant_id)?;
        let want_v = u32::try_from(
            input["version"]
                .as_u64()
                .or_else(|| input["version"].as_i64().map(|i| i as u64))
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let scope = scope_from_ctx(ctx);
        let s = self.find_summary_for(&scope, name, want_v).await?;
        let v = s.version;
        let aref = build_ref(s.scheme.as_str(), &scope, name, v);
        self.meta.add_label(&aref, "pinned", "true").await?;
        Ok(serde_json::json!({ "ref": aref.to_wire() }))
    }
}

#[async_trait]
impl ToolExecutor for ArtifactToolExecutor {
    async fn execute(
        &self,
        ctx: &AgentContext,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, OrkError> {
        match tool_name {
            "create_artifact" => self.create_artifact(ctx, input).await,
            "append_artifact" => self.append_artifact(ctx, input).await,
            "list_artifacts" => self.list_artifacts(ctx, input).await,
            "load_artifact" => self.load_artifact(ctx, input).await,
            "artifact_meta" => self.artifact_meta(ctx, input).await,
            "delete_artifact" => self.delete_artifact(ctx, input).await,
            "pin_artifact" => self.pin_artifact(ctx, input).await,
            _ => Err(OrkError::Integration(format!(
                "unknown artifact tool: {tool_name}"
            ))),
        }
    }
}
