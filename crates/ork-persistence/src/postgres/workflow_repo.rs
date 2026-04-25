use async_trait::async_trait;
use chrono::Utc;
use ork_a2a::TaskId;
use ork_common::error::OrkError;
use ork_common::types::{TenantId, WorkflowId, WorkflowRunId};
use sqlx::PgPool;

use ork_core::models::workflow::{
    StepResult, WorkflowDefinition, WorkflowRun, WorkflowRunStatus, WorkflowStep, WorkflowTrigger,
};
use ork_core::ports::repository::WorkflowRepository;

pub struct PgWorkflowRepository {
    pool: PgPool,
}

impl PgWorkflowRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WorkflowRepository for PgWorkflowRepository {
    async fn create_definition(
        &self,
        tenant_id: TenantId,
        def: &WorkflowDefinition,
    ) -> Result<WorkflowDefinition, OrkError> {
        let now = Utc::now();
        let trigger_json = serde_json::to_value(&def.trigger)
            .map_err(|e| OrkError::Internal(format!("serialize trigger: {e}")))?;
        let steps_json = serde_json::to_value(&def.steps)
            .map_err(|e| OrkError::Internal(format!("serialize steps: {e}")))?;

        sqlx::query(
            r#"
            INSERT INTO workflow_definitions (id, tenant_id, name, version, trigger, steps, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(def.id.0)
        .bind(tenant_id.0)
        .bind(&def.name)
        .bind(&def.version)
        .bind(&trigger_json)
        .bind(&steps_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("create workflow definition: {e}")))?;

        Ok(WorkflowDefinition {
            created_at: now,
            updated_at: now,
            ..def.clone()
        })
    }

    async fn get_definition(
        &self,
        tenant_id: TenantId,
        id: WorkflowId,
    ) -> Result<WorkflowDefinition, OrkError> {
        let row = sqlx::query_as::<_, WorkflowDefRow>(
            r#"
            SELECT id, tenant_id, name, version, trigger, steps, created_at, updated_at
            FROM workflow_definitions
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("get workflow definition: {e}")))?
        .ok_or_else(|| OrkError::NotFound(format!("workflow definition {id}")))?;

        row.into_definition()
    }

    async fn list_definitions(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<WorkflowDefinition>, OrkError> {
        let rows = sqlx::query_as::<_, WorkflowDefRow>(
            r#"
            SELECT id, tenant_id, name, version, trigger, steps, created_at, updated_at
            FROM workflow_definitions
            WHERE tenant_id = $1
            ORDER BY created_at
            "#,
        )
        .bind(tenant_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("list workflow definitions: {e}")))?;

        rows.into_iter().map(|r| r.into_definition()).collect()
    }

