//! ADR-0008 §`tasks/pushNotificationConfig/{set,get}` (also ADR-0009 pulled
//! forward) — integration tests against the in-memory push repo. Exercises:
//!
//! - Round-trip: `set` then `get` returns the same `PushNotificationConfig`.
//! - `INVALID_PARAMS` (`-32602`) when params are missing or malformed.
//! - `PUSH_NOTIFICATION_NOT_SUPPORTED` (`-32003`) when no config has been set.
//! - Tenant isolation: a config registered under tenant A is invisible to
//!   tenant B's `get`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ork_a2a::{ContextId, TaskId, TaskState};
use ork_api::routes::a2a;
use ork_common::types::TenantId;
use ork_core::ports::a2a_task_repo::{A2aTaskRepository, A2aTaskRow};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{auth_for, jsonrpc_request, read_body, test_state};

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
async fn push_set_then_get_round_trips_config() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_id = TaskId::new();
    seed_task(&t, tenant, task_id).await;

    let app = a2a::protected_router(t.state.clone());

    let set_body = jsonrpc_request(
        json!("rpc-set"),
        "tasks/pushNotificationConfig/set",
        json!({
            "task_id": task_id,
            "push_notification_config": {
                "url": "https://cb.example.com/hook",
                "token": "secret-token",
            },
        }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(set_body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], "rpc-set");
    assert_eq!(v["result"]["url"], "https://cb.example.com/hook");
    assert_eq!(v["result"]["token"], "secret-token");

    let get_body = jsonrpc_request(
        json!("rpc-get"),
        "tasks/pushNotificationConfig/get",
        json!({ "task_id": task_id }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(get_body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["id"], "rpc-get");
    assert_eq!(v["result"]["url"], "https://cb.example.com/hook");
    assert_eq!(v["result"]["token"], "secret-token");
}

#[tokio::test]
async fn push_get_returns_not_supported_when_no_config_registered() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_id = TaskId::new();
    seed_task(&t, tenant, task_id).await;

    let app = a2a::protected_router(t.state.clone());
    let body = jsonrpc_request(
        json!("rpc-1"),
        "tasks/pushNotificationConfig/get",
        json!({ "task_id": task_id }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], -32_003);
}

#[tokio::test]
async fn push_set_with_invalid_params_returns_invalid_params() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state.clone());

    let body = jsonrpc_request(
        json!("rpc-1"),
        "tasks/pushNotificationConfig/set",
        json!({ "wrong": "shape" }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], -32_602);
}

#[tokio::test]
async fn push_get_is_tenant_isolated() {
    let t = test_state().await;
    let owner = t.tenant_id;
    let task_id = TaskId::new();
    seed_task(&t, owner, task_id).await;

    let app = a2a::protected_router(t.state.clone());

    let set_body = jsonrpc_request(
        json!("rpc-set"),
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
        .body(Body::from(set_body))
        .unwrap();
    req.extensions_mut().insert(auth_for(owner));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let intruder = TenantId::new();
    let get_body = jsonrpc_request(
        json!("rpc-get"),
        "tasks/pushNotificationConfig/get",
        json!({ "task_id": task_id }),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(get_body))
        .unwrap();
    req.extensions_mut().insert(auth_for(intruder));
    let resp = app.oneshot(req).await.unwrap();
    let v = read_body(resp).await;
    assert_eq!(
        v["error"]["code"], -32_003,
        "tenant isolation MUST hide configs from other tenants; got: {v}"
    );
}
