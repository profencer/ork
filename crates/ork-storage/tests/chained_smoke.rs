use std::sync::Arc;

use chrono::Utc;
use ork_common::types::TenantId;
use ork_core::ports::artifact_store::{ArtifactBody, ArtifactStore};
use ork_storage::chained::ChainedArtifactStore;
use ork_storage::chained::split_scheme_name;
use ork_storage::fs::FilesystemArtifactStore;

#[test]
fn split_scheme() {
    assert_eq!(split_scheme_name("s3:foo/bar"), (Some("s3"), "foo/bar"));
    assert_eq!(split_scheme_name("report.txt"), (None, "report.txt"));
}

#[tokio::test]
async fn chains_default() {
    let a = Arc::new(FilesystemArtifactStore::new(
        tempfile::tempdir().unwrap().path(),
    ));
    let c = ChainedArtifactStore::new(a, std::iter::empty()).expect("chained");
    let t = TenantId::new();
    let sc = ork_core::ports::artifact_store::ArtifactScope {
        tenant_id: t,
        context_id: None,
    };
    let m = || ork_core::ports::artifact_store::ArtifactMeta {
        size: 0,
        created_at: Utc::now(),
        ..Default::default()
    };
    let r = c
        .put(&sc, "f.txt", ArtifactBody::Bytes("x".into()), m())
        .await
        .expect("put");
    assert_eq!(r.scheme, "fs");
}
