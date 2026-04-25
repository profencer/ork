//! ADR-0009 §`tasks/pushNotificationConfig/set` validation:
//!
//! 1. HTTPS is required outside `env=dev`. Plain `http://` is rejected with
//!    `INVALID_PARAMS` (`-32602`).
//! 2. The per-tenant cap (`config.push.max_per_tenant`) is enforced before
//!    the row is upserted. The 101st `set` for a tenant — at the default
//!    cap of 100 — MUST surface as `INVALID_PARAMS`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ork_a2a::{ContextId, JsonRpcError, TaskId, TaskState};
use ork_api::routes::a2a;
use ork_common::types::TenantId;
use ork_core::ports::a2a_push_repo::{A2aPushConfigRepository, A2aPushConfigRow};
use ork_core::ports::a2a_task_repo::{A2aTaskRepository, A2aTaskRow};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{auth_for, jsonrpc_request, read_body, test_state_with_push};

async fn seed_task(t: &crate::common::TestState, tenant: TenantId, id: TaskId) {
    let now = chrono::Utc::now();
    t.task_repo
        .create_task(&A2aTaskRow {
            id,
            context_id: ContextId::new(),
            tenant_id: tenant,
            agent_id: "planner".into(),
            parent_task_id: None,
            workflow_run_id: None,
            state: TaskState::Working,
            metadata: json!({}),
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn set_rejects_http_url_outside_dev() {
    // Switch env to "prod" so the validator enforces HTTPS-only.
    let mut t = test_state_with_push().await;
    t.state.config.env = "prod".into();

    let tenant = t.tenant_id;
    let task_id = TaskId::new();
    seed_task(&t, tenant, task_id).await;

    let app = a2a::protected_router(t.state.clone());
    let body = jsonrpc_request(
        json!("rpc-1"),
        "tasks/pushNotificationConfig/set",
        json!({
            "task_id": task_id,
            "push_notification_config": {
                "url": "http://untrusted.example.com/cb",
            },
        }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(
        v["error"]["code"],
        JsonRpcError::INVALID_PARAMS,
        "ADR-0009: HTTPS is mandatory outside env=dev; got {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("https"),
        "error message should mention the HTTPS requirement; got `{msg}`"
    );
}

#[tokio::test]
async fn set_allows_http_localhost_in_dev() {
    let t = test_state_with_push().await;
    assert_eq!(
        t.state.config.env, "dev",
        "default test fixture uses env=dev; otherwise this test is meaningless"
    );

    let tenant = t.tenant_id;
    let task_id = TaskId::new();
    seed_task(&t, tenant, task_id).await;

    let app = a2a::protected_router(t.state.clone());
    let body = jsonrpc_request(
        json!("rpc-1"),
        "tasks/pushNotificationConfig/set",
        json!({
            "task_id": task_id,
            "push_notification_config": {
                "url": "http://127.0.0.1:8080/api/webhooks/a2a-ack",
            },
        }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert!(
        v["error"].is_null(),
        "dev env MUST accept http://localhost; got error envelope: {v}"
    );
}

#[tokio::test]
async fn set_enforces_per_tenant_cap() {
    // Drop the cap to a tiny number so the test stays cheap.
    let mut t = test_state_with_push().await;
    t.state.config.push.max_per_tenant = 2;

    let tenant = t.tenant_id;

    // Pre-fill the in-memory repo to exactly the cap so the next `set` is the
    // one that should be rejected. This sidesteps having to drive the JSON-RPC
    // dispatcher 100 times.
    for _ in 0..2 {
        let task_id = TaskId::new();
        seed_task(&t, tenant, task_id).await;
        t.push_repo
            .upsert(&A2aPushConfigRow {
                id: uuid::Uuid::now_v7(),
                task_id,
                tenant_id: tenant,
                url: "https://cb.example.com/hook".parse().unwrap(),
                token: None,
                authentication: None,
                metadata: json!({}),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }
    let count = t.push_repo.count_active_for_tenant(tenant).await.unwrap();
    assert_eq!(count, 2, "preloaded {count} rows; cap should now reject");

    let task_id = TaskId::new();
    seed_task(&t, tenant, task_id).await;
    let app = a2a::protected_router(t.state.clone());
    let body = jsonrpc_request(
        json!("rpc-cap"),
        "tasks/pushNotificationConfig/set",
        json!({
            "task_id": task_id,
            "push_notification_config": {
                "url": "https://cb.example.com/hook",
            },
        }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(
        v["error"]["code"],
        JsonRpcError::INVALID_PARAMS,
        "ADR-0009: cap MUST surface as INVALID_PARAMS; got {v}"
    );
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("cap"),
        "error message should mention the cap; got `{msg}`"
    );
}
