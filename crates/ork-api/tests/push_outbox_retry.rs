//! ADR-0009 §`Delivery worker` retry + dead-letter coverage.
//!
//! The worker uses `WorkerConfig::retry_intervals` (defaults to `[1m, 5m, 30m]`)
//! between attempts. Tests inject sub-second intervals so the assertions don't
//! have to wait minutes — the production cadence is unit-tested separately.
//!
//! Scenarios:
//!
//! - 503 twice then 200: exactly 3 requests, no dead-letter row.
//! - 503 four times: exactly 4 requests, one dead-letter row.

mod common;

use std::time::Duration;

use ork_a2a::{TaskId, TaskState};
use ork_common::types::TenantId;
use ork_core::ports::a2a_push_repo::{A2aPushConfigRepository, A2aPushConfigRow};
use ork_push::worker::WorkerConfig;
use serde_json::json;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{build_worker, test_state_with_push};

/// Tight retry intervals so the whole test runs sub-second. The shape (3
/// retries → 4 attempts) matches the ADR-0009 default, only compressed.
fn fast_cfg() -> WorkerConfig {
    WorkerConfig {
        retry_intervals: vec![
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(20),
        ],
        request_timeout_secs: 5,
        max_concurrency: 4,
        user_agent: "ork-push-test/0.0.0".into(),
    }
}

async fn register_push(
    push_repo: &(impl A2aPushConfigRepository + ?Sized),
    tenant: TenantId,
    task_id: TaskId,
    url: &str,
) {
    push_repo
        .upsert(&A2aPushConfigRow {
            id: uuid::Uuid::now_v7(),
            task_id,
            tenant_id: tenant,
            url: url.parse().unwrap(),
            token: None,
            authentication: None,
            metadata: json!({}),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
}

async fn wait_for_requests(server: &MockServer, n: usize, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(reqs) = server.received_requests().await
            && reqs.len() >= n
        {
            return;
        }
        if std::time::Instant::now() > deadline {
            let count = server
                .received_requests()
                .await
                .map(|r| r.len())
                .unwrap_or(0);
            panic!("expected {n} requests within {budget:?}; saw {count}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_retries_on_5xx_and_recovers_on_2xx() {
    let t = test_state_with_push().await;
    let tenant = t.tenant_id;
    let task_id = TaskId::new();

    let subscriber = MockServer::start().await;
    // Two 503s, then a 200. wiremock evaluates mounts in order they were
    // registered, with `up_to_n_times` capping each.
    Mock::given(method("POST"))
        .and(path("/cb"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&subscriber)
        .await;
    Mock::given(method("POST"))
        .and(path("/cb"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&subscriber)
        .await;

    register_push(
        &*t.push_repo,
        tenant,
        task_id,
        &format!("{}/cb", subscriber.uri()),
    )
    .await;

    let cancel = CancellationToken::new();
    let worker = build_worker(&t, fast_cfg());
    let handle = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };

    // Wait for the worker's subscription to be live before publishing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish the outbox envelope directly via the in-AppState push service.
    t.push_service
        .publish_terminal(tenant, task_id, TaskState::Completed)
        .await;

    wait_for_requests(&subscriber, 3, Duration::from_secs(5)).await;

    let received = subscriber.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        3,
        "expected exactly 3 attempts (503, 503, 200); got {}",
        received.len()
    );

    // Give the dead-letter codepath a beat to (not) run, then snapshot.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let dl = t.dead_letter_repo.snapshot().await;
    assert!(
        dl.is_empty(),
        "successful delivery on attempt 3 MUST NOT dead-letter; got {dl:?}"
    );

    cancel.cancel();
    let _ = timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_dead_letters_after_exhausting_retries() {
    let t = test_state_with_push().await;
    let tenant = t.tenant_id;
    let task_id = TaskId::new();

    let subscriber = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/cb"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&subscriber)
        .await;

    register_push(
        &*t.push_repo,
        tenant,
        task_id,
        &format!("{}/cb", subscriber.uri()),
    )
    .await;

    let cancel = CancellationToken::new();
    let worker = build_worker(&t, fast_cfg());
    let handle = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };

    tokio::time::sleep(Duration::from_millis(50)).await;
    t.push_service
        .publish_terminal(tenant, task_id, TaskState::Failed)
        .await;

    // 4 attempts total (1 initial + 3 retries).
    wait_for_requests(&subscriber, 4, Duration::from_secs(5)).await;

    // Wait briefly for the dead-letter row to land.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let dl = loop {
        let snap = t.dead_letter_repo.snapshot().await;
        if !snap.is_empty() {
            break snap;
        }
        if std::time::Instant::now() > deadline {
            panic!("dead-letter row never appeared after exhausting retries");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(dl.len(), 1, "exactly one dead-letter row expected");
    let row = &dl[0];
    assert_eq!(row.task_id, task_id);
    assert_eq!(row.tenant_id, tenant);
    assert_eq!(row.last_status, Some(503));
    assert_eq!(row.attempts, 4, "1 initial + 3 retries = 4 attempts");
    assert_eq!(row.url, format!("{}/cb", subscriber.uri()));

    let received = subscriber.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        4,
        "expected exactly 4 attempts (503 each); got {}",
        received.len()
    );

    cancel.cancel();
    let _ = timeout(Duration::from_secs(2), handle).await;
}
