use chrono::Utc;
use ork_common::types::TenantId;
use ork_core::ports::artifact_store::{
    ArtifactBody, ArtifactMeta, ArtifactRef, ArtifactScope, ArtifactStore,
};
use ork_storage::fs::FilesystemArtifactStore;

fn scope(t: TenantId) -> ArtifactScope {
    ArtifactScope {
        tenant_id: t,
        context_id: None,
    }
}

#[tokio::test]
async fn put_get_round_trip() {
    let d = tempfile::tempdir().expect("dir");
    let s = FilesystemArtifactStore::new(d.path());
    let t = TenantId::new();
    let sc = scope(t);
    let mut m = ArtifactMeta {
        size: 0,
        created_at: Utc::now(),
        ..Default::default()
    };
    m.mime = Some("text/plain".into());
    let r = s
        .put(&sc, "hello.txt", ArtifactBody::Bytes("hi".into()), m)
        .await
        .expect("put");
    assert_eq!(r.version, 1);
    let body = s.get(&r).await.expect("get");
    match body {
        ork_core::ports::artifact_store::ArtifactBody::Bytes(b) => {
            assert_eq!(&b[..], b"hi");
        }
        _ => panic!("expected bytes"),
    }
}

#[tokio::test]
async fn version_increments() {
    let d = tempfile::tempdir().expect("dir");
    let s = FilesystemArtifactStore::new(d.path());
    let t = TenantId::new();
    let sc = scope(t);
    let m = || ArtifactMeta {
        size: 0,
        created_at: Utc::now(),
        ..Default::default()
    };
    let r1 = s
        .put(&sc, "x", ArtifactBody::Bytes("1".into()), m())
        .await
        .expect("1");
    let r2 = s
        .put(&sc, "x", ArtifactBody::Bytes("2".into()), m())
        .await
        .expect("2");
    assert_eq!(r1.version + 1, r2.version);
}

#[tokio::test]
async fn ref_wire_parses() {
    let t = TenantId::new();
    let r = ArtifactRef {
        scheme: "fs".into(),
        tenant_id: t,
        context_id: None,
        name: "a/b".into(),
        version: 2,
        etag: "x".into(),
    };
    let w = r.to_wire();
    let p = ArtifactRef::parse(&w).expect("parse");
    assert_eq!(p.tenant_id.0, t.0);
    assert_eq!(p.name, "a/b");
    assert_eq!(p.version, 2);
}
