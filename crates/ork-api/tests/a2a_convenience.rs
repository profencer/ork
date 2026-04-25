//! ADR-0008 §convenience endpoints — integration tests for `GET /a2a/agents`
//! (registry catalog) and `GET /a2a/tasks/{task_id}` (cross-agent task
//! lookup). Tenant isolation and bad-input handling are pinned here.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ork_a2a::{ContextId, MessageId, Part, TaskId, TaskState};
use ork_api::routes::a2a;
use ork_common::types::TenantId;
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use serde_json::json;
use tower::ServiceExt;

use crate::common::{auth_for, read_body, test_state, test_state_with_agents};

#[tokio::test]
async fn list_agents_returns_all_known_cards() {
    let t = test_state_with_agents(&["planner", "writer"]).await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state.clone());
    let mut req = Request::builder()
        .method("GET")
        .uri("/a2a/agents")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    let arr = v.as_array().expect("expected JSON array of cards");
    let names: Vec<String> = arr
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("planner")),
        "missing planner; got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("writer")),
        "missing writer; got: {names:?}"
    );
}

#[tokio::test]
async fn lookup_task_returns_task_for_owning_tenant() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let task_id = TaskId::new();
    let now = chrono::Utc::now();
    t.task_repo
        .create_task(&A2aTaskRow {
            id: task_id,
            context_id: ContextId::new(),
            tenant_id: tenant,
            agent_id: "planner".into(),
            parent_task_id: None,
            workflow_run_id: None,
            state: TaskState::Working,
            metadata: json!({"k": "v"}),
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
        .await
        .unwrap();
    t.task_repo
        .append_message(&A2aMessageRow {
            id: MessageId::new(),
            task_id,
            role: "user".into(),
            parts: serde_json::to_value(vec![Part::text("hello")]).unwrap(),
            metadata: json!({}),
            created_at: now,
        })
        .await
        .unwrap();

    let app = a2a::protected_router(t.state.clone());
    let mut req = Request::builder()
        .method("GET")
        .uri(format!("/a2a/tasks/{}", task_id))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_body(resp).await;
    assert_eq!(v["id"], task_id.to_string());
    assert_eq!(v["status"]["state"], "working");
    assert_eq!(v["history"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn lookup_task_returns_404_for_unknown_id() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state.clone());
    let mut req = Request::builder()
        .method("GET")
        .uri(format!("/a2a/tasks/{}", TaskId::new()))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lookup_task_is_tenant_isolated() {
    let t = test_state().await;
    let owner = t.tenant_id;
    let task_id = TaskId::new();
    let now = chrono::Utc::now();
    t.task_repo
        .create_task(&A2aTaskRow {
            id: task_id,
            context_id: ContextId::new(),
            tenant_id: owner,
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

    let intruder = TenantId::new();
    let app = a2a::protected_router(t.state.clone());
    let mut req = Request::builder()
        .method("GET")
        .uri(format!("/a2a/tasks/{}", task_id))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(auth_for(intruder));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lookup_task_with_bad_uuid_returns_400() {
    let t = test_state().await;
    let tenant = t.tenant_id;
    let app = a2a::protected_router(t.state.clone());
    let mut req = Request::builder()
        .method("GET")
        .uri("/a2a/tasks/not-a-uuid")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(auth_for(tenant));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
