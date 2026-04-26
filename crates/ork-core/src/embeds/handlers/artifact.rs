//! `«artifact_content:name | fmt»` and `«artifact_meta:name | version?»` (ADR-0016).

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::StreamExt;
use ork_a2a::Part;

use crate::ports::artifact_store::{ArtifactBody, ArtifactRef, ArtifactScope};

use super::super::{EmbedContext, EmbedError, EmbedHandler, EmbedOutput, EmbedPhase};

/// Late-phase: inline bytes or (when large) a text stub plus file URI.
pub struct ArtifactContentHandler;

/// Early + late: one-line JSON metadata.
pub struct ArtifactMetaHandler;

fn scope(ctx: &EmbedContext) -> ArtifactScope {
    ArtifactScope {
        tenant_id: ctx.tenant_id,
        context_id: ctx.context_id,
    }
}

async fn resolve_latest_ref(ctx: &EmbedContext, name: &str) -> Result<ArtifactRef, EmbedError> {
    let store = ctx.artifact_store.as_ref().ok_or_else(|| {
        EmbedError::Handler(anyhow::anyhow!(
            "artifact embeds require an ArtifactStore in EmbedContext (ADR-0016)"
        ))
    })?;
    let scope = scope(ctx);
    let list = store
        .list(&scope, if name.is_empty() { None } else { Some(name) })
        .await
        .map_err(|e: ork_common::error::OrkError| {
            EmbedError::Handler(anyhow::anyhow!(e.to_string()))
        })?;
    list.into_iter()
        .filter(|s| s.name == name)
        .max_by_key(|s| s.version)
        .map(|s| ArtifactRef {
            scheme: s.scheme,
            tenant_id: scope.tenant_id,
            context_id: scope.context_id,
            name: s.name,
            version: s.version,
            etag: String::new(),
        })
        .ok_or_else(|| EmbedError::InvalidExpression(format!("no artifact named {name}")))
}

async fn read_all(body: ArtifactBody) -> Result<Vec<u8>, EmbedError> {
    match body {
        ArtifactBody::Bytes(b) => Ok(b.to_vec()),
        ArtifactBody::Stream(mut s) => {
            let mut v = Vec::new();
            while let Some(c) = s.next().await {
                v.extend_from_slice(
                    &c.map_err(|e| EmbedError::Handler(anyhow::anyhow!(e.to_string())))?,
                );
            }
            Ok(v)
        }
    }
}

fn proxy_artifact_url(base: &str, r: &ArtifactRef) -> Result<url::Url, EmbedError> {
    let path = r.to_wire();
    let enc = urlencoding::encode(&path);
    url::Url::parse(&format!(
        "{}/api/artifacts/{}",
        base.trim_end_matches('/'),
        enc.as_ref()
    ))
    .map_err(|e| EmbedError::Handler(anyhow::anyhow!(e.to_string())))
}

fn format_bytes(bytes: &[u8], fmt: &str) -> Result<EmbedOutput, EmbedError> {
    let f = fmt.to_lowercase();
    match f.as_str() {
        "text" | "txt" => {
            let t = std::str::from_utf8(bytes)
                .map_err(|e| EmbedError::Handler(anyhow::anyhow!("utf8: {e}")))?;
            Ok(EmbedOutput::Text(t.to_string()))
        }
        "base64" => Ok(EmbedOutput::Text(B64.encode(bytes))),
        "json" => {
            let v: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| EmbedError::Handler(anyhow::anyhow!("json: {e}")))?;
            Ok(EmbedOutput::Parts(vec![Part::data(v)]))
        }
        "csv" | "auto" => {
            if let Ok(t) = std::str::from_utf8(bytes) {
                Ok(EmbedOutput::Text(t.to_string()))
            } else {
                Ok(EmbedOutput::Text(B64.encode(bytes)))
            }
        }
        _ => Err(EmbedError::InvalidFormat(f)),
    }
}

#[async_trait]
impl EmbedHandler for ArtifactContentHandler {
    fn type_id(&self) -> &'static str {
        "artifact_content"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Late
    }

    async fn resolve(
        &self,
        expr: &str,
        format: Option<&str>,
        ctx: &EmbedContext,
    ) -> Result<EmbedOutput, EmbedError> {
        let name = expr.trim();
        if name.is_empty() {
            return Err(EmbedError::InvalidExpression(
                "artifact name required".into(),
            ));
        }
        let cap = ctx.max_late_embed_output_bytes;
        let r = resolve_latest_ref(ctx, name).await?;
        let store = ctx
            .artifact_store
            .as_ref()
            .expect("checked in resolve_latest_ref");
        let body = store
            .get(&r)
            .await
            .map_err(|e: ork_common::error::OrkError| {
                EmbedError::Handler(anyhow::anyhow!(e.to_string()))
            })?;
        let bytes = read_all(body).await?;
        let fmt = format
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("auto");

        if bytes.len() > cap {
            if let Some(u) = store
                .presign_get(&r, Duration::from_secs(3600))
                .await
                .map_err(|e| EmbedError::Handler(anyhow::anyhow!(e.to_string())))?
            {
                return Ok(EmbedOutput::Parts(vec![
                    Part::text("[truncated; use linked artifact]".to_string()),
                    Part::file_uri(u, None),
                ]));
            }
            if let Some(b) = &ctx.artifact_public_base {
                let u = proxy_artifact_url(b, &r)?;
                return Ok(EmbedOutput::Parts(vec![
                    Part::text("[truncated; use linked artifact]".to_string()),
                    Part::file_uri(u, None),
                ]));
            }
            return Ok(EmbedOutput::Text(
                "[artifact too large: configure presign or API base for file embedding]".into(),
            ));
        }
        format_bytes(&bytes, fmt)
    }
}

#[async_trait]
impl EmbedHandler for ArtifactMetaHandler {
    fn type_id(&self) -> &'static str {
        "artifact_meta"
    }

    fn phase(&self) -> EmbedPhase {
        EmbedPhase::Both
    }

    async fn resolve(
        &self,
        expr: &str,
        _format: Option<&str>,
        ctx: &EmbedContext,
    ) -> Result<EmbedOutput, EmbedError> {
        let name = expr.trim();
        if name.is_empty() {
            return Err(EmbedError::InvalidExpression(
                "artifact name required".into(),
            ));
        }
        let r = resolve_latest_ref(ctx, name).await?;
        let store = ctx.artifact_store.as_ref().ok_or_else(|| {
            EmbedError::Handler(anyhow::anyhow!("artifact embeds require ArtifactStore"))
        })?;
        let m = store
            .head(&r)
            .await
            .map_err(|e: ork_common::error::OrkError| {
                EmbedError::Handler(anyhow::anyhow!(e.to_string()))
            })?;
        let j = serde_json::to_value(&m).map_err(|e| EmbedError::Handler(e.into()))?;
        Ok(EmbedOutput::Text(j.to_string()))
    }
}
