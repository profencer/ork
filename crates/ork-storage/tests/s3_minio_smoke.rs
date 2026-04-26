//! S3 `ArtifactStore` against a real MinIO container (Docker required).
//!
//! `ork-storage` default features do not include `s3-it`; this test is only built
//! with `--features s3-it`.
//! ```text
//! cargo test -p ork-storage --features s3-it --test s3_minio_smoke
//! ```
//! Override image: `ORK_MINIO_IMAGE=minio/minio:edge`.

use std::time::Duration;

use aws_config::BehaviorVersion;
use chrono::Utc;
use ork_common::types::TenantId;
use ork_core::ports::artifact_store::{ArtifactBody, ArtifactMeta, ArtifactScope, ArtifactStore};
use ork_storage::s3::S3ArtifactStore;
use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers::core::ContainerPort;
use testcontainers::runners::AsyncRunner;
use tokio::time::sleep;
use uuid::Uuid;

const MINIO_PORT: u16 = 9000;

fn endpoint_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

/// Parse `repo:tag` (default `minio/minio:latest`).
fn minio_image() -> (String, String) {
    let full =
        std::env::var("ORK_MINIO_IMAGE").unwrap_or_else(|_| "minio/minio:latest".to_string());
    full.rsplit_once(':')
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .unwrap_or((full, "latest".to_string()))
}

async fn s3_client(endpoint: &str) -> aws_sdk_s3::Client {
    const KEYS: &str = "minioadmin";
    // `set_var` is `unsafe` in Rust 2024; this integration test is single-threaded
    // and no other test should rely on a clean `AWS_*` environment.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", KEYS);
        std::env::set_var("AWS_SECRET_ACCESS_KEY", KEYS);
    }
    let base = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_types::region::Region::new("us-east-1"))
        .load()
        .await;
    let mut b = aws_sdk_s3::config::Builder::from(&base);
    b = b.endpoint_url(endpoint).force_path_style(true);
    aws_sdk_s3::Client::from_conf(b.build())
}

async fn wait_s3_up(endpoint: &str) {
    for _ in 0..60u32 {
        let c = s3_client(endpoint).await;
        if c.list_buckets().send().await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(200)).await;
    }
    panic!("MinIO S3 not responding at {endpoint}");
}

async fn ensure_bucket(client: &aws_sdk_s3::Client, name: &str) {
    let r = client.create_bucket().bucket(name).send().await;
    if r.is_ok() {
        return;
    }
    let msg = r.unwrap_err().to_string();
    if msg.to_lowercase().contains("owned") || msg.contains("BucketAlready") {
        return;
    }
    if msg.to_lowercase().contains("createbucket") && msg.to_lowercase().contains("conflict") {
        return;
    }
    panic!("create bucket {name}: {msg}");
}

#[tokio::test]
async fn minio_put_get_presign_round_trip() {
    let (repo, tag) = minio_image();
    // Readiness is handled by [`wait_s3_up`] (list_buckets) so we avoid chaining
    // `with_wait_for` on the post-`with_env` builder surface in testcontainers 0.23.
    let image = GenericImage::new(repo, tag)
        .with_exposed_port(ContainerPort::Tcp(MINIO_PORT))
        .with_env_var("MINIO_ROOT_USER", "minioadmin")
        .with_env_var("MINIO_ROOT_PASSWORD", "minioadmin")
        .with_cmd(vec!["server".to_string(), "/data".to_string()]);
    let container = image
        .start()
        .await
        .expect("start MinIO (Docker must be available)");
    let host_port = container
        .get_host_port_ipv4(MINIO_PORT)
        .await
        .expect("MinIO port");
    // Keep the container until the end of the test.
    let _container = container;

    let ep = endpoint_for(host_port);
    wait_s3_up(&ep).await;

    let bucket = format!("ork-it-{}", Uuid::new_v4().simple());
    let c = s3_client(&ep).await;
    ensure_bucket(&c, &bucket).await;

    let store = S3ArtifactStore::new(&bucket, "us-east-1", Some(ep.clone()))
        .await
        .expect("S3ArtifactStore::new (MinIO)");
    let tenant = TenantId::new();
    let scope = ArtifactScope {
        tenant_id: tenant,
        context_id: None,
    };
    let data = b"round-trip s3 it".to_vec();
    let meta = ArtifactMeta {
        size: 0,
        created_at: Utc::now(),
        ..Default::default()
    };
    let r = store
        .put(
            &scope,
            "note.txt",
            ArtifactBody::Bytes(data.clone().into()),
            meta,
        )
        .await
        .expect("put");
    assert_eq!(r.scheme, "s3");
    let body = store.get(&r).await.expect("get");
    let bytes = match body {
        ArtifactBody::Bytes(b) => b.to_vec(),
        ArtifactBody::Stream(_) => panic!("unexpected stream in small read"),
    };
    assert_eq!(bytes, data);

    let u = store
        .presign_get(&r, Duration::from_secs(60))
        .await
        .expect("presign");
    assert!(u.is_some());
    let url = u.unwrap();
    let s = url.as_str();
    assert!(s.starts_with("http"), "expected presigned URL, got {s}");
}
