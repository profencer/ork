//! Build [`ChainedArtifactStore`] from [`AppConfig`](ork_common::config::AppConfig) (ADR-0016).

use std::sync::Arc;

use anyhow::Context;
use ork_common::config::AppConfig;
use ork_core::ports::artifact_store::ArtifactStore;
use ork_storage::chained::ChainedArtifactStore;
use ork_storage::fs::FilesystemArtifactStore;
use ork_storage::s3::S3ArtifactStore;

/// Build the process-wide artifact store. `config.artifacts.enabled` must be true.
pub async fn build_artifact_store(config: &AppConfig) -> anyhow::Result<Arc<dyn ArtifactStore>> {
    let root = &config.artifacts.fs.root;
    std::fs::create_dir_all(root)
        .with_context(|| format!("create artifact root {}", root.display()))?;

    let fs = Arc::new(FilesystemArtifactStore::new(root));
    let mut others: Vec<Arc<dyn ArtifactStore>> = Vec::new();
    if let Some(s3c) = &config.artifacts.s3 {
        let s3 = S3ArtifactStore::new(&s3c.bucket, &s3c.region, s3c.endpoint.clone())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        others.push(Arc::new(s3));
    }

    let chained =
        ChainedArtifactStore::new(fs, others).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(Arc::new(chained))
}
