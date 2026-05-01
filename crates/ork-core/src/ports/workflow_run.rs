//! Workflow run handle, events, and per-run dependencies (ADR [`0050`](../../../docs/adrs/0050-code-first-workflow-dsl.md)).

use std::sync::Arc;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_common::types::WorkflowRunId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::sync::{Mutex, RwLock};

use crate::agent_registry::AgentRegistry;
use crate::ports::repository::WorkflowRepository;
use crate::ports::workflow_snapshot::WorkflowSnapshotStore;

/// Events streamed for a workflow run (Studio / REST / OTel).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowEvent {
    StepStarted {
        step_id: String,
        input: Value,
    },
    StepFinished {
        step_id: String,
        output: Value,
    },
    StepSuspended {
        step_id: String,
        payload: Value,
    },
    StepFailed {
        step_id: String,
        error: String,
        retryable: bool,
    },
    StepRetrying {
        step_id: String,
        attempt: u32,
        after_ms: u64,
    },
    Heartbeat,
}

/// High-level polled state of a run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Running,
    Suspended {
        step_id: String,
        payload: Value,
        resume_schema: Value,
    },
    Completed {
        output: Value,
    },
    Failed {
        error: String,
    },
}

#[async_trait]
pub trait WorkflowRunDriver: Send + Sync {
    fn run_id(&self) -> WorkflowRunId;

    async fn poll(&self) -> Result<RunState, OrkError>;

    fn subscribe_events(&self) -> broadcast::Receiver<WorkflowEvent>;

    async fn resume(&self, step_id: &str, data: Value) -> Result<(), OrkError>;

    fn cancel(&self);

    async fn await_done(&self) -> Result<RunState, OrkError>;
}

/// Handle to an in-flight workflow run.
#[derive(Clone)]
pub struct WorkflowRunHandle {
    inner: Arc<dyn WorkflowRunDriver>,
}

impl WorkflowRunHandle {
    #[must_use]
    pub fn new(inner: Arc<dyn WorkflowRunDriver>) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn id(&self) -> WorkflowRunId {
        self.inner.run_id()
    }

    pub async fn poll(&self) -> Result<RunState, OrkError> {
        self.inner.poll().await
    }

    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.inner.subscribe_events()
    }

    pub async fn resume(&self, step_id: &str, data: Value) -> Result<(), OrkError> {
        self.inner.resume(step_id, data).await
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub async fn await_done(&self) -> Result<RunState, OrkError> {
        self.inner.await_done().await
    }
}

/// Dependencies injected by [`crate::ports::workflow_def::WorkflowDef::run`].
#[derive(Clone, Default)]
pub struct WorkflowRunDeps {
    pub snapshot_store: Option<Arc<dyn WorkflowSnapshotStore>>,
    pub agents: Option<Arc<AgentRegistry>>,
    pub workflow_repo: Option<Arc<dyn WorkflowRepository>>,
    pub tool_executor: Option<Arc<dyn crate::workflow::engine::ToolExecutor>>,
}

impl WorkflowRunDeps {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// from [`WorkflowDef::run`].
pub struct ImmediateWorkflowRunHandle {
    run_id: WorkflowRunId,
    state: Arc<RwLock<RunState>>,
    tx: broadcast::Sender<WorkflowEvent>,
    _done: Mutex<Option<()>>,
}

impl ImmediateWorkflowRunHandle {
    /// Build a handle that is already [`RunState::Completed`].
    #[must_use]
    pub fn completed(output: Value) -> WorkflowRunHandle {
        let run_id = WorkflowRunId::new();
        let (tx, _rx) = broadcast::channel(16);
        let inner = Arc::new(Self {
            run_id,
            state: Arc::new(RwLock::new(RunState::Completed { output })),
            tx,
            _done: Mutex::new(Some(())),
        });
        WorkflowRunHandle::new(inner)
    }
}

#[async_trait]
impl WorkflowRunDriver for ImmediateWorkflowRunHandle {
    fn run_id(&self) -> WorkflowRunId {
        self.run_id
    }

    async fn poll(&self) -> Result<RunState, OrkError> {
        Ok(self.state.read().await.clone())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.tx.subscribe()
    }

    async fn resume(&self, _step_id: &str, _data: Value) -> Result<(), OrkError> {
        Err(OrkError::Unsupported(
            "immediate workflow handle does not support resume".into(),
        ))
    }

    fn cancel(&self) {
        // no-op
    }

    async fn await_done(&self) -> Result<RunState, OrkError> {
        Ok(self.state.read().await.clone())
    }
}
