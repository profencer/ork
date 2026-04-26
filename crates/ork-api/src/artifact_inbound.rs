//! ADR-0016: API handlers call [`ork_integrations::artifact_wire::rewrite_inline_file_parts_to_uris`].
//!
// ADR-0021: enforce user-level artifact ACLs in the handler before calling rewrite.

use ork_a2a::{ContextId, MessageId, Part, TaskId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::artifact_meta_repo::ArtifactMetaRepo;
use ork_core::ports::artifact_store::ArtifactStore;
use ork_integrations::artifact_wire::rewrite_inline_file_parts_to_uris;
use std::sync::Arc;

/// Resolve a public base for [`ork_a2a::FileRef::Uri`] (no trailing slash).
#[must_use]
pub fn artifact_public_base_url(config: &ork_common::config::AppConfig) -> String {
    if let Some(u) = &config.discovery.public_base_url {
        u.as_str().trim_end_matches('/').to_string()
    } else {
        format!("http://{}:{}", config.server.host, config.server.port)
    }
}

/// Inbound: upload each `FileRef::Bytes` and replace with a proxy `file: { uri }`.
#[allow(clippy::too_many_arguments)]
pub async fn rewrite_inbound_file_parts(
    store: &Arc<dyn ArtifactStore>,
    meta: &Arc<dyn ArtifactMetaRepo>,
    public_base: &str,
    tenant_id: TenantId,
    context_id: ContextId,
    task_id: TaskId,
    message_id: MessageId,
    parts: Vec<Part>,
) -> Result<Vec<Part>, OrkError> {
    rewrite_inline_file_parts_to_uris(
        store,
        meta,
        public_base,
        tenant_id,
        context_id,
        task_id,
        message_id,
        "inbound",
        parts,
    )
    .await
}
