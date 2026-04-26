//! ADR-0016: background sweep that deletes expired blobs and Postgres index rows
//! (see `ArtifactMetaRepo::eligible_for_sweep`).

use std::sync::Arc;
use std::time::Duration;

use ork_core::ports::artifact_meta_repo::ArtifactMetaRepo;
use ork_core::ports::artifact_store::ArtifactStore;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Run [`ArtifactMetaRepo::eligible_for_sweep`], then for each `ArtifactRef` delete the
/// blob and the index row. Backs off on query failures; per-ref failures are logged.
pub fn spawn_artifact_retention_sweep(
    store: Arc<dyn ArtifactStore>,
    meta: Arc<dyn ArtifactMetaRepo>,
    default_days: u32,
    task_artifacts_days: u32,
    sweep_interval_secs: u64,
    cancel: CancellationToken,
) {
    let interval = Duration::from_secs(sweep_interval_secs.max(1));
    info!(
        interval_secs = interval.as_secs(),
        default_days,
        task_days = task_artifacts_days,
        "ADR-0016: starting artifact retention sweep loop"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    let now = chrono::Utc::now();
                    let refs = match meta
                        .eligible_for_sweep(now, default_days, task_artifacts_days)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "ADR-0016: eligible_for_sweep failed");
                            continue;
                        }
                    };
                    if refs.is_empty() {
                        continue;
                    }
                    let mut deleted = 0u32;
                    for r in refs {
                        if let Err(e) = store.delete(&r).await {
                            tracing::warn!(
                                error = %e,
                                wire = %r.to_wire(),
                                "ADR-0016: store delete failed; skipping index row"
                            );
                            continue;
                        }
                        if let Err(e) = meta.delete_version(&r).await {
                            tracing::warn!(
                                error = %e,
                                wire = %r.to_wire(),
                                "ADR-0016: meta delete_version failed after blob delete"
                            );
                            continue;
                        }
                        deleted = deleted.saturating_add(1);
                    }
                    if deleted > 0 {
                        tracing::info!(count = deleted, "ADR-0016: retention sweep deleted artifact versions");
                    }
                }
            }
        }
    });
}