    async fn delete_definition(&self, tenant_id: TenantId, id: WorkflowId) -> Result<(), OrkError> {
        let result =
            sqlx::query("DELETE FROM workflow_definitions WHERE id = $1 AND tenant_id = $2")
                .bind(id.0)
                .bind(tenant_id.0)
                .execute(&self.pool)
                .await
                .map_err(|e| OrkError::Database(format!("delete workflow definition: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(OrkError::NotFound(format!("workflow definition {id}")));
        }
        Ok(())
    }

    async fn create_run(&self, run: &WorkflowRun) -> Result<WorkflowRun, OrkError> {
        let step_results_json = serde_json::to_value(&run.step_results)
            .map_err(|e| OrkError::Internal(format!("serialize step results: {e}")))?;

        sqlx::query(
            r#"
            INSERT INTO workflow_runs (
                id, workflow_id, tenant_id, status, input, output, step_results,
                started_at, completed_at, parent_run_id, parent_step_id, parent_task_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(run.id.0)
        .bind(run.workflow_id.0)
        .bind(run.tenant_id.0)
        .bind(run.status.to_string())
        .bind(&run.input)
        .bind(&run.output)
        .bind(&step_results_json)
        .bind(run.started_at)
        .bind(run.completed_at)
        .bind(run.parent_run_id.map(|id| id.0))
        .bind(&run.parent_step_id)
        .bind(run.parent_task_id.map(|id| id.0))
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("create workflow run: {e}")))?;

        Ok(run.clone())
    }

    async fn get_run(
        &self,
        tenant_id: TenantId,
        id: WorkflowRunId,
    ) -> Result<WorkflowRun, OrkError> {
        let row = sqlx::query_as::<_, WorkflowRunRow>(
            r#"
            SELECT id, workflow_id, tenant_id, status, input, output, step_results,
                   started_at, completed_at, parent_run_id, parent_step_id, parent_task_id
            FROM workflow_runs
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(id.0)
        .bind(tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("get workflow run: {e}")))?
        .ok_or_else(|| OrkError::NotFound(format!("workflow run {id}")))?;

        row.into_run()
    }

    async fn list_runs(
        &self,
        tenant_id: TenantId,
        workflow_id: Option<WorkflowId>,
    ) -> Result<Vec<WorkflowRun>, OrkError> {
        let rows = if let Some(wf_id) = workflow_id {
            sqlx::query_as::<_, WorkflowRunRow>(
                r#"
                SELECT id, workflow_id, tenant_id, status, input, output, step_results,
                       started_at, completed_at, parent_run_id, parent_step_id, parent_task_id
                FROM workflow_runs
                WHERE tenant_id = $1 AND workflow_id = $2
                ORDER BY started_at DESC
                "#,
            )
            .bind(tenant_id.0)
            .bind(wf_id.0)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, WorkflowRunRow>(
                r#"
                SELECT id, workflow_id, tenant_id, status, input, output, step_results,
                       started_at, completed_at, parent_run_id, parent_step_id, parent_task_id
                FROM workflow_runs
                WHERE tenant_id = $1
                ORDER BY started_at DESC
                "#,
            )
            .bind(tenant_id.0)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| OrkError::Database(format!("list workflow runs: {e}")))?;

        rows.into_iter().map(|r| r.into_run()).collect()
    }

    async fn update_run_status(
        &self,
        tenant_id: TenantId,
        id: WorkflowRunId,
        status: WorkflowRunStatus,
        output: Option<serde_json::Value>,
    ) -> Result<(), OrkError> {
        let now = if matches!(
            status,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::Rejected
        ) {
            Some(Utc::now())
        } else {
            None
        };

        sqlx::query(
            r#"
            UPDATE workflow_runs
            SET status = $1, output = COALESCE($2, output), completed_at = COALESCE($3, completed_at)
            WHERE id = $4 AND tenant_id = $5
            "#,
        )
        .bind(status.to_string())
        .bind(&output)
        .bind(now)
        .bind(id.0)
        .bind(tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("update run status: {e}")))?;

        Ok(())
    }

    async fn append_step_result(
        &self,
        tenant_id: TenantId,
        run_id: WorkflowRunId,
        step_result: &StepResult,
    ) -> Result<(), OrkError> {
        let step_json = serde_json::to_value(step_result)
            .map_err(|e| OrkError::Internal(format!("serialize step result: {e}")))?;

        sqlx::query(
            r#"
            UPDATE workflow_runs
            SET step_results = step_results || $1::jsonb
            WHERE id = $2 AND tenant_id = $3
            "#,
        )
        .bind(serde_json::json!([step_json]))
        .bind(run_id.0)
        .bind(tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| OrkError::Database(format!("append step result: {e}")))?;

        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct WorkflowDefRow {
    id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    name: String,
    version: String,
    trigger: serde_json::Value,
    steps: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl WorkflowDefRow {
    fn into_definition(self) -> Result<WorkflowDefinition, OrkError> {
        let trigger: WorkflowTrigger = serde_json::from_value(self.trigger)
            .map_err(|e| OrkError::Internal(format!("deserialize trigger: {e}")))?;
        let steps: Vec<WorkflowStep> = serde_json::from_value(self.steps)
            .map_err(|e| OrkError::Internal(format!("deserialize steps: {e}")))?;

        Ok(WorkflowDefinition {
            id: WorkflowId(self.id),
            tenant_id: TenantId(self.tenant_id),
            name: self.name,
            version: self.version,
            trigger,
            steps,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct WorkflowRunRow {
    id: uuid::Uuid,
    workflow_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    status: String,
    input: serde_json::Value,
    output: Option<serde_json::Value>,
    step_results: serde_json::Value,
    started_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    parent_run_id: Option<uuid::Uuid>,
    parent_step_id: Option<String>,
    parent_task_id: Option<uuid::Uuid>,
}

impl WorkflowRunRow {
    fn into_run(self) -> Result<WorkflowRun, OrkError> {
        let status = match self.status.as_str() {
            "pending" => WorkflowRunStatus::Pending,
            "running" => WorkflowRunStatus::Running,
            "input_required" => WorkflowRunStatus::InputRequired,
            "auth_required" => WorkflowRunStatus::AuthRequired,
            "completed" => WorkflowRunStatus::Completed,
            "failed" => WorkflowRunStatus::Failed,
            "cancelled" => WorkflowRunStatus::Cancelled,
            "rejected" => WorkflowRunStatus::Rejected,
            other => {
                return Err(OrkError::Internal(format!(
                    "unknown workflow run status: {other}"
                )));
            }
        };

        let step_results: Vec<StepResult> = serde_json::from_value(self.step_results)
            .map_err(|e| OrkError::Internal(format!("deserialize step results: {e}")))?;

        Ok(WorkflowRun {
            id: WorkflowRunId(self.id),
            workflow_id: WorkflowId(self.workflow_id),
            tenant_id: TenantId(self.tenant_id),
            status,
            input: self.input,
            output: self.output,
            step_results,
            started_at: self.started_at,
            completed_at: self.completed_at,
            parent_run_id: self.parent_run_id.map(WorkflowRunId),
            parent_step_id: self.parent_step_id,
            parent_task_id: self.parent_task_id.map(TaskId),
        })
    }
}
