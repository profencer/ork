//! ADR-0008 §`JSON-RPC dispatcher` envelope-shape tests.
//!
//! These pin the dispatcher's behaviour for malformed envelopes and unknown
//! methods. Per-method bodies (`message/send`, `tasks/get`, …) are exercised by
//! their own dedicated integration tests.
//!
//! We bypass the JWT middleware by inserting `AuthContext` as an extension on
//! the request directly — the dispatcher's only auth dependency is the extension
//! lookup, so this isolates the dispatcher under test.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ork_a2a::{
    ContextId, JsonRpcError, MessageSendParams, Part, TaskId, TaskIdParams, TaskQueryParams,
    TaskState,
};
use ork_api::routes::a2a;
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{auth_for, auth_for_with_scopes, jsonrpc_request, read_body, test_state};

/// ADR-0021 §`Defaults` reserves `agent:<id>:cancel` for the
/// Operator/admin profile. Cancel-test fixtures mint the End-user
/// scope set plus `agent:*:cancel` so the gate passes; the deny path
/// is pinned by `tasks_cancel_without_cancel_scope_is_forbidden`.
fn auth_with_cancel(tenant_id: ork_common::types::TenantId) -> ork_api::middleware::AuthContext {
    auth_for_with_scopes(
        tenant_id,
        &[
            "tenant:self",
            "webui:access",
            "agent:*:invoke",
            "agent:*:cancel",
            "tool:*:invoke",
            "artifact:tenant:read",
            "artifact:tenant:write",
            "model:default:default:invoke",
        ],
    )
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let t = test_state().await;
    let app = a2a::protected_router(t.state);
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": "rpc-1",
        "method": "does/not/exist",
        "params": {}
    }))
    .unwrap();
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(t.tenant_id));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], JsonRpcError::METHOD_NOT_FOUND);
    assert_eq!(v["id"], "rpc-1");
    assert!(
        v.get("result").map(|r| r.is_null()).unwrap_or(true),
        "error envelope must omit `result`"
    );
}

#[tokio::test]
async fn invalid_jsonrpc_version_returns_invalid_request() {
    let t = test_state().await;
    let app = a2a::protected_router(t.state);
    let body = json!({
        "jsonrpc": "1.0",
        "id": 1,
        "method": "message/send",
        "params": {}
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(auth_for(t.tenant_id));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], JsonRpcError::INVALID_REQUEST);
    assert_eq!(v["id"], 1);
}

#[tokio::test]
async fn malformed_json_returns_parse_error_with_null_id() {
    let t = test_state().await;
    let app = a2a::protected_router(t.state);
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    req.extensions_mut().insert(auth_for(t.tenant_id));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], JsonRpcError::PARSE_ERROR);
    assert!(v["id"].is_null(), "parse error envelope MUST use null id");
}

#[tokio::test]
async fn message_send_with_empty_params_returns_invalid_params() {
    let t = test_state().await;
    let app = a2a::protected_router(t.state);
    let body = json!({
        "jsonrpc": "2.0",
        "id": "send-1",
        "method": "message/send",
        "params": {}
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(auth_for(t.tenant_id));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], JsonRpcError::INVALID_PARAMS);
}

// =============================================================================
// Task 8: `message/send`
// =============================================================================

#[tokio::test]
async fn message_send_persists_task_and_returns_completed_task() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_repo = t.task_repo.clone();
    let app = a2a::protected_router(t.state);

    let params = MessageSendParams {
        message: ork_a2a::Message::user(vec![Part::text("ping")]),
        configuration: None,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-1"),
        "message/send",
        serde_json::to_value(&params).unwrap(),
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

    assert_eq!(v["id"], "rpc-1", "echoes request id");
    assert!(v["error"].is_null(), "expected success envelope, got {v}");
    let task_id_str = v["result"]["id"]
        .as_str()
        .expect("result.id present")
        .to_string();
    assert_eq!(v["result"]["status"]["state"], "completed");

    let history = v["result"]["history"]
        .as_array()
        .expect("history is an array");
    assert_eq!(
        history.len(),
        2,
        "history MUST contain inbound user + agent reply"
    );
    assert_eq!(history[0]["role"], "user");
    assert_eq!(history[1]["role"], "agent");

    let task_id: TaskId = task_id_str.parse().expect("task id parses");
    let row = task_repo
        .get_task(tenant, task_id)
        .await
        .unwrap()
        .expect("task persisted");
    assert_eq!(row.agent_id, "planner");
    assert_eq!(row.tenant_id, tenant);
    assert_eq!(row.state, TaskState::Completed);
    assert!(
        row.completed_at.is_some(),
        "terminal state MUST set completed_at"
    );
}

// =============================================================================
// Task 9: `tasks/get`
// =============================================================================

