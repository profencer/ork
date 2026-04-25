//! ADR-0006 §`b) Async (await:false)` — fire-and-forget delegation publishes a
//! request to the configured [`DelegationPublisher`] and the parent step
//! continues immediately.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use ork_core::a2a::AgentId;
use ork_core::agent_registry::AgentRegistry;
use ork_core::models::workflow::{DelegationSpec, WorkflowRunStatus};
use ork_core::ports::delegation_publisher::DelegationPublisher;
use ork_core::workflow::NoopWorkflowRepository;
use ork_core::workflow::compiler;
use ork_core::workflow::engine::WorkflowEngine;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::common::{echo_agent_with_prefix, empty_run, one_step_def};

/// One captured `publish_request` call: target agent, child task id, payload bytes.
type RecordedRequest = (AgentId, TaskId, Vec<u8>);

/// In-process publisher that captures every call so tests can assert what would
/// have been written to Kafka without requiring a broker.
#[derive(Default, Clone)]
struct RecordingPublisher {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    cancels: Arc<Mutex<Vec<TaskId>>>,
}

#[async_trait]
impl DelegationPublisher for RecordingPublisher {
    async fn publish_request(
        &self,
        target_agent: &AgentId,
        task_id: TaskId,
        payload: &[u8],
    ) -> Result<(), OrkError> {
        self.requests
            .lock()
            .await
            .push((target_agent.clone(), task_id, payload.to_vec()));
        Ok(())
    }

    async fn publish_cancel(&self, task_id: TaskId) -> Result<(), OrkError> {
        self.cancels.lock().await.push(task_id);
        Ok(())
    }
}

#[tokio::test]
async fn fire_and_forget_publishes_and_continues() {
    let tenant_id = TenantId::new();
    let workflow_id = WorkflowId::new();

    let spec = DelegationSpec {
        agent: "researcher".into(),
        prompt_template: "look this up".into(),
        await_: false,
        push_url: None,
        child_workflow: None,
        timeout: None,
    };
    let def = one_step_def(workflow_id, tenant_id, "parent", Some(spec));
    let graph = compiler::compile(&def).expect("compile");

    // For fire-and-forget the broker routes the request, so the agent need not be
    // *invoked* locally — but it must still be `known` to the registry (either
    // local or a cached remote card) so an unknown id can be rejected up-front.
    // The simplest registry entry that satisfies `knows()` is a local stub.
    let registry = AgentRegistry::from_agents(vec![
        echo_agent_with_prefix("parent", "p:"),
        echo_agent_with_prefix("researcher", "r:"),
    ]);
    // Keep a typed handle so we can inspect captured calls; the engine gets the
    // erased trait object form.
    let recorder = RecordingPublisher::default();
    let publisher: Arc<dyn DelegationPublisher> = Arc::new(recorder.clone());
    let engine = WorkflowEngine::new(Arc::new(NoopWorkflowRepository), Arc::new(registry))
        .with_delegation(Some(publisher), None, CancellationToken::new());

    let mut run = empty_run(workflow_id, tenant_id);
    run.id = WorkflowRunId::new();

    engine
        .execute(tenant_id, &mut run, &graph)
        .await
        .expect("execute");

    assert_eq!(run.status, WorkflowRunStatus::Completed);

    let requests = recorder.requests.lock().await;
    assert_eq!(
        requests.len(),
        1,
        "exactly one fire-and-forget publish expected"
    );
    let (target, _task_id, payload) = &requests[0];
    assert_eq!(target.as_str(), "researcher");
    let payload_json: serde_json::Value =
        serde_json::from_slice(payload).expect("payload is JSON-RPC");
    assert_eq!(payload_json["method"], "message/send");
}
