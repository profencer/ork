//! `status_update` handler + in-memory A2A repo (ADR-0015).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::embeds::{EmbedContext, EmbedLimits, EmbedRegistry, resolve_early};
use ork_core::ports::a2a_task_repo::{A2aMessageRow, A2aTaskRepository, A2aTaskRow};
use uuid::Uuid;

struct MemRepo {
    task: A2aTaskRow,
    messages: Vec<A2aMessageRow>,
}

impl MemRepo {
    fn new(tenant: TenantId, task_id: TaskId) -> Self {
        let now = Utc::now();
        Self {
            task: A2aTaskRow {
                id: task_id,
                context_id: ork_a2a::ContextId::new(),
                tenant_id: tenant,
                agent_id: "a".to_string(),
                parent_task_id: None,
                workflow_run_id: None,
                state: ork_a2a::TaskState::Working,
                metadata: serde_json::json!({ "note": "x" }),
                created_at: now,
                updated_at: now,
                completed_at: None,
            },
            messages: vec![],
        }
    }
}

#[async_trait]
impl A2aTaskRepository for MemRepo {
    async fn create_task(&self, _row: &A2aTaskRow) -> Result<(), OrkError> {
        Ok(())
    }

    async fn update_state(
        &self,
        _tenant_id: TenantId,
        _id: TaskId,
        _state: ork_a2a::TaskState,
    ) -> Result<(), OrkError> {
        Ok(())
    }

    async fn get_task(&self, tenant: TenantId, id: TaskId) -> Result<Option<A2aTaskRow>, OrkError> {
        if tenant == self.task.tenant_id && id == self.task.id {
            return Ok(Some(self.task.clone()));
        }
        Ok(None)
    }

    async fn append_message(&self, _row: &A2aMessageRow) -> Result<(), OrkError> {
        Ok(())
    }

    async fn list_messages(
        &self,
        tenant: TenantId,
        task_id: TaskId,
        _history_length: Option<u32>,
    ) -> Result<Vec<A2aMessageRow>, OrkError> {
        if tenant == self.task.tenant_id && task_id == self.task.id {
            return Ok(self.messages.clone());
        }
        Ok(vec![])
    }

    async fn list_tasks_in_tenant(
        &self,
        _tenant: TenantId,
        _limit: u32,
    ) -> Result<Vec<A2aTaskRow>, OrkError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn summary_state_json() {
    let tid = TaskId::new();
    let tenant = TenantId(Uuid::new_v4());
    let repo: Arc<dyn A2aTaskRepository> = Arc::new(MemRepo::new(tenant, tid));
    let ctx = EmbedContext {
        tenant_id: tenant,
        task_id: Some(tid),
        a2a_repo: Some(repo),
        now: Utc::now(),
        variables: HashMap::new(),
        depth: 0,
    };
    let reg = EmbedRegistry::with_builtins();
    let lim = EmbedLimits::default();
    let id = tid.to_string();
    let s = resolve_early(
        &format!("«status_update:{id} | state» «status_update:{id} | json»"),
        &ctx,
        &reg,
        &lim,
    )
    .await
    .expect("ok");
    assert!(s.contains("working"), "state: {s}");
    assert!(
        s.contains("submitted") || s.contains("working") || s.contains(&id),
        "json: {s}"
    );
}