async fn seed_task_with_message(
    repo: &(impl A2aTaskRepository + ?Sized),
    tenant: ork_common::types::TenantId,
    agent_id: &str,
) -> TaskId {
    let task_id = TaskId::new();
    let now = chrono::Utc::now();
    repo.create_task(&A2aTaskRow {
        id: task_id,
        context_id: ContextId::new(),
        tenant_id: tenant,
        agent_id: agent_id.to_string(),
        parent_task_id: None,
        workflow_run_id: None,
        state: TaskState::Working,
        metadata: serde_json::json!({"seeded": true}),
        created_at: now,
        updated_at: now,
        completed_at: None,
    })
    .await
    .unwrap();
    repo.append_message(&A2aMessageRow {
        id: ork_a2a::MessageId::new(),
        task_id,
        role: "user".into(),
        parts: serde_json::to_value(vec![Part::text("seeded")]).unwrap(),
        metadata: serde_json::json!({}),
        created_at: now,
    })
    .await
    .unwrap();
    task_id
}

#[tokio::test]
async fn tasks_get_returns_task_with_history() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_id = seed_task_with_message(&*t.task_repo, tenant, "planner").await;
    let app = a2a::protected_router(t.state);

    let params = TaskQueryParams {
        id: task_id,
        history_length: None,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-1"),
        "tasks/get",
        serde_json::to_value(&params).unwrap(),
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
    assert!(v["error"].is_null(), "expected success, got {v}");
    assert_eq!(v["result"]["id"], task_id.to_string());
    assert_eq!(v["result"]["status"]["state"], "working");
    assert_eq!(v["result"]["history"].as_array().unwrap().len(), 1);
    assert_eq!(v["result"]["history"][0]["role"], "user");
}

#[tokio::test]
async fn tasks_get_returns_task_not_found_for_unknown_id() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state);
    let params = TaskQueryParams {
        id: TaskId::new(),
        history_length: None,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-2"),
        "tasks/get",
        serde_json::to_value(&params).unwrap(),
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
    assert_eq!(v["error"]["code"], JsonRpcError::TASK_NOT_FOUND);
}

#[tokio::test]
async fn tasks_get_is_tenant_isolated() {
    let t = test_state().await;
    let other_tenant = ork_common::types::TenantId::new();
    let task_id = seed_task_with_message(&*t.task_repo, t.tenant_id, "planner").await;
    let app = a2a::protected_router(t.state);

    let params = TaskQueryParams {
        id: task_id,
        history_length: None,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-3"),
        "tasks/get",
        serde_json::to_value(&params).unwrap(),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(other_tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(
        v["error"]["code"],
        JsonRpcError::TASK_NOT_FOUND,
        "cross-tenant access MUST surface as TASK_NOT_FOUND, not leak the row"
    );
}

// =============================================================================
// Task 10: `tasks/cancel`
// =============================================================================

#[tokio::test]
async fn tasks_cancel_marks_state_canceled_and_returns_task() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_id = seed_task_with_message(&*t.task_repo, tenant, "planner").await;
    let app = a2a::protected_router(t.state);
    let task_repo = t.task_repo.clone();

    let params = TaskIdParams {
        id: task_id,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-1"),
        "tasks/cancel",
        serde_json::to_value(&params).unwrap(),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_with_cancel(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert!(v["error"].is_null(), "expected success, got {v}");
    assert_eq!(v["result"]["status"]["state"], "canceled");

    let after = task_repo.get_task(tenant, task_id).await.unwrap().unwrap();
    assert_eq!(after.state, TaskState::Canceled);
    assert!(after.completed_at.is_some());
}

#[tokio::test]
async fn tasks_cancel_unknown_task_returns_task_not_found() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state);
    let params = TaskIdParams {
        id: TaskId::new(),
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-2"),
        "tasks/cancel",
        serde_json::to_value(&params).unwrap(),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_with_cancel(tenant));
    let resp = app.oneshot(req).await.unwrap();
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], JsonRpcError::TASK_NOT_FOUND);
}

/// ADR-0021 §`Vocabulary`: `agent:<id>:cancel` is reserved for the
/// Operator/admin profile. End-user tokens get a 403 without it.
#[tokio::test]
async fn tasks_cancel_without_cancel_scope_is_forbidden() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_id = seed_task_with_message(&*t.task_repo, tenant, "planner").await;
    let app = a2a::protected_router(t.state);
    let params = TaskIdParams {
        id: task_id,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!("rpc-deny"),
        "tasks/cancel",
        serde_json::to_value(&params).unwrap(),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/planner")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    // End-user defaults from `auth_for` no longer include `agent:*:cancel`.
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn message_send_unknown_agent_returns_method_not_found() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state);
    let params = MessageSendParams {
        message: ork_a2a::Message::user(vec![Part::text("hi")]),
        configuration: None,
        metadata: None,
    };
    let body = jsonrpc_request(
        json!(7),
        "message/send",
        serde_json::to_value(&params).unwrap(),
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/a2a/agents/ghost")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["error"]["code"], JsonRpcError::METHOD_NOT_FOUND);
}
